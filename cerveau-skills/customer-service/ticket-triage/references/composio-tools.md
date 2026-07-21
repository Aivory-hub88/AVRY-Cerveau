# Composio tools for `ticket-triage`

Verified directly against `GET /v3/toolkits/<slug>` and `GET /v3/tools/<slug>`
on Aivory's Composio account, 2026-07-20. None of these are in
`COMPOSIO_CURATED` yet (`backend/vps-bridge/telegram-agent.js`) — adding one
is a prerequisite to actually using this skill against a real connection.

## Toolkit status

| Toolkit | Slug | Enabled | Auth |
|---|---|---|---|
| Zendesk | `ZENDESK` | ✅ | OAuth2 |
| Intercom | `INTERCOM` | ✅ | OAuth2 |
| Freshdesk | `FRESHDESK` | ✅ | none listed (API-key style, per-tenant subdomain) |

## Actions

| Tool | Required params | Notes |
|---|---|---|
| `ZENDESK_CREATE_ZENDESK_TICKET` | `subject`, `description` | `priority` accepts `urgent`/`high`/`normal`/`low` — same words as this skill's severity scale, map 1:1. `requester_name` and `requester_email` must be supplied together or not at all. |
| `FRESHDESK_CREATE_TICKET` | `subject`, `description` | Also has `priority`, `status`, `group_id`, `due_by`, `tags`. Never invent a `group_id` — omit if the tenant hasn't specified a routing group. |
| `INTERCOM_CREATE_CONVERSATION` | `body` | Intercom has no classic ticket object — conversations are the primary unit. `message_type` is one of `inapp`/`email`/`facebook`. Needs `from_user_id` or `from_contact_id` to attribute the conversation to the right customer. |
| `INTERCOM_ASSIGN_CONVERSATION` | `conversation_id` | Needs `admin_id` or `team_id` — use for escalation instead of a generic "escalate" call on Intercom. |
| `INTERCOM_CLOSE_CONVERSATION` / `INTERCOM_REOPEN_CONVERSATION` / `INTERCOM_REPLY_TO_CONVERSATION` | varies | Full conversation lifecycle exists if this skill is extended to handle follow-ups, not just initial triage. |

## Not found / ruled out

- No single "escalate ticket" action exists on Zendesk or Freshdesk in this
  toolkit set — escalation there means setting `priority: urgent` on
  create, or a follow-up update call (not yet enumerated here — check the
  full tool list before building update/escalate flows for these two).
- `HUBSPOT_CREATE_CONTACT` (already curated for `leads_qualifier`/CRM use)
  is a different HubSpot surface (CRM contacts, not HubSpot Service Hub
  tickets) — not reused here; if HubSpot ticketing is wanted later, it
  needs its own toolkit check, not assumed from the existing curation.

## Before wiring this in

1. Curate a small action subset in `COMPOSIO_CURATED` per connected
   platform (mirroring the pattern for `finance-invoice-ops` and
   `office-assistant`) — do not expose the full action list per toolkit.
2. Decide per-tenant which platform is authoritative if more than one is
   connected (unlike `office_assistant`'s "sync to all connected", a
   support ticket should probably go to exactly one system of record, not
   fan out — needs a product decision, not assumed here).
3. Wire in the same tenant-entity-scoped pattern as `invoice-processing`
   (Cerveau patch 0010 / `mcp_servers_for_agent_and_tenant`, or REST via
   `composioExecute` per tenant `user_id`).
