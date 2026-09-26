#!/usr/bin/env bash
# Build the C++ track. Must produce ./bin/1brc relative to this directory.
# You may edit compiler flags / source layout within the rules in AGENTS.md.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p bin
clang++ -O3 -std=c++20 -Wall -Wextra -o bin/1brc src/main.cpp
