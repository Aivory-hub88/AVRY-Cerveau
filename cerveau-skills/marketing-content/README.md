# marketing-content bundle (Mira)

- `content-rules` (always inline): voice from the business, no invented claims, draft-only, tracked links, handoff.
- `slop-catalogue`: phrases that fail a draft; two hits means rewrite.
- `content-calendar`: a month calendar from a short brief, as a Google Sheet.
- `channel-drafts`: formats and limits per channel, one draft per channel.
- `campaign-brief`: campaign record, tracked links, the packet to Lex, and the review with Ivo.

Deployed to `/home/ubuntu/.zeroclaw-cerveau/cerveau-skills/marketing-content/`; wired in
`[skill_bundles.marketing-content]` (see AVRY-V2-Main `ops/cerveau-config`). Runtime profile
`agent_marketing_content` is `compact`, so only `content-rules` is inlined and the rest is read on demand.
