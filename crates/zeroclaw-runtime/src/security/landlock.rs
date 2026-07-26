//! Landlock sandbox (Linux kernel 5.13+ LSM)
//! Landlock provides unprivileged sandboxing through the Linux kernel.
//! This module uses the pure-Rust `landlock` crate for filesystem access control.

#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
use landlock::{
    AccessFs, Errno, PathBeneath, PathFd, PathFdError, Ruleset, RulesetAttr, RulesetCreatedAttr,
};
#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
use std::path::Path;
#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
use std::process::Command;

use crate::security::traits::Sandbox;

/// Landlock sandbox backend for Linux
#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
#[derive(Debug)]
pub struct LandlockSandbox {
    workspace_dir: Option<std::path::PathBuf>,
}

#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
impl LandlockSandbox {
    /// Create a new Landlock sandbox with the given workspace directory
    pub fn new() -> std::io::Result<Self> {
        Self::with_workspace(None)
    }

    /// Create a Landlock sandbox with a specific workspace directory
    pub fn with_workspace(workspace_dir: Option<std::path::PathBuf>) -> std::io::Result<Self> {
        // Test if Landlock is available by trying to create a minimal ruleset
        let test_ruleset = Ruleset::default()
            .handle_access(AccessFs::ReadFile | AccessFs::WriteFile)
            .and_then(|ruleset| ruleset.create());

        match test_ruleset {
            Ok(_) => Ok(Self { workspace_dir }),
            Err(e) => {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "Landlock not available"
                );
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "Landlock not available",
                ))
            }
        }
    }

    /// Probe if Landlock is available (for auto-detection)
    pub fn probe() -> std::io::Result<Self> {
        Self::new()
    }

    /// Apply Landlock restrictions to the current process. Irreversible and
    /// permanent for the life of this process (Landlock is monotonic — only
    /// ever adds restrictions) — call this only from a short-lived,
    /// dedicated process meant to immediately `execve` into its real target
    /// (see the CLI's hidden `internal-landlock-exec` subcommand), never
    /// from the long-lived daemon itself.
    ///
    /// `extra_readable_root`, when given, is granted the same trust tier as
    /// `/usr`/`/bin` below (read + readdir, no write): a directory the
    /// *operator* installed and configured the exec target from — e.g. an
    /// MCP tool's own npm install under `~/.zeroclaw-cerveau/mcp-tools/`,
    /// which lives outside `/usr`/`/bin` and the tenant workspace but is
    /// exactly as trusted as system binaries (not tenant-controlled). Without
    /// this, `execve`d targets installed outside those default paths (and
    /// anything they read at startup, e.g. a Node module-resolution walk
    /// through `node_modules/`) fail closed with `EACCES` even for their
    /// *legitimate* owning tenant — this isn't a workspace-isolation gap,
    /// it's the tool's own code being unreadable.
    pub fn apply_restrictions(&self, extra_readable_root: Option<&Path>) -> std::io::Result<()> {
        let mut ruleset = Ruleset::default()
            .handle_access(
                AccessFs::Execute
                    | AccessFs::WriteFile
                    | AccessFs::ReadFile
                    | AccessFs::Truncate
                    | AccessFs::ReadDir
                    | AccessFs::RemoveDir
                    | AccessFs::RemoveFile
                    | AccessFs::MakeChar
                    | AccessFs::MakeDir
                    | AccessFs::MakeReg
                    | AccessFs::MakeSock
                    | AccessFs::MakeFifo
                    | AccessFs::MakeBlock
                    | AccessFs::MakeSym,
            )
            .and_then(|ruleset| ruleset.create())
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        // Allow workspace directory (read/write/execute).
        // If a workspace was supplied but doesn't exist, fail closed rather than
        // silently applying restrictions without a rule for it.
        if let Some(ref workspace) = self.workspace_dir {
            let workspace_fd =
                PathFd::new(workspace).map_err(|e| std::io::Error::other(e.to_string()))?;
            ruleset = ruleset
                .add_rule(PathBeneath::new(
                    workspace_fd,
                    AccessFs::Execute
                        | AccessFs::WriteFile
                        | AccessFs::ReadFile
                        | AccessFs::Truncate
                        | AccessFs::ReadDir
                        | AccessFs::RemoveDir
                        | AccessFs::RemoveFile
                        | AccessFs::MakeDir
                        | AccessFs::MakeReg
                        | AccessFs::MakeSock
                        | AccessFs::MakeFifo
                        | AccessFs::MakeSym,
                ))
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        // Allow paths for general operations.
        // `required = true`  -> fail closed if the path is missing (baseline devices, system roots).
        // `required = false` -> skip on NotFound (distro-optional loader/layout paths).
        for (allow_path, perm, required) in [
            // /tmp: general temp directory for child processes (pipes, sockets, temp files).
            // Execute is intentionally omitted to prevent running untrusted binaries from /tmp.
            (
                "/tmp",
                AccessFs::Truncate | AccessFs::WriteFile | AccessFs::ReadFile,
                true,
            ),
            // Linux dynamic linker (ld-linux-yourarch.so.version) which designed to run on FHS 3.0
            // system will read the following file/directories to retrieve dynamic linker config.
            // These are optional: minimal systems may not have all of them.
            ("/etc/ld.so.cache", AccessFs::ReadFile.into(), false),
            ("/etc/ld.so.conf", AccessFs::ReadFile.into(), false),
            ("/etc/ld.so.preload", AccessFs::ReadFile.into(), false),
            (
                "/etc/ld.so.conf.d",
                AccessFs::ReadFile | AccessFs::ReadDir,
                false,
            ),
            // In FHS 3.0 systems, system binaries will live in the following directories:
            // /usr/bin, /usr/lib, /usr/lib64, /bin, /lib, /lib64.
            // Execute: needed to run binaries (execve) and for the dynamic linker's
            // access(X_OK) checks on shared libraries.
            //
            // /usr is optional: Non-FHS distros may not have it.
            (
                "/usr",
                AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir,
                false,
            ),
            (
                "/bin",
                AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir,
                true,
            ),
            // /lib and /lib64 are distro-optional: some systems have one, some both.
            (
                "/lib",
                AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir,
                false,
            ),
            (
                "/lib64",
                AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir,
                false,
            ),
            // some variant of sh requires access to /dev/null
            ("/dev/null", AccessFs::WriteFile | AccessFs::ReadFile, true),
        ] {
            match PathFd::new(Path::new(allow_path)) {
                Ok(path_fd) => {
                    ruleset = ruleset
                        .add_rule(PathBeneath::new(path_fd, perm))
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                }
                Err(PathFdError::OpenCall { source, .. }) => {
                    if source.kind() == std::io::ErrorKind::NotFound {
                        if required {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                format!(
                                    "Required path {allow_path} not found for Landlock sandbox"
                                ),
                            ));
                        }
                        ::zeroclaw_log::record!(
                            DEBUG,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            ),
                            format!(
                                "Failed to create PathFd for a nonexistent path {}.",
                                allow_path,
                            ),
                        );
                    } else {
                        Err(std::io::Error::other(source.to_string()))?;
                    }
                }
                Err(e) => {
                    Err(std::io::Error::other(e.to_string()))?;
                }
            }
        }

        // Allow the exec target's own (operator-installed, non-tenant)
        // install root, if the caller resolved one — same trust tier as
        // the /usr and /bin entries in the table above.
        if let Some(extra_root) = extra_readable_root
            && extra_root.exists()
        {
            let extra_fd =
                PathFd::new(extra_root).map_err(|e| std::io::Error::other(e.to_string()))?;
            ruleset = ruleset
                .add_rule(PathBeneath::new(
                    extra_fd,
                    AccessFs::ReadFile | AccessFs::ReadDir,
                ))
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        // This method is only ever called from the short-lived,
        // dedicated `internal-landlock-exec` re-exec target (see
        // `wrap_command` below), never from the long-lived daemon, so
        // enforcing immediately here (rather than deferring to a
        // fork()+pre_exec step) is safe.
        match ruleset.restrict_self() {
            Ok(_) => {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "Landlock restrictions applied successfully"
                );
                Ok(())
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "Failed to apply Landlock restrictions"
                );
                Err(std::io::Error::from_raw_os_error(*Errno::from(e)))
            }
        }
    }
}

#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
impl Sandbox for LandlockSandbox {
    /// Rewrites `cmd` to invoke this same `zeroclaw` binary's hidden
    /// `internal-landlock-exec` subcommand, which applies the Landlock
    /// ruleset to *itself* (a fresh, ordinary process — safe, no
    /// async-signal-safety concerns) and then `execve`s into the real
    /// target. Mirrors `FirejailSandbox::wrap_command`'s shape (rewrite the
    /// command to prepend a wrapper, rather than restricting the calling
    /// process — the earlier version of this method did the latter, which
    /// is wrong for a long-lived multi-tenant daemon: Landlock only ever
    /// adds restrictions, so restricting the daemon itself would
    /// permanently wall it off after the very first call).
    ///
    /// Requires `self.workspace_dir` — with no workspace to scope to, there
    /// is nothing safe to restrict *to*, so this fails closed rather than
    /// executing unsandboxed or restricting to an arbitrary default.
    ///
    /// Unlike `FirejailSandbox`, this explicitly re-applies `cmd`'s
    /// original env vars onto the replacement command: `*cmd = new_cmd`
    /// (below) would otherwise silently drop any `[[mcp.servers]].env`
    /// entries a caller had already set — harmless for `shell.rs`'s call
    /// site (it unconditionally rebuilds env right after wrapping) but not
    /// for a caller with no such follow-up.
    fn wrap_command(&self, cmd: &mut std::process::Command) -> std::io::Result<()> {
        let Some(workspace) = &self.workspace_dir else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "LandlockSandbox::wrap_command requires a workspace_dir to scope to \
                 (LandlockSandbox::new()/None has nothing safe to restrict to)",
            ));
        };
        let current_exe = std::env::current_exe()?;
        let program = cmd.get_program().to_owned();
        let args: Vec<_> = cmd.get_args().map(std::ffi::OsStr::to_owned).collect();
        // `get_envs()` yields `(key, None)` for a var this `cmd` explicitly
        // *removed* (e.g. a prior `env_remove`), distinct from a var it
        // simply never touched — carry that removal forward too, not just
        // the additions, so the replacement command's env is a faithful
        // copy of what the caller had actually built.
        let envs: Vec<_> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(std::ffi::OsStr::to_owned)))
            .collect();
        // Same "don't drop caller-set state" concern as env vars: the exec'd
        // target inherits whatever CWD the `internal-landlock-exec` wrapper
        // process itself started with (exec() doesn't change CWD), so it
        // must carry this forward too — a caller that set `current_dir` to
        // scope a server's CWD-relative I/O (e.g. an MCP stdio server with
        // no native workspace flag) would otherwise silently lose that scoping.
        let current_dir = cmd.get_current_dir().map(std::path::Path::to_path_buf);

        let mut wrapped = Command::new(current_exe);
        wrapped.arg("internal-landlock-exec");
        wrapped.arg("--workspace");
        wrapped.arg(workspace);
        wrapped.arg("--");
        wrapped.arg(program);
        wrapped.args(args);
        for (key, value) in envs {
            match value {
                Some(v) => {
                    wrapped.env(key, v);
                }
                None => {
                    wrapped.env_remove(key);
                }
            }
        }
        if let Some(dir) = current_dir {
            wrapped.current_dir(dir);
        }

        *cmd = wrapped;
        Ok(())
    }

    fn is_available(&self) -> bool {
        // Try to create a minimal ruleset to verify availability
        Ruleset::default()
            .handle_access(AccessFs::ReadFile)
            .and_then(|ruleset| ruleset.create())
            .is_ok()
    }

    fn name(&self) -> &str {
        "landlock"
    }

    fn description(&self) -> &str {
        "Linux kernel LSM sandboxing (filesystem access control)"
    }
}

// Stub implementations for non-Linux or when feature is disabled
#[cfg(not(all(feature = "sandbox-landlock", target_os = "linux")))]
#[derive(Debug)]
pub struct LandlockSandbox;

#[cfg(not(all(feature = "sandbox-landlock", target_os = "linux")))]
impl LandlockSandbox {
    pub fn new() -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Landlock is only supported on Linux with the sandbox-landlock feature",
        ))
    }

    pub fn with_workspace(_workspace_dir: Option<std::path::PathBuf>) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Landlock is only supported on Linux",
        ))
    }

    pub fn probe() -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Landlock is only supported on Linux",
        ))
    }
}

#[cfg(not(all(feature = "sandbox-landlock", target_os = "linux")))]
impl Sandbox for LandlockSandbox {
    fn wrap_command(&self, _cmd: &mut std::process::Command) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Landlock is only supported on Linux",
        ))
    }

    fn is_available(&self) -> bool {
        false
    }

    fn name(&self) -> &str {
        "landlock"
    }

    fn description(&self) -> &str {
        "Linux kernel LSM sandboxing (not available on this platform)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
    #[test]
    fn landlock_sandbox_name() {
        if let Ok(sandbox) = LandlockSandbox::new() {
            assert_eq!(sandbox.name(), "landlock");
        }
    }

    #[cfg(not(all(feature = "sandbox-landlock", target_os = "linux")))]
    #[test]
    fn landlock_not_available_on_non_linux() {
        assert!(!LandlockSandbox.is_available());
        assert_eq!(LandlockSandbox.name(), "landlock");
    }

    #[test]
    fn landlock_with_none_workspace() {
        // Should work even without a workspace directory
        let result = LandlockSandbox::with_workspace(None);
        // On Linux with sandbox-landlock feature, this must succeed.
        // On other platforms or without the feature, failure is acceptable.
        if cfg!(all(feature = "sandbox-landlock", target_os = "linux")) {
            let sandbox = result.expect("landlock should succeed on linux with feature enabled");
            assert!(sandbox.is_available());
        }
    }

    // ── Parent-process protection ──
    //
    // `apply_restrictions()` must only ever run inside the short-lived,
    // freshly re-exec'd `internal-landlock-exec` child, never in the
    // long-lived daemon (parent).  These tests verify the daemon (parent)
    // process is never restricted.

    /// Regression: `wrap_command` must NOT restrict the parent process.
    ///
    /// Before the fix, restrictions were applied directly to the calling
    /// process inside `wrap_command`, which locked the daemon itself
    /// within the Landlock ruleset. Now enforcement only ever happens
    /// inside the dedicated re-exec'd child via `internal-landlock-exec`.
    #[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
    #[test]
    fn wrap_command_does_not_restrict_parent_process() {
        let sandbox = match LandlockSandbox::new() {
            Ok(s) => s,
            Err(_) => return, // Landlock not available — skip
        };

        // /etc/passwd is world-readable on every Linux but NOT in the
        // Landlock allow-list (/tmp, /usr, /bin).  After wrap_command
        // the parent must still be able to read it.
        let sentinel = Path::new("/etc/passwd");

        // The sentinel must exist and be readable before the test starts.
        // If it doesn't, the test environment is broken — fail loudly
        // rather than silently passing without verifying anything.
        assert!(
            sentinel.exists(),
            "/etc/passwd must exist as a sentinel — test environment is broken"
        );
        assert!(
            std::fs::read_to_string(sentinel).is_ok(),
            "/etc/passwd must be readable before sandboxing — test environment is broken"
        );

        let mut cmd = std::process::Command::new("true");
        sandbox
            .wrap_command(&mut cmd)
            .expect("wrap_command must succeed");

        cmd.spawn()
            .expect("child spawn must succeed")
            .wait()
            .expect("child wait must succeed");

        // THE CORE ASSERTION: after wrap_command the parent must STILL
        // be able to read /etc/passwd.  If this fails, restrict_self()
        // was called in the parent — which is the bug this commit fixes.
        assert!(
            std::fs::read_to_string(sentinel).is_ok(),
            "parent process must NOT be restricted by wrap_command — \
             restrict_self() must only run inside the re-exec'd internal-landlock-exec child"
        );
    }

    /// `wrap_command` must return `Ok(())` on a valid command.
    #[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
    #[test]
    fn wrap_command_returns_ok() {
        let sandbox = match LandlockSandbox::new() {
            Ok(s) => s,
            Err(_) => return,
        };

        let mut cmd = std::process::Command::new("true");
        assert!(sandbox.wrap_command(&mut cmd).is_ok());
    }

    /// Calling `wrap_command` on multiple distinct commands must not
    /// panic or fail. Each call independently rewrites its own `Command`
    /// to re-exec through `internal-landlock-exec`, so wrapping multiple
    /// commands is safe.
    #[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
    #[test]
    fn wrap_command_multiple_distinct_commands() {
        let sandbox = LandlockSandbox::new().expect("Failed to create landlock sandbox");

        for i in 0..3 {
            let mut cmd = std::process::Command::new("true");
            sandbox
                .wrap_command(&mut cmd)
                .unwrap_or_else(|e| panic!("wrap_command call #{i} failed: {e}"));
        }
    }

    /// When a workspace directory is set, `wrap_command` must still
    /// not lock the parent process.
    #[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
    #[test]
    fn wrap_command_with_workspace_does_not_restrict_parent() {
        let tmp = tempfile::TempDir::new().expect("must create temp dir");

        let sandbox = LandlockSandbox::with_workspace(Some(tmp.path().to_path_buf()))
            .expect("Failed to create landlock sandbox");

        let sentinel = Path::new("/etc/passwd");

        // The sentinel must exist and be readable before the test starts.
        assert!(
            sentinel.exists(),
            "/etc/passwd must exist as a sentinel — test environment is broken"
        );
        assert!(
            std::fs::read_to_string(sentinel).is_ok(),
            "/etc/passwd must be readable before wrap_command — test environment is broken"
        );

        let mut cmd = std::process::Command::new("true");
        sandbox
            .wrap_command(&mut cmd)
            .expect("wrap_command must succeed");

        cmd.spawn()
            .expect("child spawn must succeed")
            .wait()
            .expect("child wait must succeed");

        assert!(
            std::fs::read_to_string(sentinel).is_ok(),
            "parent must not be restricted even with workspace configured"
        );
    }

    // ── §1.1 Landlock stub tests ──────────────────────────────

    #[cfg(not(all(feature = "sandbox-landlock", target_os = "linux")))]
    #[test]
    fn landlock_stub_wrap_command_returns_unsupported() {
        let sandbox = LandlockSandbox;
        let mut cmd = std::process::Command::new("echo");
        let result = sandbox.wrap_command(&mut cmd);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
    }

    #[cfg(not(all(feature = "sandbox-landlock", target_os = "linux")))]
    #[test]
    fn landlock_stub_new_returns_unsupported() {
        let result = LandlockSandbox::new();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
    }

    #[cfg(not(all(feature = "sandbox-landlock", target_os = "linux")))]
    #[test]
    fn landlock_stub_probe_returns_unsupported() {
        let result = LandlockSandbox::probe();
        assert!(result.is_err());
    }

    // ── Cerveau (enterprise-hardening round: OfficeCLI workspace isolation)
    // wrap_command re-exec rewrite ───────────────────────────────────────
    //
    // These test the *rewrite* in-process (fast, no spawning) — they can't
    // also prove OS-level enforcement here: `wrap_command` re-execs via
    // `std::env::current_exe()`, which under `cargo test` resolves to this
    // test binary itself (not the real `zeroclaw` CLI, which lives in a
    // different crate — `CARGO_BIN_EXE_*` doesn't reach across crates
    // either), so there is no faithful *and* self-contained way to spawn
    // the real re-exec path from a unit test here. The actual OS-boundary
    // proof — a real absolute-path escape attempt genuinely denied — is
    // step 4 of this round's live VPS verification against the real
    // compiled binary and real kernel, which is more trustworthy than a
    // synthetic stand-in binary would be anyway.

    #[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
    #[test]
    fn wrap_command_rewrites_to_self_reexec_with_workspace_and_original_command() {
        let workspace = std::path::PathBuf::from("/tmp/example-tenant-workspace");
        let sandbox = LandlockSandbox {
            workspace_dir: Some(workspace.clone()),
        };
        let mut cmd = std::process::Command::new("cat");
        cmd.arg("/etc/passwd");
        sandbox.wrap_command(&mut cmd).expect("wrap_command should succeed with a workspace set");

        let current_exe = std::env::current_exe().unwrap();
        assert_eq!(cmd.get_program(), current_exe.as_os_str());

        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            args,
            vec![
                "internal-landlock-exec".to_string(),
                "--workspace".to_string(),
                workspace.to_string_lossy().into_owned(),
                "--".to_string(),
                "cat".to_string(),
                "/etc/passwd".to_string(),
            ]
        );
    }

    #[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
    #[test]
    fn wrap_command_preserves_original_env_vars() {
        let sandbox = LandlockSandbox {
            workspace_dir: Some(std::path::PathBuf::from("/tmp/ws")),
        };
        let mut cmd = std::process::Command::new("echo");
        cmd.env("MY_VAR", "my_value");
        sandbox.wrap_command(&mut cmd).unwrap();

        let envs: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(std::ffi::OsStr::to_owned)))
            .collect();
        assert_eq!(
            envs.get(std::ffi::OsStr::new("MY_VAR")),
            Some(&Some(std::ffi::OsString::from("my_value"))),
            "an env var set on the original command must survive the rewrite, \
             unlike FirejailSandbox's equivalent (which doesn't need to, since \
             shell.rs always rebuilds env after wrapping regardless)"
        );
    }

    #[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
    #[test]
    fn wrap_command_preserves_original_current_dir() {
        let sandbox = LandlockSandbox {
            workspace_dir: Some(std::path::PathBuf::from("/tmp/ws")),
        };
        let mut cmd = std::process::Command::new("echo");
        cmd.current_dir("/tmp/some-caller-set-cwd");
        sandbox.wrap_command(&mut cmd).unwrap();

        assert_eq!(
            cmd.get_current_dir(),
            Some(std::path::Path::new("/tmp/some-caller-set-cwd")),
            "current_dir set on the original command must survive the rewrite \
             (the exec'd target inherits the wrapper process's CWD, so losing \
             this would silently break CWD-relative I/O scoping)"
        );
    }

    #[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
    #[test]
    fn wrap_command_without_workspace_fails_closed() {
        let sandbox = LandlockSandbox { workspace_dir: None };
        let mut cmd = std::process::Command::new("echo");
        let result = sandbox.wrap_command(&mut cmd);
        assert!(
            result.is_err(),
            "with no workspace to scope to, wrap_command must refuse rather than \
             execute unsandboxed or restrict to an arbitrary default"
        );
    }
}
