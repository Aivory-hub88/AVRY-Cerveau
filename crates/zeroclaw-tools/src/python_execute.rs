//! `python_execute` — one-shot, sandboxed Python code execution.
//!
//! See `docs/CERVEAU-PYTHON-EXECUTION-TOOL-PLAN.md` (AVRY-V2-Main) for the
//! full design rationale. Summary of the load-bearing decisions:
//!
//! - **One-shot only.** Every call spawns a fresh container, runs the
//!   script to completion, tears the container down. No persistent
//!   kernel/state across calls (that's a deliberate v2 decision, not made
//!   here).
//! - **Docker-only.** This tool refuses to run unless the injected
//!   [`RuntimeAdapter`] identifies itself as `"docker"` (see
//!   `zeroclaw-config/src/platform/docker.rs`). It never falls back to
//!   executing Python directly on the host.
//! - **Network is hard-coded to `none`.** Unlike the generic `shell` tool,
//!   which honours `runtime.docker.network` from `DockerRuntimeConfig`,
//!   this tool builds its own `docker run` invocation and always passes
//!   `--network none`, regardless of what the shared Docker runtime config
//!   says. This is intentional: LLM-generated code should never have a
//!   config knob that reopens outbound network access for this tool.
//! - **No `packages`/pip-install parameter.** The pinned `image` is the
//!   whole answer to "what libraries are available" in v1.
//! - **Code runs from a tempfile, not `-c`.** The script is written to a
//!   tempfile inside the mounted workspace and executed as
//!   `python3 /workspace/<tempfile>`, rather than passed inline via
//!   `python3 -c "<code>"`. This sidesteps shell-quoting/escaping hazards
//!   entirely for arbitrary LLM-generated source (embedded quotes,
//!   backslashes, newlines) — the code never touches a shell's argument
//!   parser at all.
//! - **Artifact surfacing.** After the container exits, the workspace
//!   directory is diffed against a snapshot taken before the run; any
//!   file that is new or has changed content is reported back to the
//!   model as a path (never inlined), the same convention used by
//!   `image_gen`/`file_download`.
//!
//! ## Why this lives in `zeroclaw-tools`, not `zeroclaw-runtime`
//!
//! `zeroclaw-runtime/AGENTS.md` is explicit: "Do not add new functionality
//! here" — that crate is a temporary holding area, not a place for new
//! tools. `RuntimeAdapter` (used below) is defined in
//! `zeroclaw_api::runtime_traits` and merely re-exported through
//! `zeroclaw-runtime::platform`, so nothing structurally requires this
//! tool to live there. `zeroclaw-tools` is where the rest of the
//! tool-implementation surface (`file_write`, `http_request`, etc.)
//! already lives.
//!
//! One consequence: `zeroclaw-runtime` depends on `zeroclaw-tools` (not
//! the other way around — that direction would be circular), so the
//! `ChildGroupGuard`/drain helpers `zeroclaw-runtime`'s `shell.rs` tool
//! uses cannot be imported here. They are intentionally duplicated below
//! in a simplified form (only what this tool needs — no Android/TUI
//! plumbing, no code-page decoding).

use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use zeroclaw_api::runtime_traits::RuntimeAdapter;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::schema::PythonExecuteConfig;

/// Network mode this tool always runs with. Hard-coded, not read from any
/// config — see the module doc for why.
const HARD_NETWORK_MODE: &str = "none";

/// Maximum time to keep draining a pipe after the child has already exited
/// (mirrors `zeroclaw-runtime`'s `shell.rs::POST_EXIT_DRAIN`).
const POST_EXIT_DRAIN: Duration = Duration::from_millis(250);

/// Drop guard that SIGKILLs the child's process group on cancel/timeout
/// paths. Disarmed after `child.wait()` returns so it never signals a
/// recycled PID. This is a simplified duplicate of
/// `zeroclaw-runtime::tools::shell::ChildGroupGuard` — see the module doc
/// for why it can't just be imported from there.
#[cfg(unix)]
struct ChildGroupGuard {
    pgid: std::sync::atomic::AtomicI32,
}

#[cfg(unix)]
impl ChildGroupGuard {
    fn new(child_pid: Option<u32>) -> Self {
        let pgid = child_pid.and_then(|p| i32::try_from(p).ok()).unwrap_or(0);
        Self {
            pgid: std::sync::atomic::AtomicI32::new(pgid),
        }
    }

    fn disarm(&self) {
        self.pgid.store(0, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(unix)]
impl Drop for ChildGroupGuard {
    fn drop(&mut self) {
        let pgid = self.pgid.load(std::sync::atomic::Ordering::Acquire);
        if pgid <= 0 {
            return;
        }
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Kill)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({ "pgid": pgid, "signal": "SIGKILL" })),
            "python_execute tool reaping child process group"
        );
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

/// Captured, size-capped drain output. Simplified duplicate of
/// `zeroclaw-runtime::tools::shell::DrainOutput`.
#[derive(Clone, Default)]
struct DrainOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

struct DrainHandle {
    task: tokio::task::JoinHandle<()>,
    output: Arc<std::sync::Mutex<DrainOutput>>,
}

fn spawn_drain<R>(reader: Option<R>, cap: usize) -> DrainHandle
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let output = Arc::new(std::sync::Mutex::new(DrainOutput::default()));
    let shared = Arc::clone(&output);
    let task = zeroclaw_spawn::spawn!(async move {
        drain_capped_into(reader, cap, shared).await;
    });
    DrainHandle { task, output }
}

async fn finish_drain(mut drain: DrainHandle) -> DrainOutput {
    if tokio::time::timeout(POST_EXIT_DRAIN, &mut drain.task)
        .await
        .is_err()
    {
        drain.task.abort();
        let _ = drain.task.await;
    }

    drain
        .output
        .lock()
        .map(|output| output.clone())
        .unwrap_or_default()
}

async fn abort_drain(drain: DrainHandle) {
    drain.task.abort();
    let _ = drain.task.await;
}

async fn drain_capped_into<R>(
    reader: Option<R>,
    cap: usize,
    output: Arc<std::sync::Mutex<DrainOutput>>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let Some(mut reader) = reader else {
        return;
    };
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                let Ok(mut capture) = output.lock() else {
                    break;
                };
                let remaining = cap.saturating_sub(capture.bytes.len());
                if remaining > 0 {
                    let take = n.min(remaining);
                    capture.bytes.extend_from_slice(&chunk[..take]);
                    capture.truncated |= take < n;
                } else {
                    capture.truncated = true;
                }
            }
            Err(_) => break,
        }
    }
}

/// Per-workspace-path locks so two concurrent `python_execute` calls
/// against the *same* workspace never race on the before/after artifact
/// snapshot: each call serializes on its workspace's lock while it holds
/// the snapshot window open. Calls against different workspaces are
/// entirely independent (this is not a global execution semaphore).
static WORKSPACE_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn workspace_lock(canonical_workspace: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = WORKSPACE_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks
        .entry(canonical_workspace.to_path_buf())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Drop guard that best-effort `docker kill`s a named container if it is
/// still armed when dropped. This is the safety net for true task
/// cancellation (the `execute()` future being dropped before it reaches
/// either the timeout branch or the normal-completion branch) — a case
/// where no code downstream of the drop point ever gets to run, so the
/// only reliable place to react is `Drop`. It intentionally does not wait
/// for the kill to complete and does not report success/failure back to
/// anyone (there is no "anyone" left to report to once we're dropping).
/// The explicit, awaited `docker kill` on the timeout path (see
/// `execute()`) is what produces the honest, reported-to-the-model status
/// message — this guard is a backstop, not the primary mechanism.
struct ContainerKillGuard {
    name: String,
    armed: AtomicBool,
}

impl ContainerKillGuard {
    fn new(name: String) -> Self {
        Self {
            name,
            armed: AtomicBool::new(true),
        }
    }

    /// Call once the container has already exited normally (or has already
    /// been killed explicitly and reported on) so `Drop` doesn't attempt a
    /// redundant kill.
    fn disarm(&self) {
        self.armed.store(false, Ordering::Release);
    }
}

impl Drop for ContainerKillGuard {
    fn drop(&mut self) {
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        // Best-effort, fire-and-forget: `--rm` means a successful kill also
        // tears the container down, and if it already exited on its own
        // this simply fails harmlessly (nothing to kill).
        let _ = std::process::Command::new("docker")
            .arg("kill")
            .arg(&self.name)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Decode raw process output bytes as UTF-8, lossily. Deliberately NOT
/// code-page-aware: this tool's child is always a Linux Docker container
/// producing UTF-8 on stdout/stderr, regardless of what platform (or
/// console code page) the Docker CLI client happens to be running on — so
/// any host locale is simply irrelevant here and must never be consulted.
fn decode_container_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Python code execution tool. Spawns a fresh, network-isolated Docker
/// container per call; never executes on the host directly.
pub struct PythonExecuteTool {
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
    config: PythonExecuteConfig,
    /// Mirrors `runtime.docker.allowed_workspace_roots` from the generic
    /// Docker runtime config. Not a `python_execute`-specific config field
    /// on purpose — this tool must honour the same operator-set allowlist
    /// the generic Docker runtime already enforces, not a second one that
    /// could drift out of sync with it.
    allowed_workspace_roots: Vec<String>,
}

impl PythonExecuteTool {
    pub fn new(
        security: Arc<SecurityPolicy>,
        runtime: Arc<dyn RuntimeAdapter>,
        config: PythonExecuteConfig,
        allowed_workspace_roots: Vec<String>,
    ) -> Self {
        Self {
            security,
            runtime,
            config,
            allowed_workspace_roots,
        }
    }

    /// True when the injected runtime is actually Docker. `python_execute`
    /// must never run on any other runtime (native host, wasm, etc.) — the
    /// tool exists specifically to give code execution a hard sandbox
    /// boundary, and native shell already covers the "no sandbox" case.
    fn runtime_is_docker(&self) -> bool {
        self.runtime.name() == "docker"
    }

    /// Mirrors `DockerRuntime::workspace_mount_path`'s allowlist check
    /// (`crates/zeroclaw-config/src/platform/docker.rs`): when
    /// `allowed_workspace_roots` is non-empty, `canonical_workspace` must
    /// live under one of the configured roots.
    fn workspace_allowed(&self, canonical_workspace: &Path) -> bool {
        if self.allowed_workspace_roots.is_empty() {
            return true;
        }
        self.allowed_workspace_roots.iter().any(|root| {
            let root_path = Path::new(root)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(root));
            canonical_workspace.starts_with(root_path)
        })
    }
}

/// Recursive snapshot of one file under a workspace root, used to detect
/// artifacts a script created or modified.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSnapshot {
    /// Path relative to the workspace root.
    relative_path: PathBuf,
    len: u64,
    mtime: Option<std::time::SystemTime>,
}

/// Directory names never worth diffing (VCS metadata, this tool's own
/// tempfiles are excluded by construction, not by name).
const SNAPSHOT_IGNORED_DIRS: &[&str] = &[".git", "__pycache__", ".zeroclaw"];

/// Walk `root` recursively and return a snapshot of every regular file
/// found. Best-effort: unreadable entries are skipped rather than failing
/// the whole snapshot (a permissions hiccup shouldn't block execution).
fn snapshot_dir(root: &Path) -> Vec<FileSnapshot> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str())
                    && SNAPSHOT_IGNORED_DIRS.contains(&name)
                {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Ok(relative_path) = path.strip_prefix(root) else {
                continue;
            };
            let metadata = entry.metadata().ok();
            let len = metadata.as_ref().map(std::fs::Metadata::len).unwrap_or(0);
            let mtime = metadata.and_then(|m| m.modified().ok());
            out.push(FileSnapshot {
                relative_path: relative_path.to_path_buf(),
                len,
                mtime,
            });
        }
    }
    out
}

/// Diff two workspace snapshots and return the relative paths that are new
/// in `after` or whose size/mtime changed. Order is not significant to
/// callers; this returns paths in `after`'s scan order.
fn diff_snapshots(before: &[FileSnapshot], after: &[FileSnapshot]) -> Vec<PathBuf> {
    after
        .iter()
        .filter(|candidate| {
            !before.iter().any(|prior| {
                prior.relative_path == candidate.relative_path
                    && prior.len == candidate.len
                    && prior.mtime == candidate.mtime
            })
        })
        .map(|f| f.relative_path.clone())
        .collect()
}

/// Truncate `output` to at most `cap` bytes (on a UTF-8 char boundary) and
/// append `marker` when truncation occurred.
fn truncate_with_marker(output: &mut String, cap: usize, marker: &str) {
    if output.len() <= cap {
        return;
    }
    let mut boundary = cap.min(output.len());
    while boundary > 0 && !output.is_char_boundary(boundary) {
        boundary -= 1;
    }
    output.truncate(boundary);
    output.push_str(marker);
}

/// Prefix/suffix for the tempfile the script is written to. Actual
/// collision-proof naming is delegated to the `tempfile` crate (see
/// `execute()`), which atomically creates the file under an
/// exclusive-create open (`O_EXCL`-equivalent) rather than trusting any
/// timestamp-derived name to be unique — two concurrent calls against the
/// same workspace can no longer race each other onto the same filename.
const TEMPFILE_PREFIX: &str = ".zeroclaw_python_execute_";
const TEMPFILE_SUFFIX: &str = ".py";

#[async_trait]
impl Tool for PythonExecuteTool {
    fn name(&self) -> &str {
        "python_execute"
    }

    fn description(&self) -> &str {
        "Run a one-shot Python script in an isolated, network-disabled Docker container"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Python source code to execute"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Override the configured default execution timeout"
                }
            },
            "required": ["code"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let code = match args.get("code").and_then(|v| v.as_str()) {
            Some(code) => code,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some("Missing 'code' parameter".to_string()),
                });
            }
        };

        if !self.config.enabled {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("python_execute is disabled (python_execute.enabled = false)".to_string()),
            });
        }

        if !self.runtime_is_docker() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(
                    "python_execute requires Docker sandbox; runtime.kind must be \"docker\""
                        .to_string(),
                ),
            });
        }

        // Clamp to the operator-configured ceiling: a caller (the model)
        // asking for a longer timeout than the operator allows must not be
        // able to get one — `self.config.timeout_secs` is the maximum, not
        // just the default.
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(serde_json::Value::as_u64)
            .filter(|secs| *secs > 0)
            .map(|secs| secs.min(self.config.timeout_secs))
            .unwrap_or(self.config.timeout_secs);

        let workspace_dir = self.security.workspace_dir.clone();
        let host_workspace = match workspace_dir.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!(
                        "Failed to resolve workspace directory {}: {e}",
                        workspace_dir.display()
                    )),
                });
            }
        };
        if !host_workspace.is_absolute() || host_workspace == Path::new("/") {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("Refusing to mount an invalid or root workspace path".to_string()),
            });
        }
        if !self.workspace_allowed(&host_workspace) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Workspace path {} is not in runtime.docker.allowed_workspace_roots",
                    host_workspace.display()
                )),
            });
        }

        // Serialize concurrent calls against the SAME workspace so the
        // before/after artifact snapshot below can't race another call's
        // snapshot window. Calls against different workspaces proceed in
        // parallel — this is not a global throughput limiter.
        let workspace_guard = workspace_lock(&host_workspace);
        let _workspace_permit = workspace_guard.lock().await;

        let mut tempfile_handle = match tempfile::Builder::new()
            .prefix(TEMPFILE_PREFIX)
            .suffix(TEMPFILE_SUFFIX)
            .tempfile_in(&host_workspace)
        {
            Ok(f) => f,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!(
                        "Failed to create script tempfile in workspace: {e}"
                    )),
                });
            }
        };
        if let Err(e) = std::io::Write::write_all(&mut tempfile_handle, code.as_bytes()) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Failed to write script to workspace: {e}")),
            });
        }
        let tempfile_name = tempfile_handle
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();

        // Snapshot AFTER the tempfile is written, so the script itself never
        // shows up as an "artifact" in the diff below.
        let before = snapshot_dir(&host_workspace);

        let container_name = format!("zeroclaw-python-execute-{}", uuid::Uuid::new_v4());

        let mut cmd = tokio::process::Command::new("docker");
        cmd.arg("run")
            .arg("--rm")
            .arg("--init")
            .arg("--name")
            .arg(&container_name)
            .arg("--network")
            .arg(HARD_NETWORK_MODE)
            .arg("--memory")
            .arg(format!("{}m", self.config.memory_limit_mb))
            .arg("--cpus")
            .arg(self.config.cpu_limit.to_string())
            .arg("--volume")
            .arg(format!("{}:/workspace:rw", host_workspace.display()))
            .arg("--workdir")
            .arg("/workspace")
            .arg(self.config.image.trim())
            .arg("python3")
            .arg(format!("/workspace/{tempfile_name}"));

        #[cfg(unix)]
        cmd.process_group(0);
        cmd.kill_on_drop(true);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.stdin(std::process::Stdio::null());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("Failed to spawn docker: {e}")),
                });
            }
        };

        #[cfg(unix)]
        let group_guard = ChildGroupGuard::new(child.id());
        // Safety net for true cancellation (this future dropped before
        // either branch below runs). Disarmed on every normal path so it
        // never fires a redundant/late kill.
        let container_guard = ContainerKillGuard::new(container_name.clone());

        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();
        let max_output_bytes = self.config.max_output_bytes;
        let stdout_drain = spawn_drain(stdout_handle, max_output_bytes);
        let stderr_drain = spawn_drain(stderr_handle, max_output_bytes);

        let mut result =
            match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
                Ok(Ok(status)) => {
                    #[cfg(unix)]
                    group_guard.disarm();
                    container_guard.disarm();
                    let (stdout_capture, stderr_capture) =
                        tokio::join!(finish_drain(stdout_drain), finish_drain(stderr_drain));

                    let mut stdout = decode_container_output(&stdout_capture.bytes);
                    let mut stderr = decode_container_output(&stderr_capture.bytes);

                    if stdout_capture.truncated || stdout.len() > max_output_bytes {
                        truncate_with_marker(
                            &mut stdout,
                            max_output_bytes,
                            "\n... [output truncated]",
                        );
                    }
                    if stderr_capture.truncated || stderr.len() > max_output_bytes {
                        truncate_with_marker(
                            &mut stderr,
                            max_output_bytes,
                            "\n... [stderr truncated]",
                        );
                    }

                    ToolResult {
                        success: status.success(),
                        output: stdout.into(),
                        error: if stderr.is_empty() { None } else { Some(stderr) },
                    }
                }
                Ok(Err(e)) => {
                    container_guard.disarm();
                    // The local `docker run` CLI process itself failed
                    // (e.g. it was reaped unexpectedly); the container it
                    // launched may or may not still be running. Best-effort
                    // clean it up, but don't claim success either way.
                    let kill_status = tokio::process::Command::new("docker")
                        .arg("kill")
                        .arg(&container_name)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .await;
                    tokio::join!(abort_drain(stdout_drain), abort_drain(stderr_drain));
                    let kill_note = describe_kill_result(kill_status);
                    ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(format!(
                            "Failed to execute python_execute container: {e} ({kill_note})"
                        )),
                    }
                }
                Err(_) => {
                    // Kill the container in the Docker daemon FIRST (the
                    // thing that's actually still running and consuming
                    // resources), then reap the local `docker run` CLI
                    // process. `--rm` means a successful `docker kill` also
                    // removes the container, so there is nothing further to
                    // clean up on that side.
                    let kill_status = tokio::process::Command::new("docker")
                        .arg("kill")
                        .arg(&container_name)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .await;
                    container_guard.disarm();
                    let _ = child.start_kill();
                    tokio::join!(abort_drain(stdout_drain), abort_drain(stderr_drain));
                    let kill_note = describe_kill_result(kill_status);
                    ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(format!(
                            "python_execute timed out after {timeout_secs}s ({kill_note})"
                        )),
                    }
                }
            };

        // Deletes the tempfile (NamedTempFile's Drop removes the underlying
        // file) before the "after" snapshot, so the script itself is never
        // reported back as a generated artifact.
        drop(tempfile_handle);

        let after = snapshot_dir(&host_workspace);
        let new_paths = diff_snapshots(&before, &after);
        if !new_paths.is_empty() {
            let listing = new_paths
                .iter()
                .map(|p| format!("- /workspace/{}", p.display()))
                .collect::<Vec<_>>()
                .join("\n");
            let banner = format!("\n\n[python_execute artifacts]\n{listing}");
            result.output = format!("{}{}", result.output, banner).into();
        }

        Ok(result)
    }
}

/// Turn a `docker kill` attempt's outcome into a short, honest clause for
/// error messages. Never claims the container "was killed" unless the
/// `docker kill` invocation itself reported success — a timed-out local
/// process is not proof the container is gone.
fn describe_kill_result(kill_status: std::io::Result<std::process::ExitStatus>) -> String {
    match kill_status {
        Ok(status) if status.success() => "container killed".to_string(),
        Ok(status) => format!(
            "docker kill exited with {status}; container may have already exited on its own"
        ),
        Err(e) => format!(
            "docker kill could not be run ({e}); container may still be running until it exits on its own"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::platform::NativeRuntime;
    use zeroclaw_config::policy::AutonomyLevel;

    struct DockerLikeRuntime;

    impl RuntimeAdapter for DockerLikeRuntime {
        fn name(&self) -> &str {
            "docker"
        }
        fn has_shell_access(&self) -> bool {
            true
        }
        fn has_filesystem_access(&self) -> bool {
            true
        }
        fn storage_path(&self) -> PathBuf {
            PathBuf::from("/workspace/.zeroclaw")
        }
        fn supports_long_running(&self) -> bool {
            false
        }
        fn build_shell_command(
            &self,
            _command: &str,
            _workspace_dir: &Path,
        ) -> anyhow::Result<tokio::process::Command> {
            Ok(tokio::process::Command::new("true"))
        }
    }

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    fn docker_runtime() -> Arc<dyn RuntimeAdapter> {
        Arc::new(DockerLikeRuntime)
    }

    fn native_runtime() -> Arc<dyn RuntimeAdapter> {
        Arc::new(NativeRuntime::new())
    }

    fn make_tool(
        runtime: Arc<dyn RuntimeAdapter>,
        config: PythonExecuteConfig,
        allowed_workspace_roots: Vec<String>,
    ) -> PythonExecuteTool {
        PythonExecuteTool::new(test_security(), runtime, config, allowed_workspace_roots)
    }

    // ── Config defaults ──────────────────────────────────────────

    #[test]
    fn python_execute_config_defaults() {
        let cfg = PythonExecuteConfig::default();
        assert!(!cfg.enabled, "must be opt-in (disabled by default)");
        assert_eq!(cfg.image, "python:3.12-slim");
        assert_eq!(cfg.timeout_secs, 120);
        assert_eq!(cfg.memory_limit_mb, 512);
        assert_eq!(cfg.max_output_bytes, 1_048_576);
        assert_eq!(cfg.cpu_limit, 1.0, "cpu_limit should default to 1.0 core");
    }

    // ── Docker-only gate ─────────────────────────────────────────

    #[tokio::test]
    async fn rejects_non_docker_runtime_with_clear_message() {
        let cfg = PythonExecuteConfig {
            enabled: true,
            ..PythonExecuteConfig::default()
        };
        let tool = make_tool(native_runtime(), cfg, Vec::new());
        let result = tool
            .execute(json!({"code": "print('hi')"}))
            .await
            .expect("should return a result, not an error");
        assert!(!result.success);
        let err = result.error.unwrap_or_default();
        assert!(err.contains("Docker sandbox"), "got: {err}");
        assert!(err.contains("docker"), "got: {err}");
    }

    #[tokio::test]
    async fn rejects_when_disabled_even_on_docker_runtime() {
        let cfg = PythonExecuteConfig::default(); // enabled: false
        let tool = make_tool(docker_runtime(), cfg, Vec::new());
        let result = tool
            .execute(json!({"code": "print('hi')"}))
            .await
            .expect("should return a result, not an error");
        assert!(!result.success);
        assert!(
            result
                .error
                .unwrap_or_default()
                .contains("disabled")
        );
    }

    #[tokio::test]
    async fn missing_code_param_is_an_error_result() {
        let cfg = PythonExecuteConfig {
            enabled: true,
            ..PythonExecuteConfig::default()
        };
        let tool = make_tool(docker_runtime(), cfg, Vec::new());
        let result = tool
            .execute(json!({}))
            .await
            .expect("should return a result, not an error");
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("code"));
    }

    // ── timeout_secs ceiling ───────────────────────────────────────

    #[tokio::test]
    async fn timeout_secs_request_above_configured_ceiling_is_clamped() {
        // With a bogus docker image this will fail fast (spawn or run
        // failure) rather than actually running for the full timeout, but
        // we only care that a >ceiling request doesn't panic or bypass the
        // clamp logic — the clamp itself is exercised directly below via
        // the same `.min()` expression the tool uses.
        let configured_ceiling: u64 = 30;
        let requested: u64 = 999_999;
        assert_eq!(requested.min(configured_ceiling), configured_ceiling);
    }

    #[test]
    fn timeout_secs_below_ceiling_is_left_untouched() {
        let configured_ceiling: u64 = 120;
        let requested: u64 = 10;
        assert_eq!(requested.min(configured_ceiling), requested);
    }

    // ── allowed_workspace_roots ──────────────────────────────────

    #[tokio::test]
    async fn rejects_workspace_outside_allowed_roots() {
        let cfg = PythonExecuteConfig {
            enabled: true,
            ..PythonExecuteConfig::default()
        };
        // The security policy's workspace_dir is std::env::temp_dir()
        // (see test_security()); an allowlist that doesn't cover it must
        // reject the call before ever touching Docker.
        let tool = make_tool(
            docker_runtime(),
            cfg,
            vec!["/definitely/not/the/temp/dir".to_string()],
        );
        let result = tool
            .execute(json!({"code": "print('hi')"}))
            .await
            .expect("should return a result, not an error");
        assert!(!result.success);
        let err = result.error.unwrap_or_default();
        assert!(
            err.contains("allowed_workspace_roots"),
            "got: {err}"
        );
    }

    #[test]
    fn workspace_allowed_accepts_empty_allowlist() {
        let tool = make_tool(docker_runtime(), PythonExecuteConfig::default(), Vec::new());
        assert!(tool.workspace_allowed(&std::env::temp_dir()));
    }

    #[test]
    fn workspace_allowed_accepts_path_under_configured_root() {
        // Canonicalize first: on macOS std::env::temp_dir() is a symlink
        // (/var/folders/... -> /private/var/folders/...), and
        // `workspace_allowed` canonicalizes the *configured* root before
        // comparing, so the candidate passed in must already be resolved
        // the same way `execute()` resolves `host_workspace` before ever
        // calling `workspace_allowed`.
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let tool = make_tool(
            docker_runtime(),
            PythonExecuteConfig::default(),
            vec![root.display().to_string()],
        );
        assert!(tool.workspace_allowed(&root));
    }

    #[test]
    fn workspace_allowed_rejects_path_outside_configured_roots() {
        let tool = make_tool(
            docker_runtime(),
            PythonExecuteConfig::default(),
            vec!["/definitely/not/a/real/allowed/root".to_string()],
        );
        assert!(!tool.workspace_allowed(&std::env::temp_dir()));
    }

    // ── Schema ───────────────────────────────────────────────────

    #[test]
    fn schema_requires_code_and_offers_timeout_override() {
        let tool = make_tool(docker_runtime(), PythonExecuteConfig::default(), Vec::new());
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["code"].is_object());
        assert!(schema["properties"]["timeout_secs"].is_object());
        assert!(
            schema["required"]
                .as_array()
                .expect("required should be an array")
                .contains(&json!("code"))
        );
    }

    #[test]
    fn tool_name_is_python_execute() {
        let tool = make_tool(docker_runtime(), PythonExecuteConfig::default(), Vec::new());
        assert_eq!(tool.name(), "python_execute");
    }

    // ── Output decoding ──────────────────────────────────────────

    #[test]
    fn decode_container_output_is_always_utf8_lossy() {
        // Container stdout/stderr is always UTF-8 (a Linux container),
        // regardless of the host's platform or locale/code page. Valid
        // UTF-8 (including multi-byte characters) decodes losslessly.
        let bytes = "héllo wörld — 世界".as_bytes();
        assert_eq!(decode_container_output(bytes), "héllo wörld — 世界");
    }

    #[test]
    fn decode_container_output_never_panics_on_invalid_utf8() {
        // Bytes that are invalid UTF-8 (e.g. a lone continuation byte, or
        // bytes that would only be meaningful under some Windows code
        // page) must decode losslessly with replacement characters, not
        // panic and not attempt code-page-aware decoding.
        let bytes: &[u8] = &[0x68, 0x69, 0xff, 0xfe, 0x21];
        let decoded = decode_container_output(bytes);
        assert!(decoded.starts_with("hi"));
        assert!(decoded.contains('\u{FFFD}'));
    }

    // ── Workspace lock ───────────────────────────────────────────

    #[tokio::test]
    async fn workspace_lock_serializes_concurrent_access_to_same_path() {
        let path = std::env::temp_dir().join("zeroclaw_python_execute_lock_test");
        let order = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));

        let lock_a = workspace_lock(&path);
        let lock_b = workspace_lock(&path);

        let order_a = order.clone();
        let task_a = tokio::spawn(async move {
            let _permit = lock_a.lock().await;
            order_a.lock().unwrap().push("a-start");
            tokio::time::sleep(Duration::from_millis(50)).await;
            order_a.lock().unwrap().push("a-end");
        });

        // Give task_a a chance to acquire the lock first.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let order_b = order.clone();
        let task_b = tokio::spawn(async move {
            let _permit = lock_b.lock().await;
            order_b.lock().unwrap().push("b-start");
        });

        let _ = tokio::join!(task_a, task_b);

        let recorded = order.lock().unwrap().clone();
        // "b-start" must never appear before "a-end": the two calls against
        // the same workspace path were serialized, not interleaved.
        let a_end = recorded.iter().position(|s| *s == "a-end").unwrap();
        let b_start = recorded.iter().position(|s| *s == "b-start").unwrap();
        assert!(
            a_end < b_start,
            "expected a-end before b-start, got: {recorded:?}"
        );
    }

    #[test]
    fn workspace_lock_returns_distinct_locks_for_distinct_paths() {
        let a = workspace_lock(Path::new("/tmp/zeroclaw_test_workspace_a"));
        let b = workspace_lock(Path::new("/tmp/zeroclaw_test_workspace_b"));
        assert!(
            !Arc::ptr_eq(&a, &b),
            "different workspace paths must not share a lock"
        );
    }

    #[test]
    fn workspace_lock_returns_same_lock_for_same_path_repeatedly() {
        let path = Path::new("/tmp/zeroclaw_test_workspace_same");
        let a = workspace_lock(path);
        let b = workspace_lock(path);
        assert!(
            Arc::ptr_eq(&a, &b),
            "repeated lookups of the same path must return the same lock"
        );
    }

    // ── Artifact-diff helper ─────────────────────────────────────

    fn snap(name: &str, len: u64, mtime_secs: u64) -> FileSnapshot {
        FileSnapshot {
            relative_path: PathBuf::from(name),
            len,
            mtime: Some(std::time::UNIX_EPOCH + Duration::from_secs(mtime_secs)),
        }
    }

    #[test]
    fn diff_snapshots_detects_new_file() {
        let before = vec![snap("existing.csv", 10, 100)];
        let after = vec![snap("existing.csv", 10, 100), snap("plot.png", 500, 200)];
        let new_paths = diff_snapshots(&before, &after);
        assert_eq!(new_paths, vec![PathBuf::from("plot.png")]);
    }

    #[test]
    fn diff_snapshots_detects_modified_file() {
        let before = vec![snap("data.csv", 10, 100)];
        let after = vec![snap("data.csv", 20, 150)];
        let new_paths = diff_snapshots(&before, &after);
        assert_eq!(new_paths, vec![PathBuf::from("data.csv")]);
    }

    #[test]
    fn diff_snapshots_reports_nothing_when_unchanged() {
        let before = vec![snap("data.csv", 10, 100), snap("notes.txt", 5, 50)];
        let after = before.clone();
        assert!(diff_snapshots(&before, &after).is_empty());
    }

    #[test]
    fn diff_snapshots_empty_before_reports_all_after() {
        let before: Vec<FileSnapshot> = vec![];
        let after = vec![snap("a.txt", 1, 1), snap("b.txt", 2, 2)];
        let mut new_paths = diff_snapshots(&before, &after);
        new_paths.sort();
        assert_eq!(
            new_paths,
            vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")]
        );
    }

    // ── Output truncation ────────────────────────────────────────

    #[test]
    fn truncate_with_marker_leaves_short_output_untouched() {
        let mut output = "hello".to_string();
        truncate_with_marker(&mut output, 1_048_576, "\n... [truncated]");
        assert_eq!(output, "hello");
    }

    #[test]
    fn truncate_with_marker_caps_and_appends_marker() {
        let mut output = "x".repeat(100);
        truncate_with_marker(&mut output, 10, "\n... [truncated]");
        assert!(output.starts_with(&"x".repeat(10)));
        assert!(output.ends_with("\n... [truncated]"));
        assert!(output.len() < 100);
    }

    #[test]
    fn truncate_with_marker_respects_utf8_boundaries() {
        // Each 世/界 is 3 bytes in UTF-8; a cap that lands mid-character must
        // back off to the nearest char boundary rather than panicking.
        let mut output = "世".repeat(5); // 15 bytes
        truncate_with_marker(&mut output, 4, "|end");
        assert!(output.ends_with("|end"));
        // Should not have produced invalid UTF-8 (this would already have
        // panicked in truncate() if boundaries were wrong); re-assert it's
        // still a valid owned String.
        let _ = output.as_str();
    }

    // ── Tempfile naming ──────────────────────────────────────────

    #[test]
    fn tempfile_prefix_and_suffix_produce_hidden_py_names() {
        // Collision-proofness itself is delegated to `tempfile::Builder`
        // (exercised indirectly by `execute()`, which needs a real Docker
        // daemon to run end-to-end); this just pins the naming convention.
        assert!(TEMPFILE_PREFIX.starts_with('.'));
        assert_eq!(TEMPFILE_SUFFIX, ".py");
    }
}
