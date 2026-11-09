---
name: weekly-report
description: Build the weekly or monthly business report in one pass — one kpi_snapshot call for leads, invoices and tickets with the previous period for comparison, optional ERP or CRM reads for what Aivory does not hold, anomaly flags, and the fixed report layout. Use when the user asks for a weekly report, monthly report, business summary, KPI review, "how are we doing" or a report for the owner.
license: MIT
author: aivory
version: 0.1.0
category: analytics
tags: [report, kpi, anomalies, tenant-scoped]
---

# Weekly report

Target: correct, sourced, in at most 4 model steps. Do not wander.

## Steps

1. **Period.** Use the dates the user gave. With none, call `kpi_snapshot` with
   no dates (last 7 full days, UTC) and state the dates it returns. Ask one
   question only if the dates are truly ambiguous ("last month" is not).
2. **One call.** `kpi_snapshot` returns leads, invoices and tickets for the
   period and the previous period of the same length, each metric with its
   source. Do not call it again per section.
3. **Other systems, only if connected.** If the business keeps its books in
   Odoo, ERPNext or HubSpot, read just what the report needs (for example
   revenue and open receivables) and name the system. If they disagree with
   Aivory's figures, show both and say which is which; do not merge them. Skip
   this step when no such system is connected, and say it was skipped.
4. **Forecast only when asked.** Use `sales_funnel_forecast`; its arithmetic is
   its own. Do not do rate math yourself.
5. **Write the report** in the layout below. Plain text, short.

## Layout

```
Report: <business or subject>
<period start> to <period end> (<n> days, UTC). Compared with <previous start> to <previous end>.

The one thing
<one sentence: what changed most, or that nothing did>

Findings
- <metric>: <value> (<change vs previous>) [source]
...

Do these first
1. <action, who owns it>

What is working
- <one or two items, only if the numbers support it>

What I could not determine
- <gaps: not stored, no connected system, sample too small, partial period>
```

## Anomaly flags (rules of thumb, say they are)

Raise a flag, with the numbers, when any of these hold. Do not raise a flag
on a base smaller than 5 records.

- Tickets opened up by 50 percent or more against the previous period, and at
  least 5 more tickets.
- Invoiced total down by 20 percent or more in any currency.
- Any receivable more than 60 days past due.
- Close rate that moved by 15 points or more, but only if both periods have 30
  or more decided deals; otherwise say it is directional only.

For each flag, send a handoff packet to the owner (see analyst-rules) and say
in the report that you did. Do not retarget campaigns, chase customers or
change records yourself.

## Saving

Only save the report to Sheets, Docs or PDF when the user asks. Saving goes
through approval: say "awaiting approval" until it is approved, then give the
link. Send the Aira briefing packet only after the file exists.

## Do not

- Estimate a missing figure or fill a gap with last period's number.
- Sum different currencies.
- Present updated_at-based figures (closed, paid) as exact; they follow the
  record's last edit, as the snapshot's caveats say.
- Call `kpi_snapshot` repeatedly or search Drive for data to "check" it.
