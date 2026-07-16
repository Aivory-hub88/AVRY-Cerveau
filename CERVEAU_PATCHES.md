# Aivory Cerveau — Patch Series over upstream zeroclaw

**What this repo is:** Aivory's fork of [zeroclaw-labs/zeroclaw](https://github.com/zeroclaw-labs/zeroclaw)
(Apache-2.0), product name **Aivory Cerveau** — the multi-tenant deployable-agent engine behind
the Aivory user dashboard (Telegram/Slack/WhatsApp/Office Assistant agents).

**Base:** upstream tag `v0.8.3`. (Originally bootstrapped on `v0.8.1` for prod parity, re-based same day: the durable run/task **control plane only exists from v0.8.2**, and v0.8.3 carries a wave of security hardening — SSRF fixes, constant-time token comparison, RUSTSEC bumps. The Aivory production vanilla instance still runs v0.8.1; the webhook contract is unchanged.)
**Branch model:** `cerveau-main` = upstream base + the patch series below, kept **rebase-friendly**:
every Cerveau change is a focused commit on top of upstream, documented here. Where a change is
generic (not Aivory-specific), we attempt to upstream it and drop it from this series.
Upstream is not mirrored in this repo — add it as a remote when rebasing:

```
git remote add upstream https://github.com/zeroclaw-labs/zeroclaw.git
git fetch upstream --tags
```

**Planning docs** (in the private `AVRY-V2-Main` monorepo): `docs/DEPLOYABLE_AGENT_RUNTIME_PLANNING.md`
(phased execution plan) and `docs/ADR-001-AIVORY-CERVEAU-PHASE0.md` (fork scope decisions).

---

## Patch series

| # | Patch | Why | Touches |
|---|---|---|---|
| 0001 | Replace upstream CI with `cerveau-build` | Upstream's ~25 workflows target their release/publish infra (secrets we don't have; scheduled jobs we don't want). One workflow builds the one artifact we deploy: `zeroclaw` for `x86_64-unknown-linux-gnu`, same recipe as upstream's v0.8.3 release (toolchain 1.96.1, `cargo web build` for the embedded dashboard, feature set resolved via `cargo run -p xtask --bin generate -- features --selection dist`) | `.github/workflows/` |

### Planned (per the execution plan; not yet applied)

- **P-identity** — DB-backed multi-tenant identity resolution (persona/tone/language/voice/tool-scope
  per authenticated tenant from Postgres), alongside the existing config-alias model. Stamps
  `TaskRecord.principal_id` (upstream's own EPIC-D seam).
- **P-isolation** — row-level tenant scoping in `zeroclaw-memory` / `control_plane` Postgres backends,
  mirroring upstream's default-jailed per-agent semantics at tenant granularity.
- **F-1** — boot recovery enqueues a continuation for owned tasks with a persisted
  `TaskContinuationContext` instead of defaulting them to `Lost`.
- **F-2** — idempotency keys on side-effectful tool executions
  (dedup key = task id + turn + tool + args hash). Upstream candidate.

## Rebase procedure

1. `git fetch upstream --tags`
2. `git rebase <new-upstream-tag> cerveau-main` (patches are focused; conflicts should be small and
   localized — if a patch conflicts hard, check whether upstream implemented the same idea and drop ours)
3. Update this file's table if any patch was dropped/renumbered
4. Push; CI must produce a working artifact before the binary is deployed
