# Composio tools for `meeting-outcomes`

All verified directly against `GET /v3/tools/<slug>` on Aivory's Composio
account 2026-07-19 (not from documentation summaries — a prior WebFetch pass
this session hallucinated toolkit names that turned out not to exist, so
everything below was cross-checked against the raw API response).

**Unlike `finance_invoice_ops`, no new Composio toolkit needs to be enabled
for this skill.** Notion, Slack, and Google Sheets are already in
`COMPOSIO_CURATED` in `backend/vps-bridge/telegram-agent.js` — the work here
is using the *right* action within each, and handling the fields that
require a real target ID (never invented).

## Notion

| Tool | Required params | Notes |
|---|---|---|
| `NOTION_CREATE_NOTION_PAGE` | `parent_id`, `title` | `parent_id` is the UUID of an existing parent page or database — Notion has no "workspace root" you can create into blind. Optional: `icon` (single emoji), `cover` (public image URL). |
| `NOTION_APPEND_BLOCK_CHILDREN` | `block_id`, `children` | `block_id` = the page/block to append into (e.g. the page just created). `children` = an array of real Notion block objects (heading, paragraph, to-do, table, etc.) — not a single text blob. Optional `after` to insert after a specific existing block. |
| `NOTION_SEARCH_NOTION_PAGE` | *(none required)* | Empty query lists all accessible pages/databases — use this to help a tenant pick a `parent_id` instead of asking them to paste a UUID manually. |

## Slack

| Tool | Required params | Notes |
|---|---|---|
| `SLACK_SEND_MESSAGE` | `channel` | Use the `markdown_text` field for the message body — the plain `text` and `blocks` fields on this same tool are themselves marked deprecated in favor of `markdown_text`. `channel` accepts an ID or name. |
| ~~`SLACK_CHAT_POST_MESSAGE`~~ | — | **Deprecated by Composio** ("posts a message... use `send message` instead"). **This is what `COMPOSIO_CURATED.slack` in `telegram-agent.js` currently points to** — a real bug worth fixing at the source (swap to `SLACK_SEND_MESSAGE`), independent of this skill. Flagged 2026-07-19, not yet fixed. |

## Google Sheets

| Tool | Required params | Notes |
|---|---|---|
| `GOOGLESHEETS_SPREADSHEETS_VALUES_APPEND` | `spreadsheetId`, `range`, `valueInputOption`, `values` | Appends rows after the last row with data in `range`. `spreadsheetId` must be a real sheet the tenant designates — never invented. `valueInputOption` should be `"USER_ENTERED"` for values that should behave like typed input (dates, numbers) rather than literal strings. |
| `GOOGLESHEETS_SEARCH_SPREADSHEETS` | *(none required)* | Use to help a tenant pick a `spreadsheetId` by name instead of asking them to paste an ID. |

## Things this skill deliberately does NOT depend on

- `claude-office-skills/skills`' `notion-automation` and `slack-workflows`
  skills were reviewed as design references (fetched and read directly,
  not via a summarizing tool, after the WebFetch hallucination caught
  earlier this session) — they are n8n workflow *pattern* libraries
  (`mcp: server: notion-mcp` / `slack-mcp`, tool names like
  `notion_create_page`), not directly callable against Aivory's actual
  Composio-REST integration. Useful for workflow-shape ideas (e.g. the
  "Slack reaction → Notion task" pattern), not copy-pasteable.
- `claude-office-skills/skills`' `meeting-notes` skill assumes an
  `office-mcp` server with a `create_docx` tool (Word document generation).
  Aivory's delivery surface is chat (Telegram/Slack/WhatsApp), not
  downloadable documents, so this dependency is dropped; only its
  extraction *templates and guidance* (action-item/decision/owner
  heuristics) were adapted into this skill's Instructions §1.
