# Room delegation canary — live prod state (Cerveau `:3100`)

The product team is Geno, Teo, Lex, Finn, and Ofira. Aira
(`chief_of_staff`) is the supervisor and is intentionally documented
separately from the specialist mesh.

## Applied to `/home/ubuntu/.zeroclaw-cerveau/config.toml`

```toml
[[agents.chief_of_staff.delegates]]
agent = "autonomous"
mode = "bounded"

[[agents.chief_of_staff.delegates]]
agent = "customer_service"
mode = "bounded"

[[agents.chief_of_staff.delegates]]
agent = "leads_qualifier"
mode = "bounded"

[[agents.chief_of_staff.delegates]]
agent = "finance_invoice_ops"
mode = "bounded"

[[agents.chief_of_staff.delegates]]
agent = "office_assistant"
mode = "bounded"

[risk_profiles.agent_chief_of_staff.delegation_policy]
mode = "allow"
```

Plus, per agent: `max_delegation_depth = 0` → `1` in
`[runtime_profiles.agent_*]`, and `"delegate"` appended to
`allowed_tools` — deliberately NOT to `auto_approve`, so every
delegation parks as an F-1 approval in the dashboard (human-in-the-loop).

## Staging proof (before flip)

Staging daemon `:3101` (isolated DB `aivory_cerveau_staging`):
- Trial 1 (room-style prompt, approval-gated): Teo coordinated in text,
  no delegate fired — mechanism idle, no spend.
- Trial 2 (explicit delegate order, auto-approve): `delegate` fired once
  (`zeroclaw_tool_calls_total{success="true",tool="delegate"} 1`),
  full round 13.2s / 44,547 tokens on the shared analyst key.
- Denied/parked attempts fail closed, nothing loops (depth 1).

## Rollback

```bash
# VPS: restore pre-flip backup, restart
sudo cp /home/ubuntu/.zeroclaw-cerveau/config.toml.bak-pre-room-delegates-<TS> \
  /home/ubuntu/.zeroclaw-cerveau/config.toml
sudo systemctl restart zeroclaw-cerveau
```

## Promote pattern (next agent)

Mirror the 6 edits above for the new pair, keep depth 1,
keep `delegate` out of `auto_approve` until its own staging trial.

## Staging expansion trial (hub model + peer_groups)

Staging daemon `:3101`, isolated DB `aivory_cerveau_staging`, trial tenant
rows only — prod untouched. Staging-only extras vs prod: `delegate` briefly
in `auto_approve` (trial 2), then reverted to approval-gated.

Config: hub edges Geno↔Lex, Geno↔Finn, Geno↔Ofira (depth 1, policy allow,
`delegate` in allowed_tools, NOT auto_approve) + `[peer_groups.room_team]`
(`channel = "console"`, all 5 agents). Boots clean, `config migrate` clean.

| Trial | Result |
|---|---|
| Teo room-style, no order | Text coordination, no delegate (6.7s) |
| Teo explicit delegate order, auto-approve | Delegate fired 1×, full loop 13.2s / 44,547 tokens |
| Lex explicit delegate order, approval-gated | NO delegate attempt — textual handoff instead (24.7s / 12.7k) |
| Lex out-of-scope ticket task | Clean refusal + redirect, no delegate (7.4s) |
| Teo room-style with peer_groups loaded | Peer-aware reply, no delegate fired (5.2s / 12.3k) |

Findings:
- Delegates are discretionary per turn, not forced routing — the agent
  decides; approval-gating further suppresses attempts (fails closed).
  No loops, no surprise spend observed in any trial.
- `peer_groups.room_team` loads and boots clean; no dramatic effect on
  webhook turns (it governs channel dispatch/pairing/session-send auth,
  not LLM context). Enforcement surface needs channel-level tests.
- Cost datum: normal room turn 5–7s / ~12k tokens; full delegated round
  13s / ~45k tokens on the shared analyst key (deepseek-v4.1-flash).

## Prod hub promotion

After the staging trial, the hub model is live in approval-gated mode:

- Geno <-> Teo (existing canary), Geno <-> Lex, Geno <-> Finn, and Geno
  <-> Ofira are bounded at depth 1.
- `delegate` is allowed for all five product agents but is intentionally
  absent from every product agent's `auto_approve` list. A delegate request
  therefore parks at the existing F-1 approval surface before the child turn
  runs.
- `peer_groups.room_team` is enabled for the `console` channel with all five
  product aliases plus `chief_of_staff` mutually opted in.
- Aira's direct tool surface is coordination-only: `delegate`, web research,
  calculator, read-only file access, memory/graph recall and internal task
  ledger tools. It has no specialist business integrations.
- Phase 3A guardrails: Aira is capped at `12` actions/hour, `200` cents/day,
  `8` tool iterations, and delegation depth `2`. These are native Cerveau
  runtime controls; the global daily/monthly budget remains the backstop.
- Backup before promotion: `/home/ubuntu/.zeroclaw-cerveau/config.toml.bak-pre-room-hub-20260915-073334`.
- Cerveau restarted cleanly and `/health` returned `status: ok` after the
  promotion.

### Phase 3A staging check

The first staging request exposed a name/alias mismatch (`Geno` versus the
runtime alias `autonomous`) and failed closed without a delegate call. The
identity now documents the alias map explicitly. The corrected read-only
trial delegated once to `autonomous`, completed in `~89k` reported tokens,
and made no external business changes. This is still a high-cost path, so
the native Aira caps remain enabled in production.

Rollback: restore that backup and restart `zeroclaw-cerveau`; this removes the
three new hub edges and the console peer group while retaining the earlier
Geno/Teo canary only if the backup was taken after that canary promotion.

## Observability (delegate + cost metrics)

`:3100/metrics` already exposes everything; nothing is scraped yet (the
monitoring stack isn't deployed — no prometheus/grafana containers running).
When it is, add:

```yaml
- job_name: zeroclaw-cerveau
  static_configs:
    - targets: ['host.docker.internal:3100']
```

(prometheus service needs `extra_hosts: ["host.docker.internal:host-gateway"]`.
`:3100` binds `0.0.0.0`, verified reachable.)

Key queries:
- Delegate fire rate: `sum(rate(zeroclaw_tool_calls_total{tool="delegate"}[5m]))`
- Delegate failures: `sum(rate(zeroclaw_tool_calls_total{tool="delegate",success="false"}[5m]))`
- Token burn (cumulative): `zeroclaw_tokens_input_total`, `zeroclaw_tokens_output_total`
- Last-round cost: `zeroclaw_tokens_used_last`
- Turn latency: `histogram_quantile(0.95, rate(zeroclaw_agent_duration_seconds_bucket[5m]))`

Note: `zeroclaw_tool_calls_total` only appears after the first delegate
fires in prod (Prometheus client registers counters lazily) — absence of
the series means zero delegate turns so far, not a scrape failure.
Approval-parked delegates surface separately in the dashboard Approvals
feed + `GET /api/aira/tasks` (`status=blocked` with approver named).
