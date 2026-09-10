---
name: conversational-gate
description: The one decision to make before anything else on every turn — does this message need a tool call, or is a plain conversational reply enough? Use on every incoming message, before picking any specific tool.
license: MIT
author: aivory
version: 0.1.0
category: tooling
tags: [routing, determinism, shared]
---

# Conversational Gate

Call a tool only when the reply genuinely requires one of:

- Reading or writing something outside this conversation — an invoice,
  ticket, lead, calendar, inbox, CRM record, a live webpage, a file.
- A fact that changes over time and isn't already in this conversation.
- A calculation with real numbers where precision matters — use
  `calculator` rather than doing it in the reply.
- Recalling something this tenant told a past conversation, before asking
  them to repeat it.

If none of those apply — a greeting, "what can you help with", small talk,
or anything answerable from what's already in this conversation — **reply
in plain text immediately.** Do not call `tool_search`, `web_search_tool`,
or any other tool "just in case" before answering something you already
know. Every extra tool call is latency the user feels for no benefit.
