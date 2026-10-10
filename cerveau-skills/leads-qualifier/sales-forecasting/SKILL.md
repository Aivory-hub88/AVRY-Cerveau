---
name: sales-forecasting
description: Build sales funnel metrics and a forward forecast from the business's own weekly numbers — bleed, hold, presentation, quotation and close rates, revenue and cost per job or per unit (e.g. roofing squares), profit margin, projected leads, jobs and revenue for the coming weeks, how many leads are needed to close X jobs per week, and expected closes from open quotes. Use when the user asks for a sales forecast, conversion or close rates, how many leads they need, where sales are heading, or a weekly sales/metrics review. Not for totalling the current pipeline by stage (that is pipeline_summary).
license: MIT
author: aivory
version: 0.1.0
category: sales
tags: [forecasting, funnel, metrics, odoo, tenant-scoped]
---

# Sales Forecasting

Turns weekly funnel counts into rates, unit economics and a forecast. The
arithmetic is done by the `sales_funnel_forecast` tool, never by you: chained
rates over many weeks are easy to get wrong, and the user may plan staffing
and spend around the result. Your job is to collect correct counts, call the
tool, and explain what it returns.

## Instructions

### 1. Agree on what is being measured

Funnel stages the tool understands, in order:

| Stage | Meaning | Rate it produces |
|---|---|---|
| `leads` | New leads that came in that week | — |
| `appointments_set` | Appointments booked | `set_rate`, and `bleed_rate` = leads that never got an appointment |
| `appointments_held` | Appointments that actually happened | `hold_rate` |
| `presentations` | Full presentations given | `presentation_rate` |
| `quotes` | Quotes/estimates issued | `quotation_rate` |
| `closes` | Jobs sold | `close_rate` |

Plus optional money fields per week: `revenue` (sold value), `units`
(e.g. squares, square metres, hours) and `cost`.

Only `leads` and `closes` are required. Skip any stage the business does not
track; never fill one in with an estimate. If the user uses different words
(e.g. "inspections" for held appointments), confirm the mapping once, then
save it with `graph_remember` so the next forecast does not ask again.
Check `graph_recall` for a saved mapping before asking.

### 2. Collect the weekly counts

Aim for 8–12 weeks, oldest first. Four or more weeks lets the tool apply a
trend; fewer gives a flat projection (the tool warns about this).

**Fix the week window before pulling anything.** Weeks run Monday to Sunday.
Use only complete weeks: the window ends on the Sunday before the current
week and starts N Mondays earlier (for 12 weeks on Saturday 2026-10-10, that
is Monday 2026-07-13 to Sunday 2026-10-04, ISO weeks 29–40). Leave the
current week out; a half-finished week drags the trend down. Write the list
of N weeks out first, then fill it in:
- A source that groups by week (Odoo `read_group`) returns no row for a week
  with no records. That week is a real zero, not missing data: keep it in the
  list with 0 for every stage. Dropping it shortens the window and hides a
  slowdown, which is exactly what a forecast should show.
- Match each returned group to its week by the week start date in the
  group's value for the groupby key (e.g. `2026-07-13`), not by parsing
  a label such as "W29 2026".
- Pass exactly N week objects to the tool. If the business has less history
  than asked for (the first records start partway through the window), use
  what exists and say how many weeks the forecast is built on.

**If the tenant has Odoo connected** (Od-MCP tools), count per week with
`odoo_read_group`, grouping by the date field with a `:week` suffix. Typical
mapping on a standard Odoo CRM + Sales setup:

- `leads`: `crm.lead`, group by `create_date:week`, `["type", "in", ["lead", "opportunity"]]`. Include archived lost leads with `["active", "in", [true, false]]`, or the bleed rate will look better than it is.
- `appointments_set` / `appointments_held`: `calendar.event` linked to an opportunity (`["opportunity_id", "!=", false]`), group by `start:week`. Held vs set needs a field or tag the business uses for no-shows; ask if it is not obvious.
- `quotes`: `sale.order`, group by `date_order:week` (the quotation date), `["state", "!=", "cancel"]`, plus `["opportunity_id", "!=", false]` if they only want quotes from the CRM pipeline. Avoid `create_date` on orders: imported or migrated orders all carry the import date.
- `closes` and `revenue`: `sale.order` with `["state", "=", "sale"]`, group by `date_order:week`, sum `amount_untaxed`. One call returns both the count and the sum.
- `units`: sum `product_uom_qty` on `sale.order.line` of confirmed orders, filtered to the product(s) sold per unit (e.g. roofing per square). Ask which product or unit of measure is the unit.
- `cost`: the `margin` field on `sale.order` (cost = `amount_untaxed - margin`) when the sale-margin module is installed; otherwise ask how they track job cost.

Every business configures Odoo differently. Before trusting a mapping, check
stage names and fields with `odoo_get_model_metadata` or a small
`odoo_search_read`, and tell the user which models and filters you used.

**Stay within the call budget.** The runtime stops a turn after about nine
calls to the same tool, before you reach the forecast. Plan for at most six
Od-MCP calls in total, one per stage, then call `sales_funnel_forecast` in
the same turn:
- `odoo_read_group` arguments: the date bucket goes ONLY in `groupby`
  (`["date_order:week"]`); `fields` holds only aggregates written as
  `field:agg` (e.g. `["amount_untaxed:sum"]`), and the count comes back on its
  own. Never repeat `date_order:week` in `fields`: on Odoo 19 that is read as
  an aggregate and every call fails.
- Use a single `odoo_read_group` per model and date field, covering the
  whole window with a closed date domain, e.g.
  `["date_order", ">=", "2026-07-13"], ["date_order", "<", "2026-10-05"]`
  (window start, and the Monday of the current week); never one call per
  week.
- Check metadata only for a field you are unsure exists (e.g. `margin`), and
  only once. If a mapping you saved earlier is in `graph_recall`, skip the
  checks.
- If a stage turns out to be empty or unusable (e.g. almost no
  `calendar.event` linked to opportunities), drop that stage and carry on
  with the ones you have. Do not keep probing. Tell the user in your answer.
- If you still run short, stop pulling, run the forecast on the stages you
  have, and offer to add the rest next turn.

**If there is no connected CRM/ERP**, or a stage cannot be found in it, ask
the user for the weekly numbers directly. A pasted table or spreadsheet is
fine; repeat the parsed numbers back before calling the tool.

Make sure every week uses the same date basis. If a later stage is larger
than the stage before it in some week, the tool flags it; usually the counts
were taken with different date filters. Fix the filter rather than ignoring
the warning.

### 3. Call `sales_funnel_forecast`

- `weeks`: one object per week, oldest first, the same stages in every week.
- `horizon_weeks`: how far ahead (default 4, max 12).
- `target_closes_per_week`: when the user asks "how many leads do I need to close X jobs".
- `open_quotes` (`count`, optional `value`) and `follow_up_close_rate`: when they want quotes already out included. If they track a follow-up or second-connect close rate, pass it as `follow_up_close_rate` (0–1); otherwise the historical close rate is used.
- `currency` (ISO code, ask if unknown, never assume) and `units_label` (e.g. "squares").
- `weighting`: leave as `recent` unless the user asks for a plain average.
- `rate_overrides`: only for a what-if (see step 5).

### 4. Report the result

Every number in your answer comes from a tool result: the weekly rows you
passed in, or a field the forecast returned. Do no arithmetic of your own,
not even a column total. For the total of the history table, quote
`history_totals`; if a total you want is not in the result, leave it out.
When you list the Odoo models and filters you used, copy the domain from the
calls you actually made, not from this skill's suggested mapping.

Lead with the answer to what they asked, then the supporting numbers:

1. Projected jobs and revenue for the horizon (with the low–high range when returned).
2. The current rates, and which step is weakest compared with the others.
3. If a target was given: leads needed per week and the gap from today's lead flow.
4. Expected closes from open quotes, if requested.
5. The tool's `warnings` and `assumptions`, in plain words. Always mention that the forecast assumes this week's leads convert within the forecast window (there is no lag model).

End with one concrete next action, usually the stage with the biggest drop-off.

Never put a number on a what-if ("raising the hold rate to 85% adds N jobs")
unless that number came from a `sales_funnel_forecast` call with
`rate_overrides`. Mental estimates of chained rates are routinely off by an
order of magnitude. If you did not make that call, name the weak stage
without a figure and offer to run the scenario.

### 5. What-if scenarios

To answer "what if the hold rate were 85%?", call the tool again with the
SAME weeks and `rate_overrides: {"hold_rate": 0.85}`. Overridable rates:
`set_rate`, `bleed_rate`, `hold_rate`, `presentation_rate`,
`quotation_rate`, `close_rate`; several can be combined in one call. The
result keeps the baseline and adds a `scenario` block (projection, totals,
`change_vs_baseline`, and the leads needed for the target at the new rates).

Never simulate a scenario by editing the weekly counts. Raising
`appointments_held` alone also lowers the presentation rate, so closes come
out unchanged and the scenario looks worthless when it is not.

Keep the weekly table you used in the conversation (repeat it back once
before the first call), so a follow-up scenario can reuse it without asking
the user to paste it again.

## Examples

**"What will we sell next month?"** → recall the saved stage mapping → read 12
weeks from Odoo → call the tool with `horizon_weeks: 4` → report projected jobs,
revenue and range, plus warnings.

**"How many leads do I need to close 15 jobs a week?"** → collect the weeks →
call with `target_closes_per_week: 15` → report required leads per week, per
stage, and the gap.

**User pastes this week's numbers only** → one week cannot produce a rate
trend; ask for at least the previous three weeks, or look them up if a CRM is
connected.

## Out of scope

- Writing anything back to the CRM/ERP: this skill only reads.
- Per-deal win probabilities and the current pipeline total (use `pipeline_summary`).
- Currency conversion: keep each currency in its own forecast.
- Seasonality and lead-to-close lag modelling: say so if the user's business is strongly seasonal.
