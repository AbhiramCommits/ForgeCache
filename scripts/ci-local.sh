#!/usr/bin/env bash
# Local CI: format check, clippy, tests, and a cold+warm smoke build.
# Mirrors .github/workflows/ci.yml on a developer machine.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "========================================"
echo " 1. cargo fmt --check"
echo "========================================"
cargo fmt --all -- --check

echo "========================================"
echo " 2. cargo clippy -D warnings"
echo "========================================"
cargo clippy --all-targets -- -D warnings

echo "========================================"
echo " 3. cargo test --workspace"
echo "========================================"
TEST_LOG="$(mktemp)"
cargo test --workspace 2>&1 | tee "$TEST_LOG"

echo "========================================"
echo " 4. cargo build (debug, for smoke)"
echo "========================================"
cargo build --workspace

echo "========================================"
echo " 5. cold + warm smoke build"
echo "========================================"
bash scripts/smoke_build.sh 12 95

TOTAL="$(python3 - "$TEST_LOG" <<'PY'
import re
import sys

text = open(sys.argv[1]).read()
total = sum(int(m.group(1)) for m in re.finditer(r"test result: ok\. (\d+) passed", text))
print(total)
PY
)"
rm -f "$TEST_LOG"
echo "========================================"
echo " ci-local OK — ${TOTAL} tests passed"
echo "========================================"
