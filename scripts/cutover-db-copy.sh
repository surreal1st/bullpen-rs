#!/usr/bin/env bash
# S14: copy live TS bullpen.db into rust BULLPEN_DATA_DIR (meridian recipe).
# Default is DRY-RUN (prints plan + source hash only). Set CUTOVER_DB_COPY=1 to apply.
#
# Run ON meridian as root, from a checkout or copied script:
#   sudo bash scripts/cutover-db-copy.sh
#
# Never touches bullpen.service (:4360). Stops bullpen-rs.service briefly when applying.
set -euo pipefail

LIVE_DB="${BULLPEN_LIVE_DB:-/home/bullpen/data/bullpen.db}"
RS_HOME="${BULLPEN_RS_HOME:-/home/bullpen/bullpen-rs}"
DEST_DB="${BULLPEN_RS_DB:-${RS_HOME}/bullpen.db}"
APPLY="${CUTOVER_DB_COPY:-0}"
TS="$(date -u +%Y%m%d-%H%M%S)"
BACKUP_DIR="${RS_HOME}/backups/before-cutover-${TS}"

log() { printf '%s\n' "$*"; }
die() { log "ERROR: $*" >&2; exit 1; }

if [[ "${APPLY}" != "0" && "${APPLY}" != "1" ]]; then
  die "CUTOVER_DB_COPY must be 0 or 1 (got ${APPLY})"
fi

if [[ "${APPLY}" == "1" && "${EUID}" -ne 0 ]]; then
  die "Apply mode requires root (sudo). Dry-run can run as any user with read access."
fi

if [[ ! -f "${LIVE_DB}" ]]; then
  die "Live DB not found: ${LIVE_DB}"
fi

log "Live source:  ${LIVE_DB}"
log "Rust dest:    ${DEST_DB}"
log "Mode:         $([[ "${APPLY}" == "1" ]] && echo APPLY || echo DRY-RUN)"

log ""
log "Source sha256:"
LIVE_SHA="$(sha256sum "${LIVE_DB}" | awk '{print $1}')"
log "  ${LIVE_SHA}  ${LIVE_DB}"

if [[ -f "${DEST_DB}" ]]; then
  DEST_SHA="$(sha256sum "${DEST_DB}" | awk '{print $1}')"
  log ""
  log "Current rust DB sha256 (will be backed up before overwrite):"
  log "  ${DEST_SHA}  ${DEST_DB}"
else
  log ""
  log "No existing rust DB at dest (first install copy)."
fi

log ""
log "Plan:"
log "  1. systemctl stop bullpen-rs.service   # rust only; NOT bullpen.service"
log "  2. mkdir -p ${BACKUP_DIR}"
if [[ -f "${DEST_DB}" ]]; then
  log "  3. cp -a ${DEST_DB} ${BACKUP_DIR}/bullpen.db"
fi
log "  4. cp -a ${LIVE_DB} ${DEST_DB}"
log "  5. chown bullpen:bullpen ${DEST_DB}"
log "  6. sha256sum dest (must equal source ${LIVE_SHA})"
log "  7. systemctl start bullpen-rs.service"
log ""
log "Handoff §8 / docs/s14-cutover: verify BOTH hashes after any cross-machine move."

if [[ "${APPLY}" != "1" ]]; then
  log ""
  log "DRY-RUN complete. To apply on meridian: CUTOVER_DB_COPY=1 sudo bash scripts/cutover-db-copy.sh"
  exit 0
fi

log ""
log "Applying copy..."

systemctl stop bullpen-rs.service
mkdir -p "${BACKUP_DIR}"
if [[ -f "${DEST_DB}" ]]; then
  cp -a "${DEST_DB}" "${BACKUP_DIR}/bullpen.db"
  sha256sum "${BACKUP_DIR}/bullpen.db" > "${BACKUP_DIR}/CHECKSUMS"
fi

cp -a "${LIVE_DB}" "${DEST_DB}"
chown bullpen:bullpen "${DEST_DB}"
NEW_SHA="$(sha256sum "${DEST_DB}" | awk '{print $1}')"
if [[ "${NEW_SHA}" != "${LIVE_SHA}" ]]; then
  die "Hash mismatch after copy (dest ${NEW_SHA} != source ${LIVE_SHA})"
fi

echo "${NEW_SHA}  bullpen.db" > "${BACKUP_DIR}/CHECKSUMS.after"
systemctl start bullpen-rs.service

log "OK: rust DB replaced; backup at ${BACKUP_DIR}"
log "  verified sha256: ${NEW_SHA}"
