# Cerveau (ZeroClaw) skills

Version-controlled copy of the **ZeroClaw "Cerveau" daemon** skills + identity.
The daemon (`zeroclaw-cerveau.service`, port 3100) maps the dashboard's
`entrypoint` values to `SKILL.md` files **by directory name**:

```
/home/ubuntu/.zeroclaw-cerveau/skills/<entrypoint>/SKILL.md
```

These files were previously **only on the VPS** (not in any git repo), so a
re-provision would silently lose them. This directory + `sync.sh` makes them
reproducible.

## Entrypoints

| Entrypoint | Skill | Purpose |
|-----------|-------|---------|
| `workflow_generate` | `skills/workflow_generate/SKILL.md` | LLM generates a workflow from a request |
| `workflow_clarify` | `skills/workflow_clarify/SKILL.md` | Ask clarifying questions |
| `workflow_edit` | `skills/workflow_edit/SKILL.md` | Edit an existing workflow |
| `workflow_repair` | `skills/workflow_repair/SKILL.md` | Repair failed steps |
| `workflow_semantic_review` | `skills/workflow_semantic_review/SKILL.md` | **Blueprint semantic review** — strict JSON findings |

`identity.md` / `soul.md` are the daemon's business identity / tone.

## Agent Team

Product agents are mirrored under `agents/<type>/workspace/IDENTITY.md`:

| Agent | Type | Responsibility |
|---|---|---|
| Aira | `chief_of_staff` | Plan, delegate, track, and synthesize |
| Geno | `autonomous` | Generalist execution and cross-domain support |
| Teo | `customer_service` | Support and ticket operations |
| Lex | `leads_qualifier` | Sales and Lead: BANT qualification + outbound plays |
| Finn | `finance_invoice_ops` | Invoice and finance operations |
| Ofira | `office_assistant` | Meetings, tasks, and office operations |

Aira is coordination-only. It can use delegation, research, memory, graph,
and internal task tools, but it does not receive specialist business tools
directly. External writes remain owned and approval-gated by the specialist.

## Outbound sales skills (Lex)

Tier 1 knowledge-only skills vendored byte-identical from
`growthenginenowoslawski/coldoutboundskills` (MIT) under
`skills/leads-qualifier/`: ICP onboarding, copywriting, spam-word checks,
list-quality scorecards, lead magnets, campaign strategy, experiment design,
kickoff, weekly rhythm, ICP prompt builder, personalization pattern.
Anything spending money or sending externally (Tier 2: Smartlead, Prospeo,
domains, inboxes) is intentionally excluded until tenant keys exist.

## Sync

```bash
cd services/cerveau
./sync.sh status     # diff local vs VPS
./sync.sh capture    # pull VPS → repo (backup before changes)
./sync.sh deploy     # push repo → VPS + restart daemon
```

## How a new entrypoint is added

1. Create `skills/<entrypoint>/SKILL.md` here.
2. `./sync.sh deploy`.
3. In the dashboard, route the bridge operation with
   `entrypoint: '<entrypoint>'` (see `lib/workflows/bridgeCopilot.ts`).

## Caveat

`services/avry-zeroclaw/` (git submodule) contains **stale Python code** that
does not match the running daemon — the live Cerveau is a Rust binary
(`/usr/local/bin/zeroclaw-cerveau`) whose config/skills live under
`/home/ubuntu/.zeroclaw-cerveau/`. Treat this directory as the source of truth
for the daemon's prompts, not the submodule.
