#!/usr/bin/env bash
# S14: smoke checks against rust Bullpen (:4380). Never touches :4360.
set -euo pipefail

BASE="${BULLPEN_URL:-http://127.0.0.1:4380}"
BASE="${BASE%/}"

echo "Cutover check against ${BASE}"

curl -fsS "${BASE}/api/health" >/dev/null
curl -fsS "${BASE}/api/version" | grep -qE '"version"|"commit"|newestKnown'

# Auth gate: roster without session should not succeed with 200 JSON roster.
code="$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/api/roster")"
if [[ "${code}" == "200" ]]; then
  echo "FAIL: /api/roster returned 200 without auth (expected 401/503)" >&2
  exit 1
fi

push_code="$(curl -s -o /tmp/bullpen-cutover-push.json -w '%{http_code}' "${BASE}/api/push")"
if [[ "${push_code}" == "200" ]]; then
  grep -q '"configured"' /tmp/bullpen-cutover-push.json
elif [[ "${push_code}" != "401" ]]; then
  echo "FAIL: /api/push returned ${push_code} (expected 200 or 401)" >&2
  exit 1
fi

lib_code="$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/api/library")"
if [[ "${lib_code}" == "200" ]]; then
  echo "FAIL: /api/library returned 200 without auth (expected 401/503)" >&2
  exit 1
fi

echo "OK: health, version, auth gate, push route reachable, library gated"
