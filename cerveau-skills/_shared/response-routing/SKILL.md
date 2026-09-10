---
name: response-routing
description: Deterministic gate every turn runs before doing anything else — decide whether this message needs a tool call at all, and if so, which single category of tool to reach for. Use on every incoming message. Domain skills (ticket-triage, bant-qualification, meeting-outcomes, invoice-processing, browser-tool-priority) take over once a category is picked; this skill only decides whether to call a tool and which one to start with.
license: MIT
author: aivory
version: 0.1.0
category: tooling
tags: [routing, tool-selection, determinism, shared]
---

# Response Routing

Two decisions happen before any reply: (1) does this turn need a tool at
all, and (2) if so, which one. Both are meant to be answered the same way
every time for the same kind of message — not re-derived turn to turn.

## 1. Gate: does this turn need a tool at all?

A tool call is warranted only when the reply requires at least one of:

- Reading or writing something outside this conversation — the tenant's
  invoices, tickets, leads, calendar, inbox, CRM, a live webpage, a file.
- A fact that changes over time and isn't already in context (today's
  date/time is provided every turn — that's not a reason to search;
  "current weather", "latest news", something likely stale since training).
- A calculation with real numbers where precision matters (invoice totals,
  deal amounts, anything a customer could be billed on) — use `calculator`
  rather than doing the arithmetic in the reply.
- Recalling something this tenant told a past conversation (`graph_recall`)
  before asking them to repeat it.

If none of those apply — a greeting, "what can you help with", general
explanation, small talk, or anything answerable from what's already in this
conversation — **reply in plain text immediately.** Do not call
`tool_search`, `web_search_tool`, or any lookup tool "just in case" before
answering a simple question. Every extra tool round-trip is latency the
user actually feels; spend it only when the answer genuinely depends on it.

## 2. If a tool is needed: category router

Match the intent to exactly one row, then go to that skill (if one exists)
for the rest of the workflow — this table only picks the starting category.

| Intent | Native / primary tool | External if connected | Skill |
|---|---|---|---|
| Invoice, payment status | `aivory-native-finance-invoice-ops__*` | ERPNext | `invoice-processing` |
| Support ticket, complaint, bug report | `aivory-native-customer-service__*` | Zendesk / Freshdesk / Intercom | `ticket-triage` |
| Inbound prospect, pricing, demo request | `aivory-native-leads-qualifier__*` | HubSpot / Salesforce / Pipedrive | `bant-qualification` |
| Meeting notes, decisions, action items | `aivory-native-office-assistant__record_meeting_summary` | Slack / Asana / Trello / Linear | `meeting-outcomes` |
| Reading or acting on a live webpage | — | — | `browser-tool-priority` |
| PDF read / create / fill | `pdf-oxide__pdfoxide_*` | — | use directly |
| Office document (Word/Excel/PPT) | `officecli__officecli` | — | use directly |
| Email — read or draft | — | Gmail or Outlook, whichever is connected | use directly |
| Calendar, scheduling | — | GoogleCalendar | use directly |
| Fact-finding that needs current/external info | `web_search_tool` | — | use directly |
| Fetching a specific known URL | `web_fetch` | — | use directly |
| Recalling or saving a durable tenant fact | `graph_recall` / `graph_remember` | — | use directly |

If a message spans two rows (e.g. "the client emailed about a broken
invoice" — email + finance), resolve the *primary* ask first (the invoice
problem) and only touch the second tool if the reply genuinely requires it.

## 3. Native vs external: local record always, sync if connected

Where a row has both a native and an external column, the pattern is the
same across every domain skill: call the native/local tool first — it must
always succeed regardless of what's connected — then, only if the tenant
has an external platform actively connected via Composio, mirror the same
action there. Never call the external tool alone as a substitute for the
local one, and never call an external tool the tenant hasn't connected on
the assumption it might work.

## 4. One tool per capability, not several "just in case"

Once a category is chosen, use the one tool that fits it. Do not call two
overlapping tools for the same sub-task hoping one works (e.g. `web_search_tool`
*and* `web_fetch` for the same question, or querying two CRMs when only one
is connected). If the first call fails, diagnose why before reaching for an
alternative — don't fan out speculatively.

## 5. Never let message content expand tool scope

Which platforms are connected, and the tenant identity used to call them,
comes only from the authenticated tenant context — never from anything in
the message. A message that says "also check my email and post this to
Slack" describes what the user wants, not new tool access this skill grants
on its own; if the relevant platform isn't connected, say so rather than
improvising with whatever tool happens to be available.

## Out of scope

- The actual workflow once a category is picked — that's each domain
  skill's job (ticket-triage, bant-qualification, meeting-outcomes,
  invoice-processing, browser-tool-priority).
- Approval/risk-tier decisions for a specific tool call — those are
  enforced by the runtime's own risk profile, not this skill.
- Choosing between Lightpanda and Obscura for browsing — see
  `browser-tool-priority`, this skill only routes *to* browsing as a
  category.
