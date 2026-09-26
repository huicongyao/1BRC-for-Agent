#!/usr/bin/env bash
# One-time environment setup:
#   1. build harness tools
#   2. generate data/measurements_1b.txt (1,000,000,000 rows, ~13.7 GB)
#   3. precompute + cache the reference output (ground truth)
#
# Usage:  bash harness/setup.sh
# Env:    ROWS (default 1000000000), SEED (default 42)
set -euo pipefail
cd "$(dirname "$0")/.."

ROWS="${ROWS:-1000000000}"
SEED="${SEED:-42}"
DATA="data/measurements_1b.txt"

mkdir -p data harness/bin harness/.cache results/raw

bash harness/build.sh

if [ -f "$DATA" ]; then
    echo "data file already exists, skipping generation: $DATA"
else
    echo "generating $ROWS rows (seed=$SEED) -> $DATA"
    time harness/bin/generate "$ROWS" "$SEED" "$DATA"
fi

echo "data sha256:"
shasum -a 256 "$DATA" | tee "$DATA.sha256"

echo "precomputing reference output (cached under harness/.cache/) ..."
python3 harness/bench.py expected

echo
echo "setup complete."
echo "next:"
echo "  python3 harness/bench.py check --track both"
echo "  python3 harness/bench.py bench --track both"
