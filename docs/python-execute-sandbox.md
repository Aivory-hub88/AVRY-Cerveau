# python_execute sandbox: decision record and operations

Status: live on prod since 2026-11-03 for `agent_analyst_brain` only.
Implementation: PR #49 (backend and helper), PR #50 (first-prod-run fixes).
Install and rollback commands: `scripts/sandbox/README.md`.

## Problem

Agents need to run Python (numpy) safely, with bounded concurrency, on a single
VPS that is also prod. Constraints found on the host:

- No KVM, so no microVMs (Firecracker, Kata).
- Ubuntu 24.04, kernel 6.8, cgroup v2, 4 cores, 7.7 GB RAM.
- `apparmor_restrict_unprivileged_userns=1`.
- Docker has `live-restore=false` and about 40 containers, so restarting dockerd
  (needed to try gVisor) would bounce every service.
- `runtime.kind = "native"`, so the existing Docker python_execute path was off.

## Options measured on the prod host (4 concurrent, pass / numpy)

| Backend | Result |
|---|---|
| native (no isolation) | pass 21 ms. Baseline only. |
| bubblewrap as unprivileged user | Unusable: `loopback: Failed RTM_NEWADDR`, because AppArmor strips capabilities inside the new user namespace. Fixing it needs a custom AppArmor profile. |
| Docker runc | pass 437 ms (4.7/s). numpy not measured. |
| systemd-run transient unit | pass about 173 ms (13/s), numpy about 345 ms average. |
| gVisor, nsjail | Not measured: gVisor needs a dockerd restart, nsjail is not packaged. |

## Decision

A transient systemd service started by a root-owned helper. It needs no
unprivileged user namespaces, leaves AppArmor unchanged, and is the cheapest
option that was measured.

```
daemon (ubuntu) --sudo -n--> /usr/local/sbin/cerveau-pysandbox --exec--> systemd-run
                                                     |
                                   unit runs as cerveau-sandbox
```

Sandbox properties: dedicated user `cerveau-sandbox`, no network
(`PrivateNetwork`), private `/tmp` and `/dev`, `ProtectSystem=strict`,
`ProtectHome=tmpfs`, `ProtectProc=invisible`, `NoNewPrivileges`, empty
capability set, `RestrictNamespaces`, a seccomp allowlist
(`@system-service` minus `@privileged @resources`) with
`SystemCallErrorNumber=EPERM`, `/etc /var /run /opt /srv /mnt` replaced by
empty read-only tmpfs (only `ld.so.cache` and the venv are bound back), only the
workspace writable, and `MemoryMax`, `MemorySwapMax=0`, `TasksMax`,
`CPUQuota`, `RuntimeMaxSec` from config.

The helper trusts nothing from the daemon: `SUDO_UID` must be the daemon user,
numbers are range-checked, the workspace must resolve under `ALLOWED_ROOTS` and
be owned by the daemon, the script name must match
`.zeroclaw_python_execute_<alnum>.py` as a regular non-symlink file, and the
interpreter must be `/usr/bin/python3*` or `/opt/<name>/bin/python*` and be
root-owned and not group/world-writable. 16 dry-run tests cover this
(`scripts/sandbox/cerveau-pysandbox.test.sh`).

Code: `PythonExecuteBackend` and the new `[python_execute]` fields in
`crates/zeroclaw-config/src/schema.rs`; command building in
`crates/zeroclaw-tools/src/python_execute.rs`; registration in
`crates/zeroclaw-runtime/src/tools/mod.rs`.

## Prod configuration

```toml
[python_execute]
enabled = true
backend = "systemd"
systemd_python = "/opt/cerveau-pyenv/bin/python"   # root-owned venv, numpy only
timeout_secs = 60
memory_limit_mb = 512
cpu_limit = 1.0
```

Access is per agent: `python_execute` is in `allowed_tools` and `auto_approve`
of `risk_profiles.agent_analyst_brain` only. To grant another agent, add the
tool to both lists of its risk profile and restart.

Host pieces: user `cerveau-sandbox`, package `acl`, helper,
`/etc/cerveau-pysandbox.conf`, `/etc/sudoers.d/cerveau-pysandbox`,
`/opt/cerveau-pyenv`.

## Bugs found on the first prod run (fixed in #50)

1. `ProtectHome=yes` made `/home` untraversable for the sandbox user
   (`status=200/CHDIR`). Now `ProtectHome=tmpfs` with the workspace bind-mounted.
2. The recursive read ACL pass ran after the `rwx` grant and downgraded the
   workspace directory to `r-x`, so writes failed. The recursive pass now runs
   first.
3. Found by a test before prod: `/opt/../bin/python3` passed the interpreter
   check. The name must not start with a dot.
4. numpy died with SIGSYS under the seccomp filter. Fixed with
   `SystemCallErrorNumber=EPERM`.

## Known limits

- Subdirectories of a workspace are readable but not writable by the sandbox.
  Only the workspace root is.
- Timeouts end through the unit's `RuntimeMaxSec`; the daemon cannot signal the
  root-owned helper.
- Concurrency: the process-wide gate (`sandbox_gate.rs`) is built from
  `runtime.docker` limits. Whether systemd runs pass through it has not been
  confirmed. Measured headroom: 12 parallel numpy runs finish in about 2.1 s.
- Only numpy is installed. Add packages to `/opt/cerveau-pyenv` as root.
- This is process isolation, not a VM, so a kernel-level escape is possible in
  principle. The seccomp filter, no capabilities, no network and a non-login
  user keep the surface small.
- Not verified end to end: an agent turn that calls the tool.

## Operations

Smoke test (as the daemon user, on a real workspace):

```bash
sudo -n /usr/local/sbin/cerveau-pysandbox <workspace> .zeroclaw_python_execute_x.py 30 512 100 128 /opt/cerveau-pyenv/bin/python
```

Disable: set `enabled = false` under `[python_execute]` and restart
`zeroclaw-cerveau`. Remove the host side with the rollback steps in
`scripts/sandbox/README.md`.

Deploy lesson: do not health-check with `ss ... | grep -q` under `pipefail`
(SIGPIPE gives a false negative), and do not roll back by `cp` over a running
binary (`Text file busy`); use `install` or `mv`.
