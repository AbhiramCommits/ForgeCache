#!/usr/bin/env bash
# Generate a small firmware workload and run a cold + warm build through a real
# local worker, failing if the warm build is not at least THRESHOLD% cache hits.
#
# Usage: scripts/smoke_build.sh [MODULES] [THRESHOLD_PERCENT]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

MODULES="${1:-12}"
THRESHOLD="${2:-95}"

WORKLOAD="$(mktemp -d)"
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$WORKLOAD" "$SCRATCH"; if [[ -n "${WORKER_PID:-}" ]]; then kill "$WORKER_PID" 2>/dev/null || true; fi' EXIT

echo "== generating ${MODULES}-module workload =="
python3 tools/gen_workload.py --modules "$MODULES" --fanout 2 --out "$WORKLOAD" >/dev/null
python3 tools/gen_manifest.py --workload "$WORKLOAD" >/dev/null

PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"

echo "== starting worker on 127.0.0.1:${PORT} =="
./target/debug/forge-worker --listen "127.0.0.1:${PORT}" --slots 2 \
    --workdir "$SCRATCH" --id smoke >/dev/null 2>&1 &
WORKER_PID=$!
sleep 0.6

echo "== cold build =="
./target/debug/forge-coord build --manifest "$WORKLOAD/build.json" \
    --workers "127.0.0.1:${PORT}" --jobs 2 --clean-cache \
    --report "$SCRATCH/cold.json" >/dev/null 2>&1

echo "== warm build =="
./target/debug/forge-coord build --manifest "$WORKLOAD/build.json" \
    --workers "127.0.0.1:${PORT}" --jobs 2 \
    --report "$SCRATCH/warm.json" >/dev/null 2>&1

python3 - "$SCRATCH/cold.json" "$SCRATCH/warm.json" "$THRESHOLD" <<'PY'
import json
import sys

cold = json.load(open(sys.argv[1]))
warm = json.load(open(sys.argv[2]))
threshold = float(sys.argv[3])

print(f"cold: {cold['actions_miss']}/{cold['actions_total']} executed")
print(f"warm: {warm['actions_hit']}/{warm['actions_total']} hits "
      f"= {warm['cache_hit_rate'] * 100:.0f}%")

assert cold["actions_miss"] == cold["actions_total"], "cold build must execute every action"
assert warm["actions_miss"] == 0, "warm build must execute nothing"
assert warm["cache_hit_rate"] * 100 >= threshold, \
    f"warm hit rate {warm['cache_hit_rate'] * 100:.0f}% below {threshold}%"
PY

echo "smoke build OK (warm hit rate >= ${THRESHOLD}%)"
