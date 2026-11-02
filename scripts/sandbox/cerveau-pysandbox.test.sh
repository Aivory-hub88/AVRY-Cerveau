#!/usr/bin/env bash
# Validation tests for cerveau-pysandbox (dry-run only; needs GNU coreutils,
# so run on Linux). Usage: scripts/sandbox/cerveau-pysandbox.test.sh
set -u
HELPER="$(cd "$(dirname "$0")" && pwd)/cerveau-pysandbox"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
mkdir -p "$T/ws/tenant1" "$T/other"
ln -s "$T/ws/tenant1" "$T/link"
S=.zeroclaw_python_execute_abc123.py
echo 'print(1)' >"$T/ws/tenant1/$S"
echo 'x' >"$T/ws/tenant1/notascript.txt"
cat >"$T/conf" <<CONF
ALLOWED_ROOTS="$T/ws"
SANDBOX_USER=nobody
DAEMON_USER=$(id -un)
CONF
export CERVEAU_PYSANDBOX_CONF="$T/conf"
PY=/usr/bin/python3
WS="$T/ws/tenant1"
fail=0
run() { "$HELPER" --dry-run "$@" >"$T/out" 2>"$T/err"; }
expect_ok()  { if run "$@"; then echo "ok   $NAME"; else echo "FAIL $NAME: $(cat "$T/err")"; fail=1; fi; }
expect_err() { if run "$@"; then echo "FAIL $NAME: accepted"; fail=1; else echo "ok   $NAME"; fi; }

NAME="valid request";            expect_ok  "$WS" "$S" 60 256 100 128 "$PY"
grep -q 'PrivateNetwork=yes' "$T/out" && grep -q 'CapabilityBoundingSet=' "$T/out" \
  && grep -q "BindPaths=$WS" "$T/out" || { echo "FAIL command is missing fixed properties"; fail=1; }
NAME="workspace outside roots";  expect_err "$T/other" "$S" 60 256 100 128 "$PY"
NAME="symlinked workspace";      expect_err "$T/link" "$S" 60 256 100 128 "$PY"
NAME="relative workspace";       expect_err "ws/tenant1" "$S" 60 256 100 128 "$PY"
NAME="root workspace";           expect_err "/" "$S" 60 256 100 128 "$PY"
NAME="script with traversal";    expect_err "$WS" "../x.py" 60 256 100 128 "$PY"
NAME="script wrong name";        expect_err "$WS" "notascript.txt" 60 256 100 128 "$PY"
NAME="script missing";           expect_err "$WS" ".zeroclaw_python_execute_zzz.py" 60 256 100 128 "$PY"
ln -s /etc/passwd "$WS/.zeroclaw_python_execute_link.py"
NAME="script is a symlink";      expect_err "$WS" ".zeroclaw_python_execute_link.py" 60 256 100 128 "$PY"
NAME="timeout too large";        expect_err "$WS" "$S" 9999 256 100 128 "$PY"
NAME="timeout zero";             expect_err "$WS" "$S" 0 256 100 128 "$PY"
NAME="memory not a number";      expect_err "$WS" "$S" 60 "256;id" 100 128 "$PY"
NAME="pids too large";           expect_err "$WS" "$S" 60 256 100 100000 "$PY"
NAME="interpreter off-list";     expect_err "$WS" "$S" 60 256 100 128 /bin/sh
NAME="interpreter traversal";    expect_err "$WS" "$S" 60 256 100 128 /opt/../bin/python3
NAME="wrong argument count";     expect_err "$WS" "$S" 60 256 100 128
exit $fail
