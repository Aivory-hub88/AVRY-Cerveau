//! Landlock sandbox (Linux kernel 5.13+ LSM)
//!
//! Landlock provides unprivileged sandboxing through the Linux kernel.
//! This module uses the pure-Rust `landlock` crate for filesystem access control.

#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
use landlock::{AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr};
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
                AccessFs::ReadFile
                    | AccessFs::WriteFile
                    | AccessFs::ReadDir
                    | AccessFs::RemoveDir
                    | AccessFs::RemoveFile
                    | AccessFs::MakeChar
                    | AccessFs::MakeSock
                    | AccessFs::MakeFifo
                    | AccessFs::MakeBlock
                    | AccessFs::MakeReg
                    | AccessFs::MakeSym,
            )
            .and_then(|ruleset| ruleset.create())
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        // Allow workspace directory (read/write)
        if let Some(ref workspace) = self.workspace_dir
            && workspace.exists()
        {
            let workspace_fd =
                PathFd::new(workspace).map_err(|e| std::io::Error::other(e.to_string()))?;
            ruleset = ruleset
                .add_rule(PathBeneath::new(
                    workspace_fd,
                    AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::ReadDir,
                ))
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        // Allow /tmp for general operations
        let tmp_fd =
            PathFd::new(Path::new("/tmp")).map_err(|e| std::io::Error::other(e.to_string()))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(
                tmp_fd,
                AccessFs::ReadFile | AccessFs::WriteFile,
            ))
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        // Allow /usr and /bin for executing commands
        let usr_fd =
            PathFd::new(Path::new("/usr")).map_err(|e| std::io::Error::other(e.to_string()))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(
                usr_fd,
                AccessFs::ReadFile | AccessFs::ReadDir,
            ))
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let bin_fd =
            PathFd::new(Path::new("/bin")).map_err(|e| std::io::Error::other(e.to_string()))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(
                bin_fd,
                AccessFs::ReadFile | AccessFs::ReadDir,
            ))
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        // Allow /etc (read-only) — standard system config, same trust tier
        // as /usr and /bin. Discovered live: Node's own OpenSSL init reads
        // /etc/ssl/openssl.cnf at startup regardless of what the spawned
        // tool actually does; other runtimes commonly need /etc/resolv.conf,
        // /etc/nsswitch.conf, /etc/localtime, ca-certificates, etc. None of
        // this is tenant- or workspace-specific, so it's not a workspace-
        // isolation concern — omitting it only breaks legitimate use.
        let etc_fd =
            PathFd::new(Path::new("/etc")).map_err(|e| std::io::Error::other(e.to_string()))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(
                etc_fd,
                AccessFs::ReadFile | AccessFs::ReadDir,
            ))
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        // Allow the exec target's own (operator-installed, non-tenant)
        // install root, if the caller resolved one — same trust tier as
        // /usr and /bin above.
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

        // Apply the ruleset
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
                Err(std::io::Error::other(e.to_string()))
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
