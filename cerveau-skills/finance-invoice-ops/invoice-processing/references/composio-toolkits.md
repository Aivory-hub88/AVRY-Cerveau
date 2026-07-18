# Composio accounting toolkits for `invoice-processing`

Confirmed live against **Aivory's actual Composio account**
(`GET /api/v3/toolkits/<slug>`, `enabled: true` on every row below) — not
just public docs. Checked 2026-07-18.

## Confirmed enabled, invoice-capable, multi-country

| Toolkit | Slug | Auth | Notes |
|---|---|---|---|
| QuickBooks | `QUICKBOOKS` | OAuth2, Composio-managed | 114 tools: create/read invoices, estimates, bills, payments, customers/vendors, full reports (balance sheet, P&L, aged receivables). International editions exist; strongest in US/CA/UK/AU. 0 triggers (no webhooks — poll, don't expect push events). |
| Xero | `XERO` | OAuth2, **self-managed** (no Composio-managed auth scopes returned — Aivory needs its own Xero OAuth app credentials, not just the Composio key) | 53 tools: create/read/update invoices, credit notes, bank transactions, contacts, payments, quotes; reports. Strong in UK/AU/NZ/US/CA/Singapore. |
| Stripe | `STRIPE` | OAuth2, Composio-managed (also supports raw API_KEY auth) | `STRIPE_CREATE_INVOICE`, `_CREATE_INVOICE_ITEM`, `_ADD_INVOICE_LINES`, `_CREATE_CREDIT_NOTE`, `_ATTACH_INVOICE_PAYMENT`, `_FINALIZE_INVOICE`. Genuinely global (190+ countries). Best fit when the tenant wants invoicing bundled with actual payment collection. |
| Zoho Books | `ZOHO_BOOKS` | OAuth2 | Confirmed enabled; action list not yet drilled (tool-search query returned empty this session — needs the dashboard or a corrected query param, not a real absence). Strong in India/APAC/global SMB — likely the best fit for non-US/EU/AU tenants. |
| FreshBooks | `FRESHBOOKS` | OAuth2 | Confirmed enabled; sample actions seen: `FRESHBOOKS_LIST_BUSINESSES`, `FRESHBOOKS_LIST_PROJECTS` (invoice-specific actions not yet enumerated). SMB-focused, North America-strong. |
| Sage | `SAGE` | OAuth2 | Confirmed enabled; action list not yet drilled. Strong in UK/EU mid-market. |
| NetSuite | `NETSUITE` | none listed (likely custom/self-managed OAuth per-tenant NetSuite account, not Composio-managed) | Confirmed enabled; enterprise-tier, heavier onboarding per tenant (each NetSuite account needs its own app registration) — lowest priority for SMB-focused Aivory tenants. |

## Checked, does not exist on Composio

- **Wave** — every slug guess (`WAVE`, `WAVEAPPS`) returned 404. Not on Composio's catalog under any obvious name. If a tenant specifically needs Wave, it isn't reachable via Composio today.

## Not relevant (checked, ruled out during skill design)

- `6missedcalls/personal-finance-skill` — US personal wealth management (Plaid/Alpaca/IBKR), not a Composio toolkit, not business invoicing.
- BuilderCed's `fr-facturation-electronique` / `fr-comptabilite` / `fr-fec-generator` — France-only (Factur-X, PCG, DGFIP); not on Composio, not general. This is *why* this skill's core instructions carry no hardcoded tax/currency logic — those repos' approach doesn't generalize to a global tenant base.

## Suggested launch order

Stripe (most global, simplest Composio-managed auth) → QuickBooks
(Composio-managed auth, huge action set) → Zoho Books (best fit for
APAC/non-Western tenants) → Xero (self-managed OAuth app adds setup work) →
FreshBooks/Sage → NetSuite (enterprise, defer).

## Before wiring a toolkit in

1. Drill the actual action list for `ZOHO_BOOKS` / `SAGE` / FreshBooks'
   invoice-specific actions (not done this session — param issue, not a
   real absence).
2. Curate a small action subset per toolkit (mirroring `COMPOSIO_CURATED` in
   `backend/vps-bridge/telegram-agent.js`) — create invoice, list invoices,
   get invoice, list/get payments, list customers. Do not expose all 114
   QuickBooks tools or all 53 Xero tools to the LLM at once.
3. Wire the chosen toolkit(s) in, either as a `tenant_entity_query_param`
   -gated `[[mcp.servers]]` entry per Cerveau patch 0010 (Composio's MCP
   endpoint), or the way `telegram-agent.js` already does today
   (`composioConnectedToolkits` / `composioExecute` per tenant `user_id`,
   REST API) — whichever integration path finance_invoice_ops ends up on.

## How this was checked

`curl -H "x-api-key: <COMPOSIO_API_KEY>" https://backend.composio.dev/api/v3/toolkits/<SLUG>`
per candidate slug; `enabled: true` + a real `name` field = confirmed live.
