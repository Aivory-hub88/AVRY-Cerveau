# Composio tools for `bant-qualification`

Verified directly against `GET /v3/toolkits/<slug>` and `GET /v3/tools/<slug>`
on Aivory's Composio account, 2026-07-20.

## Toolkit status

| Toolkit | Slug | Enabled | Notes |
|---|---|---|---|
| HubSpot | `HUBSPOT` | ✅ (already partially curated — `HUBSPOT_CREATE_CONTACT` is in `COMPOSIO_CURATED` today for a different agent's use) | |
| Salesforce | `SALESFORCE` | ✅ | 97 total actions; has a native `Lead` object distinct from `Contact` — use the Lead actions, not Contact, for this skill. |
| Pipedrive | `PIPEDRIVE` | ✅ | 100 total actions; also has a native `Lead` object distinct from `Deal`/`Person`. |

## Actions

| Tool | Required params | Notes |
|---|---|---|
| `HUBSPOT_CREATE_CONTACT` | *(none required)* | `email` is the practical unique identifier — always pass it when known. Also has `company`, `phone`, `message` (good for a qualification note), plus demographic fields (`city`/`state`/`country`) — only fill what the conversation actually surfaced. |
| `SALESFORCE_CREATE_LEAD` | `last_name`, `company` | Will reject the call without both — ask the prospect rather than inventing a placeholder. Also has `rating`, `status`, `industry`, `description`. |
| `PIPEDRIVE_ADD_A_LEAD` | `title` | `title` is the lead's *display name* (e.g. "Acme Corp — website inquiry"), not a job title. `value__amount`/`value__currency` can carry an estimated deal size — only if the conversation surfaced one. `person_id`/`organization_id` link to existing Pipedrive records if known; omit if not. |

## Related but not used here

- `ComposioHQ/awesome-claude-skills`' `lead-research-assistant` — real,
  reviewed directly, but solves outbound prospecting (finding new
  companies matching an ICP) with a 1-10 fit score, not inbound BANT
  qualification of a lead who already made contact. Different job; not
  adapted into this skill.
- Zendesk/Intercom/Freshdesk (see the `ticket-triage` skill for
  `customer_service`) are support/ticketing platforms, not CRMs — a lead
  qualification result does not belong there.

## Before wiring this in

1. Curate exactly these three action slugs (or fewer, per launch priority)
   into `COMPOSIO_CURATED` in `telegram-agent.js` — `HUBSPOT_CREATE_CONTACT`
   is already there; `SALESFORCE_CREATE_LEAD` and `PIPEDRIVE_ADD_A_LEAD`
   are not yet added.
2. Decide the "one CRM, not fan-out" selection rule concretely: if a tenant
   somehow has more than one of HubSpot/Salesforce/Pipedrive connected,
   which wins? (Not specified anywhere yet — likely whichever the tenant's
   `agent_profiles` config designates as primary, but that field doesn't
   exist today either.)
3. Wire in the same tenant-entity-scoped pattern as the other two skills
   (Cerveau patch 0010, or REST via `composioExecute` per tenant `user_id`).
