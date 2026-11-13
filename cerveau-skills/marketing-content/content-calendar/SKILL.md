---
name: content-calendar
description: Build a monthly content calendar from a short brief (goals, products, promos, dates) as a new Google Sheet — theme per week, one row per post with channel, format, hook, call to action and tracking link, in the business's language. Use when the user asks for a content calendar, posting plan, editorial calendar, or a month of content.
license: MIT
author: aivory
version: 0.1.0
category: marketing
tags: [calendar, planning, sheets, tenant-scoped]
---

# Content calendar

Target: a usable calendar in few steps, drafted only.

## Steps

1. **Brief.** You need the month, the goal (awareness, leads, a launch), the products or promos, the channels the business actually uses, how often they can post, and any fixed dates (holidays, launches). Ask one short question for what is missing; do not assume channels.
2. **Business context.** Read the voice, proof and never-say list (see content-rules). Check `graph_recall` for what past campaigns taught.
3. **Plan the weeks.** One theme per week tied to the goal. Spread formats (education, proof, offer, behind the scenes); put the offer posts where a promo or date supports them. Respect local dates and holidays only when the user named the market.
4. **One row per post.** Columns: Date, Channel, Format, Theme, Hook, Draft status (`planned`), Call to action, Tracking link, Proof needed. Use `campaign_links` once for the whole calendar for the tracking links (one call, up to 30 links; split bigger calendars). A post with no link target gets an empty cell, not a made-up URL.
5. **Create the Sheet** as a new file for the owner (creating a new Sheet is allowed; editing an existing one is not without approval). Give the link and say `drafted`.
6. **Offer drafts**, do not write all of them unprompted: copy for the first week is plenty for a first review.

## Report

```
Calendar: <business>, <month>
Goal: <one line>. Channels: <list>. Posts: <n> (<per week>).

The one thing
<what this month is built around>

Weeks
- Week 1: <theme>, <n> posts ...

Needs your input
- <proof or detail missing, as [NEED: ...]>

What I could not determine
- <channels not confirmed, dates not given, claims without proof>
```

## Do not

- Invent events, discounts, prices, stock levels or dates the user did not give.
- Schedule or post anything. The calendar is a plan.
- Plan on a channel the business does not use.
