#!/usr/bin/env bash
# =============================================================================
# Product-agent persona sync — this repo (aivory/agents/) <-> live VPS
# (/home/ubuntu/.zeroclaw-cerveau/agents/). Scoped to the 5 deployable
# agents only; internal *_brain workspaces are managed by Cerveau's own
# sync-aivory-identity.sh on the VPS, not here.
#
# Usage (run from aivory/):
#   CERVEAU_HOST=aivory-prod CERVEAU_SUDO="sudo -n" ./sync.sh status
#   CERVEAU_HOST=aivory-prod CERVEAU_SUDO="sudo -n" ./sync.sh deploy
#   CERVEAU_HOST=aivory-prod CERVEAU_SUDO="sudo -n" ./sync.sh capture
# CERVEAU_SUDO: set to "sudo -n" when the SSH user needs passwordless sudo
# for ubuntu-owned remote files; leave unset with direct write access.
# deploy restarts zeroclaw-cerveau to reload personas.
# =============================================================================

set -euo pipefail

HOST="${CERVAU_HOST:-aivory-prod}"
REMOTE_DIR="/home/ubuntu/.zeroclaw-cerveau"
LOCAL_DIR="$(cd "$(dirname "$0")" && pwd)"
PRODUCT_AGENTS="autonomous customer_service leads_qualifier finance_invoice_ops office_assistant"
SUDO_PREFIX="${CERVEAU_SUDO:+$CERVEAU_SUDO }"

log()  { echo -e "\033[0;36m[sync]\033[0m $*"; }
warn() { echo -e "\033[1;33m[!]\033[0m $*"; }

case "${1:-}" in
  deploy)
    log "Deploying product-agent personas → ${HOST}:${REMOTE_DIR}/agents/"
    for a in ${PRODUCT_AGENTS}; do
      tar -czf - -C "${LOCAL_DIR}/agents" "${a}/workspace/IDENTITY.md" \
        | ssh "${HOST}" "${SUDO_PREFIX}tar -xzf - -C ${REMOTE_DIR}/agents && ${SUDO_PREFIX}chown ubuntu:ubuntu ${REMOTE_DIR}/agents/${a}/workspace/IDENTITY.md"
    done
    log "Restarting zeroclaw-cerveau to reload personas..."
    ssh "${HOST}" "${SUDO_PREFIX}systemctl restart zeroclaw-cerveau.service && sleep 2 && systemctl is-active zeroclaw-cerveau.service"
    log "Done."
    ;;
  capture)
    log "Capturing product-agent personas from ${HOST}"
    ssh "${HOST}" "${SUDO_PREFIX}tar -czf - -C ${REMOTE_DIR}/agents $(for a in ${PRODUCT_AGENTS}; do printf '%s ' ${a}/workspace/IDENTITY.md; done)" \
      | tar -xzf - -C "${LOCAL_DIR}/agents"
    log "Captured. Review with git diff."
    ;;
  status)
    log "Diffing local vs VPS (dry run)..."
    for a in ${PRODUCT_AGENTS}; do
      ssh "${HOST}" "${SUDO_PREFIX}cat ${REMOTE_DIR}/agents/${a}/workspace/IDENTITY.md" \
        | diff -q - "${LOCAL_DIR}/agents/${a}/workspace/IDENTITY.md" > /dev/null \
        && log "  agents/${a}: in sync" \
        || warn "  agents/${a}: DIFFERS"
    done
    warn "Use './sync.sh deploy' to push, './sync.sh capture' to pull."
    ;;
  *)
    echo "Usage: $0 {deploy|capture|status}" >&2
    exit 1
    ;;
esac
