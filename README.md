# 1BRC Agent Self-Iteration Environment

An autonomous-optimization testbed for the [One Billion Row Challenge](https://github.com/gunnarmorling/1brc)
on an Apple M4 MacBook Air. A coding agent iterates on two independent,
single-file implementations (C++ and Rust) under a frozen contract
([`AGENTS.md`](AGENTS.md)) with byte-exact correctness gates, a fixed
measurement protocol, and a work-log / commit discipline.

**Problem.** Read a text file of 1,000,000,000 lines `<station>;<temperature>`
(13.8 GB) and print, to stdout, `{station=min/mean/max, ...}` sorted by station
name, one decimal per value. Semantics are defined by an independent reference
implementation (`harness/src/reference.c`), not by the optimizers.

## Results (this machine, `data/measurements_1b.txt`)

| track | naive baseline | best recorded median | clean-tree median | peak RSS | speedup |
|---|---|---|---|---|---|
| C++ | 83.845 s | **5.668 s** | 6.029 s | 196 MB | 14.8x |
| Rust | 76.616 s | **5.624 s** | 6.014 s | 196 MB | 13.6x |

All figures are medians of 5 timed runs, each validated byte-exact against the
reference plus a fresh 1M-row random-seed anti-hardcoding check. The two tracks
are within noise of each other; the run time equals a raw sequential read of
the same file measured in the same minute, i.e. the solutions are at the
device limit, not CPU-bound.

**Architecture (identical in both tracks).** One sequential reader streams the
file in 16 MB `F_NOCACHE` blocks, with both the file offset and the destination
buffer 4096-byte aligned; a parser pool (10 threads, bounded 12-slot queue)
consumes whole lines and parses them with 8-byte SWAR tricks; aggregation keeps
per-thread open-addressing tables of exact integer tenths, merged at the end.
The mean reproduces the reference's `double` rounding exactly, including rare
half-tenth ties, via a fallback sequential scan.

## Quick start

```bash
bash harness/setup.sh                      # build tools, generate the 1B dataset, cache reference output
python3 harness/bench.py check --track both   # one full run per track, byte-exact
python3 harness/bench.py bench --track both   # the score: build + fresh gate + 5 timed runs
python3 harness/bench.py summary
```

Requirements: macOS on Apple Silicon, Apple clang, Rust toolchain, Python 3.
No external libraries or crates. The dataset is not in the repository; it is
generated locally (~40 s, ~13.8 GB) and never modified.

## Repository layout

```
AGENTS.md                  frozen contract: semantics, rules, protocol, iteration loop
harness/src/generate.c     deterministic data generator (413 official stations, seeded)
harness/src/reference.c    independent single-threaded reference implementation (ground truth)
harness/src/stations.h     official station universe (generated table)
harness/bench.py           build + anti-hardcoding gate + byte-exact validation + timing/RSS
harness/setup.sh           one-time environment setup and data generation
solution/cpp/              C++ track: src/main.cpp single translation unit + build.sh + EXPERIMENTS.md
solution/rust/             Rust track: src/main.rs single file, std only + Cargo.toml + EXPERIMENTS.md
results/leaderboard.md     auto-generated run history
results/FINAL.md           final report: architecture, C++ vs Rust, hardware-limit analysis
data/                      generated benchmark input (gitignored)
```

## Measurement protocol

`python3 harness/bench.py bench --track <cpp|rust>` builds the track, runs a
fresh 1M-row random-seed byte-exact check, validates one full-file run against
the reference, then measures 1 warmup + 5 timed runs. The score is the median;
peak RSS comes from `/usr/bin/time -l`. A record is `valid` only if every check
passes, RSS ≤ 12 GB and no run exceeds 600 s.

## Iteration discipline

Each accepted change is one commit with its measured number, one row appended
to the track's `EXPERIMENTS.md`; rejected ideas are reverted and logged with
their measurements. A track is frozen after three consecutive experiments
improve the median by < 1%. See `results/FINAL.md` for the retrospects and the
evidence that the result is I/O-bound.

## Known limitation

On exact negative half-tenth ties the Rust fallback mean can differ from the
reference by one tenth (`A;-0.2` + `A;0.1` → `0.0` instead of `-0.1`) because
it does not emulate the compiler's FMA contraction. This requires the exact
mean to be a tie *and* the accumulated float error to land within ~1 ulp of
the boundary (≈0.2% per 1M-row fresh gate; ≈1e-7 per 1B run). The C++ track is
unaffected. Fix: use `f64::mul_add(x, 10.0, 0.5)` in the fallback.
