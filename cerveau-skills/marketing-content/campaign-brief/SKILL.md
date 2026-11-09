---
name: campaign-brief
description: Turn a campaign idea into a tracked campaign — brief, channel plan, tagged links from campaign_links, the campaign packet to Lex so he knows what leads to expect, and later a review with Ivo that is honest about sample size and attribution. Use when the user asks to plan or launch a campaign, track a campaign, hand a campaign to sales, or review how a campaign did.
license: MIT
author: aivory
version: 0.1.0
category: marketing
tags: [campaign, utm, handoff, attribution]
---

# Campaign brief

## Plan

1. **Brief in five lines:** goal, audience, offer, channels, dates. Ask one short question for what is missing.
2. **Claims:** list every factual claim with its proof; unproven ones become `[NEED: proof]` and do not go into copy.
3. **Links:** call `campaign_links` once with the campaign name and every landing URL by channel (use `content` to tell two links on one channel apart). Give the user the tagged links and any refusals with their reasons.
4. **Save** the brief as a new Google Doc for the owner (allowed) and say `drafted`.

## Hand to Lex

Only after the owner says the campaign is approved to launch, send Lex a `campaign`
packet (see content-rules): campaign, channels, the `packet_utm` lines from the tool,
launch date, audience, claims. Say what he should expect: which leads come from this
campaign, and how to ask them where they came from if the CRM does not capture the tags.

## Review (with Ivo)

When asked how a campaign did, ask Ivo (`team_ask`) for the leads and outcomes by the
campaign's utm values, with the period. Report:

- the numbers as Ivo gave them, with his sources;
- "directional only" when fewer than 30 leads are behind a rate; no winning channel from a handful;
- old and new campaigns kept apart;
- what cannot be known: leads that arrived without tags, offline conversions, overlapping campaigns.

Never estimate results Ivo did not give you.

## Do not

- Tell Lex a campaign is live before it is approved.
- Type a UTM link by hand or reuse a link from another campaign.
- Present attribution as certain when the form or CRM does not record the tags.
