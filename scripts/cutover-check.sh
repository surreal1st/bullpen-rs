#!/usr/bin/env bash
# S14: smoke checks against rust Bullpen (:4380). Never touches :4360.
set -euo pipefail

BASE="${BULLPEN_URL:-http://127.0.0.1:4380}"
BASE="${BASE%/}"

echo "Cutover check against ${BASE}"

curl -fsS "${BASE}/api/health" >/dev/null
curl -fsS "${BASE}/api/version" | grep -q '"version"'

# Auth gate: roster without session should not succeed with 200 JSON roster.
code="$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/api/roster")"
if [[ "${code}" == "200" ]]; then
  echo "FAIL: /api/roster returned 200 without auth (expected 401/503)" >&2
  exit 1
fi

curl -fsS "${BASE}/api/push" | grep -q '"configured"'

echo "OK: health, version, auth gate, push describe"
