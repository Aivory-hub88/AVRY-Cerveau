# Room delegation canary — live prod state (Cerveau `:3100`)

Canary pair only: `customer_service` (Teo) ↔ `autonomous` (Geno).
Lex / Finn / Ofira intentionally unchanged (`max_delegation_depth = 0`,
no delegates, policy default `forbidden`).

## Applied to `/home/ubuntu/.zeroclaw-cerveau/config.toml`

```toml
[[agents.customer_service.delegates]]
agent = "autonomous"
mode = "bounded"

[[agents.autonomous.delegates]]
agent = "customer_service"
mode = "bounded"

[risk_profiles.agent_customer_service.delegation_policy]
mode = "allow"

[risk_profiles.agent_autonomous.delegation_policy]
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
