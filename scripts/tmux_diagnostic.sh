#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${AISHD_URL:-http://127.0.0.1:5033}"

if ! command -v python >/dev/null 2>&1; then
  echo "python is required to parse JSON" >&2
  exit 2
fi

resp="$(curl -s "${BASE_URL}/v1/diagnostics/tmux")"
echo "${resp}"

python - <<'PY' "$resp"
import json, sys
data = json.loads(sys.argv[1])
if not data.get("ok"):
    sys.exit(1)
PY
