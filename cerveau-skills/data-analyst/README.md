# data-analyst bundle (Ivo)

- `analyst-rules` (always inline): sourcing, fast refusal, sample honesty, currencies, handoff packet.
- `weekly-report`: the report recipe and anomaly flags.

Deployed to `/home/ubuntu/.zeroclaw-cerveau/cerveau-skills/data-analyst/`; wired in
`[skill_bundles.data-analyst]` (see AVRY-V2-Main `ops/cerveau-config`). Runtime profile
`agent_data_analyst` is `compact`, so only `analyst-rules` is inlined and the rest is read on demand.
