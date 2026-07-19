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

## Current contents

- `finance-invoice-ops/invoice-processing/` — jurisdiction-agnostic invoice
  extraction + action skill for the `finance_invoice_ops` agent. Defers all
  tax/currency/e-invoicing-format logic to whatever accounting platform
  (QuickBooks/Xero/Stripe/Zoho Books/FreshBooks/Sage/NetSuite — see the
  skill's `references/composio-toolkits.md`) the tenant has connected via
  Composio, entity-scoped through Cerveau patch 0010
  (`mcp_servers_for_agent_and_tenant`). **Not yet installed to any running
  instance** — drafted and parse-verified only; wiring a real Composio
  toolkit connection is a separate follow-up (see the skill's reference doc
  for the checklist).
- `office-assistant/meeting-outcomes/` — extracts decisions/action-items
  (owner + due date)/risks from meeting notes for the `office_assistant`
  agent (Enterprise-gated on the platform side via `record_meeting_summary`
  in `ENTERPRISE_TOOLS`), always persisted locally first, then synced to
  whichever of Notion/Slack/Google Sheets the tenant has connected — all
  three toolkits are already in `COMPOSIO_CURATED`, no new toolkit needed.
  See `references/composio-tools.md` for verified tool schemas, including a
  flagged bug: `COMPOSIO_CURATED.slack` in `telegram-agent.js` currently
  points to Composio's deprecated `SLACK_CHAT_POST_MESSAGE`, not the
  current `SLACK_SEND_MESSAGE`. **Not yet installed to any running
  instance.**
