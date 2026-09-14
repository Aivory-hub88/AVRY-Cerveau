# Aivory Cerveau live overlay — product-agent personas + room coordination
#
# This directory is the version-controlled home of the LIVE Cerveau-side
# artifacts for Aivory's 5 deployable agents (Geno/Teo/Lex/Finn/Ofira).
# The daemon reads these from `/home/ubuntu/.zeroclaw-cerveau/agents/` on
# the VPS; this repo copy is the source of truth, the VPS is the target.
#
# Layout (mirrors the daemon's runtime paths 1:1):
#   agents/<type>/workspace/IDENTITY.md  — persona per product agent,
#                                          incl. §6 Mission Control Room
#   ROOM-DELEGATES.md                    — delegates canary state, cost
#                                          observations, rollback procedure
#   sync.sh                              — deploy/capture/status for personas
#
# Full skills + root identity/soul sync lives in the platform entry repo
# (services/cerveau/sync.sh) alongside a mirror of these personas; if both
# copies ever disagree, THIS directory wins for Cerveau-side files.
#
# Deploy flow (from this dir):
#   CERVEAU_HOST=aivory-prod CERVEAU_SUDO="sudo -n" ./sync.sh status
#   CERVEAU_HOST=aivory-prod CERVEAU_SUDO="sudo -n" ./sync.sh deploy
# deploy restarts zeroclaw-cerveau (seconds of 503s on agent-chat).
# Rollback: restore the `.bak-pre-*` file on the VPS (see ROOM-DELEGATES.md).
