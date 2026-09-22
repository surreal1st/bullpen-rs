#!/usr/bin/env bash
# S14: extend cutover-check with optional signed-in API smoke (no browser).
# Never touches :4360.
#
# Unsigned checks always run (via cutover-check.sh).
# Signed-in checks run only when a password is supplied WITHOUT printing it:
#   CUTOVER_SMOKE_PASSWORD=...   or   BULLPEN_PASSWORD_FILE=/path/to/file
#
# Josh manual smoke (send message, approval UI) still required for cutover — see RESULT doc.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export BULLPEN_URL="${BULLPEN_URL:-http://127.0.0.1:4380}"
# shellcheck source=/dev/null
bash "${ROOT}/scripts/cutover-check.sh"

BASE="${BULLPEN_URL}"
BASE="${BASE%/}"
JAR="$(mktemp)"
trap 'rm -f "${JAR}"' EXIT

read_password() {
  if [[ -n "${CUTOVER_SMOKE_PASSWORD:-}" ]]; then
    printf '%s' "${CUTOVER_SMOKE_PASSWORD}"
    return 0
  fi
  if [[ -n "${BULLPEN_PASSWORD_FILE:-}" && -f "${BULLPEN_PASSWORD_FILE}" ]]; then
    tr -d '\r\n' < "${BULLPEN_PASSWORD_FILE}"
    return 0
  fi
  return 1
}

if ! PW="$(read_password)"; then
  echo "SKIP: signed-in smoke (set CUTOVER_SMOKE_PASSWORD or BULLPEN_PASSWORD_FILE)"
  echo "Manual: sign in at ${BASE}/, send a message, exercise approval, read spend — docs/s14-cutover.md"
  exit 0
fi

login_code="$(curl -s -o /tmp/bullpen-cutover-login.json -w '%{http_code}' \
  -c "${JAR}" -b "${JAR}" \
  -H 'Content-Type: application/json' \
  -d "$(python3 -c "import json,sys; print(json.dumps({'password': sys.stdin.read()}))" <<<"${PW}")" \
  "${BASE}/api/auth/login")"

if [[ "${login_code}" != "200" ]]; then
  echo "FAIL: POST /api/auth/login returned ${login_code} (not printing body — may contain hints)" >&2
  exit 1
fi

for path in /api/auth/status /api/roster /api/spend /api/approvals /api/library; do
  code="$(curl -s -o /dev/null -w '%{http_code}' -b "${JAR}" "${BASE}${path}")"
  if [[ "${code}" != "200" ]]; then
    echo "FAIL: ${path} returned ${code} after login (expected 200)" >&2
    exit 1
  fi
done

grep -q '"signedIn":true' <<<"$(curl -fsS -b "${JAR}" "${BASE}/api/auth/status")"

echo "OK: signed-in smoke — auth/status, roster, spend, approvals, library"
