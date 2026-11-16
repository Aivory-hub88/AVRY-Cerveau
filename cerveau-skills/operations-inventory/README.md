# operations-inventory bundle (Juno)

- `ops-rules` (always inline): sourcing, no invented inputs, draft-only, approval wording, handoff packets.
- `stock-check`: the daily/weekly stock check recipe and reorder suggestions.
- `purchase-order`: drafting a purchase order and following up late receipts.

Deployed to `/home/ubuntu/.zeroclaw-cerveau/cerveau-skills/operations-inventory/`; wired in
`[skill_bundles.operations-inventory]` (see AVRY-V2-Main `ops/cerveau-config`). Runtime profile
`agent_operations_inventory` is `compact`, so only `ops-rules` is inlined and the rest is read on demand.
