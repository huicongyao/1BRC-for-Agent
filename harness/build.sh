#!/usr/bin/env bash
# Build harness tools: data generator + independent reference implementation.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p bin

clang -O2 -std=c11 -Wall -Wextra -o bin/generate src/generate.c
clang -O2 -std=c11 -Wall -Wextra -o bin/reference src/reference.c

echo "built harness/bin/generate harness/bin/reference"
