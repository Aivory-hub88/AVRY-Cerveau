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
