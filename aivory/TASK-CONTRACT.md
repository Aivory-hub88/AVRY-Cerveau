# Phase 3B Task Contract — Aira orchestration ledger

No schema migration. The zeroclaw task tools (`task_create`,
`task_update_status`, `task_list`) accept only title, status, priority, and
blocked reason, so the contract is convention on `cerveau.agent_tasks` plus
derivation in `GET /api/aira/tasks` (see `lib/airaTasks.ts`).

## Row roles

- Parent: the earliest `chief_of_staff` row in a room `session_id`.
  Aira titles it `AIRA-orch-<UTC HHMM>: <objective>`.
- Children: every other `agent_type` row in the same session. Specialists
  update only their own rows, never the parent.

## Status vocabulary

`todo` = planned, `in_progress` = running, `blocked` = waiting (blocked
reason names the approver or missing input — approval waits live here),
`done` = delivered and verified against the `Done when` criteria.

## Timeout (derived, human-enforced)

- Child SLA: 15 minutes from creation.
- Parent SLA: 60 minutes from creation.
- The API flags `overdue: true` and reports `elapsed_ms`; nothing
  auto-cancels. Enforcement stays human-driven, consistent with the
  approval-gated delegation posture.

## Rollback

No schema to revert. To pause the contract, stop Aira creating parent rows
(identity edit + daemon restart); existing rows keep rendering as flat
kanban cards. Delete test rows with
`DELETE FROM cerveau.agent_tasks WHERE tenant_id = '<probe-tenant>'`.
