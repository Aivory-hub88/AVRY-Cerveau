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

1. Copy `<agent-type>/<skill-name>/` into the target skill bundle's
   `directory` (see `[skill_bundles.<alias>]` in `config.toml` —
   `directory` is relative to the workspace root).
2. Ensure the host agent that serves that `agent-type`'s tenants has
   `skill_bundles = ["<alias>"]` in its `[agents.<alias>]` entry (or the
   bundle's `include`/`exclude` admits the skill by name).
3. Skills are loaded per **host agent alias**, not per-tenant — every tenant
   riding that agent_type shares the same installed skill set. This is
   correct (skills are capability, not tenant data) but means a bad skill
   affects every tenant of that agent_type at once; review before
   installing to a production host agent, ideally through
   `zeroclaw-runtime/src/skills/review.rs`'s audit pipeline.

**Blocker found 2026-07-20, not yet resolved:** the `:3100` Cerveau
instance's `[agents.<alias>]` entries are still the 6 generic vanilla-zeroclaw
brain roles (`analyst_brain`, `builder_brain`, `comms_brain`,
`diagnostic_brain`, `security_brain`, `workflow_brain`) — copied from prod
zeroclaw's own config, not from Aivory's 5 deployable-agent types. The
webhook's host-agent selection (`?agent=<alias>` query param /
`resolve_gateway_chat_agent_alias` in `zeroclaw-gateway/src/lib.rs`) is
**entirely independent** of the `X-Agent-Type` tenant header (that header
only drives persona lookup + memory/principal scoping, per
`zeroclaw-gateway/src/tenant.rs`). No ADR or the planning doc specifies how
`X-Agent-Type` values (`finance_invoice_ops`, `office_assistant`,
`customer_service`, `leads_qualifier`, `autonomous`) should map to a host
`[agents.<alias>]` — new dedicated aliases, or a `?agent=` param the
(not-yet-built) Phase 6 bridge integration would pass explicitly. **This is
a real open architectural question, decided nowhere yet** — none of the
four skills below can be installed to a live, traffic-serving agent until
it's resolved. Don't invent an answer unilaterally in a live config; it's a
product/engineering decision (whoever owns Phase 6) with real behavioral
consequences (wrong mapping = a tenant's turn silently runs on the wrong
persona/tools).

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

**None of the four skills above are installed to any running instance** —
all are parse-verified drafts blocked on the host-agent-alias question
above (or, for finance/office-assistant, additionally on picking + wiring a
real Composio toolkit connection).
