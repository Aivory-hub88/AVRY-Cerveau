#!/usr/bin/env bash
# =============================================================================
# Cerveau (ZeroClaw) skills sync — deploy/capture the daemon's skills + identity
# between this repo and the live VPS.
#
# The ZeroClaw "Cerveau" daemon (zeroclaw-cerveau.service) maps the frontend's
# `entrypoint` values to SKILL.md files by directory name under
#   /home/ubuntu/.zeroclaw-cerveau/skills/<entrypoint>/SKILL.md
# These files are NOT in any git repo by default — they live only on the VPS.
# This script keeps them version-controlled here and reproducible.
#
# Usage (run from services/cerveau/):
#   ./sync.sh deploy    # push local skills + identity + agent personas to the VPS, restart daemon
#   ./sync.sh capture   # pull current VPS skills + identity + agent personas into this repo
#   ./sync.sh status    # diff local vs VPS (no changes)
#
# Product-agent personas (agents/<type>/workspace/IDENTITY.md) are the
# enterprise source of truth for the 5 deployable agents (Geno/Teo/Lex/Finn/
# Ofira) — including §6 Mission Control Room team coordination. VPS files are
# ubuntu-owned, so remote ops run under sudo -n (passwordless sudo required).
# =============================================================================

set -euo pipefail

HOST="${CERVAU_HOST:-tencent-vps}"
REMOTE_DIR="/home/ubuntu/.zeroclaw-cerveau"
LOCAL_DIR="$(cd "$(dirname "$0")" && pwd)"
# Scoped to product agents only — internal *_brain workspaces are managed
# by Cerveau's own sync-aivory-identity.sh on the VPS, not here.
PRODUCT_AGENTS="autonomous customer_service leads_qualifier finance_invoice_ops office_assistant chief_of_staff"
# Remote files are ubuntu-owned. Operators with direct write access leave
# CERVEAU_SUDO unset (legacy behavior). Operators with passwordless sudo
# instead (e.g. irfan via aivory-prod) export CERVEAU_SUDO="sudo -n".
SUDO_PREFIX="${CERVEAU_SUDO:+$CERVEAU_SUDO }"
RSYNC_PATH_ARGS=()
if [ -n "${CERVEAU_SUDO:-}" ]; then RSYNC_PATH_ARGS=(--rsync-path="sudo -n rsync"); fi

log()  { echo -e "\033[0;36m[sync]\033[0m $*"; }
warn() { echo -e "\033[1;33m[!]\033[0m $*"; }

case "${1:-}" in
  deploy)
    log "Deploying skills + identity → ${HOST}:${REMOTE_DIR}"
    rsync -az "${RSYNC_PATH_ARGS[@]}" --delete \
      --exclude='._*' --exclude='.DS_Store' \
      "${LOCAL_DIR}/skills/"       "${HOST}:${REMOTE_DIR}/skills/"
    rsync -az "${RSYNC_PATH_ARGS[@]}" "${LOCAL_DIR}/identity.md" "${LOCAL_DIR}/soul.md" "${HOST}:${REMOTE_DIR}/"
    log "Deploying product-agent personas → ${HOST}:${REMOTE_DIR}/agents/"
    for a in ${PRODUCT_AGENTS}; do
      tar -czf - -C "${LOCAL_DIR}/agents" "${a}/workspace/IDENTITY.md" \
        | ssh "${HOST}" "${SUDO_PREFIX}tar -xzf - -C ${REMOTE_DIR}/agents && ${SUDO_PREFIX}chown ubuntu:ubuntu ${REMOTE_DIR}/agents/${a}/workspace/IDENTITY.md"
    done
    log "Restarting zeroclaw-cerveau to load skills..."
    ssh "${HOST}" "sudo systemctl restart zeroclaw-cerveau.service && sleep 2 && systemctl is-active zeroclaw-cerveau.service"
    log "Done."
    ;;
  capture)
    log "Capturing skills + identity from ${HOST}"
    ssh "${HOST}" "cd ${REMOTE_DIR} && tar --exclude='._*' -czf - skills identity.md soul.md" \
      | tar -xzf - -C "${LOCAL_DIR}"
    log "Capturing product-agent personas from ${HOST}"
    ssh "${HOST}" "${SUDO_PREFIX}tar -czf - -C ${REMOTE_DIR}/agents $(for a in ${PRODUCT_AGENTS}; do printf '%s ' ${a}/workspace/IDENTITY.md; done)" \
      | tar -xzf - -C "${LOCAL_DIR}/agents"
    log "Captured. Review with: git -C ${LOCAL_DIR}/.. diff"
    ;;
  status)
    log "Diffing local vs VPS (dry run)..."
    rsync -azn "${RSYNC_PATH_ARGS[@]}" --delete --exclude='._*' --exclude='.DS_Store' \
      "${LOCAL_DIR}/skills/" "${HOST}:${REMOTE_DIR}/skills/"
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
