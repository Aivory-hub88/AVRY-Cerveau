# Aivory Cerveau — shipped skills

Aivory-authored `SKILL.md` content (per the open Agent Skills spec,
agentskills.io — the same format Cerveau's `zeroclaw-runtime/src/skills`
module natively parses/scaffolds/installs). This directory is **content**,
not engine code: it does not ship inside the compiled binary and is not
part of the `cerveau-build` release artifact. It is a source-controlled
staging area for skills meant to be installed into a deployed Cerveau
instance's skill bundle directories.

Verified this session: `crates/zeroclaw-runtime/src/skills/document.rs`'s
`SkillDocument::parse` round-trips every file below cleanly (frontmatter +
body) — checked via a throwaway test, not shipped as a permanent test since
this content isn't engine code.

## Layout

```
cerveau-skills/<agent-type>/<skill-name>/
  SKILL.md              # required: frontmatter (name, description, ...) + instructions
  references/            # supporting docs the skill's instructions point to
  scripts/                # optional executable helpers
  assets/                 # optional templates/binary assets
```

## Installing a skill from here into a running Cerveau instance

These skills are **tenant-agent-type-scoped**, not host-agent-alias-scoped —
patch 0011 added the resolution primitive for exactly this, after tracing
how the *current* (pre-Cerveau) bridge already solves the same problem:
`telegram-agent.js`'s `agent_type` is a per-request data value that
dynamically selects prompt/tools in one running process — never a
provisioning axis (no per-agent-type host/container/config). Cerveau now
mirrors that, on top of the same tenant-context pattern already used for
persona injection and Composio entity scoping:

1. Put the skill's files under a `[skill_bundles.<alias>]` entry as usual
   (`directory` relative to the workspace root, or absolute).
2. Grant it to a **tenant agent type**, not a host alias, via
   `[agent_type_skill_bundles.<agent_type>]`:
   ```toml
   [skill_bundles.finance-invoice-ops]
   directory = "cerveau-skills/finance-invoice-ops/invoice-processing"

   [agent_type_skill_bundles.finance_invoice_ops]
   bundles = ["finance-invoice-ops"]
   ```
   Every tenant turn whose `X-Agent-Type: finance_invoice_ops` header
   authenticates gets this bundle **on top of** whatever the serving host
   `[agents.<alias>]` already grants via its own `skill_bundles` — resolved
   fresh per turn via `Config::skill_bundle_aliases_for_tenant`, no new host
   alias needed. A vanilla (non-tenant) turn, or a tenant of a *different*
   `agent_type`, never sees it (fail-closed, same "omission is not a grant"
   rule as `mcp_servers_for_bundles`).
3. Skills granted this way are still **shared across every tenant of that
   agent_type** (capability, not tenant data) — a bad skill affects all of
   them at once; review before installing to a production instance, ideally
   through `zeroclaw-runtime/src/skills/review.rs`'s audit pipeline.
4. This only grants the skill; it doesn't yet make the `?agent=` host alias
   the current bridge would need to pass line up with the underlying
   `[agents.<alias>]` config, or wire a real Composio toolkit connection —
   see each skill's own reference doc for what's still needed before a
   tenant turn can actually use it end to end.

## Current contents

- `finance-invoice-ops/invoice-processing/` — jurisdiction-agnostic invoice
  extraction + action skill for the `finance_invoice_ops` agent. Defers all
  tax/currency/e-invoicing-format logic to whatever accounting platform
  (QuickBooks/Xero/Stripe/Zoho Books/FreshBooks/Sage/NetSuite — see the
  skill's `references/composio-toolkits.md`) the tenant has connected via
  Composio, entity-scoped through Cerveau patch 0010
  (`mcp_servers_for_agent_and_tenant`).
- `office-assistant/meeting-outcomes/` — extracts decisions/action-items
  (owner + due date)/risks from meeting notes for the `office_assistant`
  agent (Enterprise-gated on the platform side via `record_meeting_summary`
  in `ENTERPRISE_TOOLS`), always persisted locally first, then synced to
  whichever of Notion/Slack/Google Sheets the tenant has connected — all
  three toolkits are already in `COMPOSIO_CURATED`, no new toolkit needed.
  The `COMPOSIO_CURATED.slack` deprecated-tool bug this skill's research
  turned up is **fixed** (`telegram-agent.js`, live on both git and the
  running vps-bridge process as of 2026-07-20).
- `customer-service/ticket-triage/` — triage → resolve → log → escalate for
  the `customer_service` agent. No usable open-source skill existed for
  this (the one candidate found in earlier research was an unlicensed
  demo), so this is authored from scratch. Logs locally via `create_ticket`
  always; syncs to Zendesk/Freshdesk (classic tickets) or Intercom
  (conversation-based, no ticket object) if connected — none of the three
  are in `COMPOSIO_CURATED` yet.
- `leads-qualifier/bant-qualification/` — inbound BANT (Budget, Authority,
  Need, Timeline) qualification for the `leads_qualifier` agent, one
  question at a time. Also authored from scratch (the closest open-source
  candidate does outbound prospecting, a different job). Saves locally via
  `save_lead` always; syncs to exactly one CRM (HubSpot/Salesforce/
  Pipedrive — HubSpot's contact action is already curated, the other two
  are not) if connected — no fan-out to multiple CRMs, unlike the
  office-assistant skill's multi-target sync.

**None of the four skills above are installed to any running instance yet**
— all are parse-verified drafts. Installing them is now unblocked (patch
0011), but still needs: deciding which `[agents.<alias>]` host(s) actually
serve Aivory tenant traffic once Phase 6 wires a real bridge integration,
and — for finance-invoice-ops/office-assistant specifically — picking +
wiring a real Composio toolkit connection (see each skill's reference doc).
