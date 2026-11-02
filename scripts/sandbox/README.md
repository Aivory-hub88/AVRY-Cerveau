# python_execute systemd sandbox

Runs each `python_execute` script as a transient systemd service. No Docker and
no unprivileged user namespaces are needed, so the AppArmor
`apparmor_restrict_unprivileged_userns` setting stays as it is.

How it fits together:

- The Cerveau daemon (user `ubuntu`) calls `sudo -n /usr/local/sbin/cerveau-pysandbox ...`.
- The helper validates every argument against `/etc/cerveau-pysandbox.conf`, grants
  the sandbox user access to that one workspace, and starts the service.
- The code runs as `cerveau-sandbox`: no network, no capabilities, a seccomp filter,
  read-only system, only the workspace writable, and memory / process / CPU / time limits.

## Install (as root)

```bash
# 1. Dedicated user. It needs no shell, no home and no login.
useradd --system --no-create-home --shell /usr/sbin/nologin cerveau-sandbox

# 2. Tools: setfacl for workspace access.
apt-get install -y acl

# 3. Helper, config, sudoers rule.
install -o root -g root -m 0755 scripts/sandbox/cerveau-pysandbox /usr/local/sbin/cerveau-pysandbox
install -o root -g root -m 0644 scripts/sandbox/cerveau-pysandbox.conf.example /etc/cerveau-pysandbox.conf
$EDITOR /etc/cerveau-pysandbox.conf        # set ALLOWED_ROOTS to the real workspace root(s)
install -o root -g root -m 0440 scripts/sandbox/cerveau-pysandbox.sudoers /etc/sudoers.d/cerveau-pysandbox
visudo -cf /etc/sudoers.d/cerveau-pysandbox
```

Then in the Cerveau config:

```toml
[python_execute]
enabled = true
backend = "systemd"
systemd_python = "/opt/pysandbox-venv/bin/python"   # optional: a root-owned venv with numpy etc.
```

A venv must live directly under `/opt/<name>/`, be owned by root and not be
writable by anyone else. `/usr/bin/python3` works with no venv.

## Smoke test (as the daemon user)

```bash
ws=/home/ubuntu/.zeroclaw-cerveau/workspace/<tenant>      # an existing workspace
f=$(mktemp "$ws/.zeroclaw_python_execute_XXXXXX.py"); echo 'print("hello", __import__("os").getuid())' > "$f"
sudo -n /usr/local/sbin/cerveau-pysandbox "$ws" "$(basename "$f")" 20 256 100 128 /usr/bin/python3
rm -f "$f"
```

Expected: `hello <uid of cerveau-sandbox>`. A refusal prints `cerveau-pysandbox: <reason>`.

## Tests

`scripts/sandbox/cerveau-pysandbox.test.sh` runs the helper in `--dry-run` mode and
checks that bad workspaces, script names, numbers and interpreters are rejected.
It needs GNU coreutils, so run it on Linux (or in an Ubuntu container).

## Rollback

```bash
rm /etc/sudoers.d/cerveau-pysandbox /usr/local/sbin/cerveau-pysandbox /etc/cerveau-pysandbox.conf
userdel cerveau-sandbox
```

and set `python_execute.backend` back to `"docker"` (or disable the tool).
