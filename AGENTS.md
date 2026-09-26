# AGENTS.md — 1BRC Self-Iteration Environment

You are the optimization agent for this repository. Your job is to make two
implementations of the One Billion Row Challenge (1BRC) as fast as physically
possible on this machine while remaining byte-exact correct. You iterate
autonomously: hypothesize → change → verify → measure → keep or revert → log.

Read this file fully before touching anything. It is the contract and it is
frozen (do not modify it).

---

## 1. Machine

- Apple M4 MacBook Air, 10 cores (4P + 6E), 16 GB unified memory, macOS 27.
- Toolchain available: Apple clang (C++), rustc/cargo 1.93 (no external crates
  needed), Python 3.13 (harness only). No Java and none needed.
- Data lives on the internal SSD. Do not run other heavy processes while
  benchmarking.

## 2. Problem

Input is a text file with 1,000,000,000 lines, each:

```
<station name>;<temperature>\n
```

- Station is one of 413 names from the official 1BRC generator (see
  `harness/src/stations.h`).
- Temperature is a decimal with exactly one fraction digit (e.g. `18.0`,
  `-6.9`, `0.0`).
- Example line: `Abha;18.0`

Output must be written to **stdout**, as a single line, byte-exact:

```
{Abha=18.0/18.0/18.0, Abidjan=15.7/26.0/34.1, ..., Zürich=2.1/9.3/16.1}
```

Semantics (authoritative implementation: `harness/bin/reference`):

- Stations sorted by their name bytes (UTF-8 byte order; equals the official
  Java ordering for this station set).
- Format per station: `name=min/mean/max`, separator between stations is
  `, ` (comma + space).
- `min` / `max`: the parsed temperature values.
- Values are printed with exactly one fraction digit (`%.1f` style).
- `mean = floor((sum / count) * 10.0 + 0.5) / 10.0` where `sum` is the sum of
  the parsed values accumulated as IEEE-754 `double` and `count` is the number
  of samples for that station. This is Java `Math.round` semantics (note:
  `floor(x + 0.5)`, not round-half-away-from-zero; they only differ at exact
  negative ties).
- Parsing a temperature as the decimal value means: parse the scaled tenths
  `t10` as an integer and use `t10 / 10.0` (IEEE-754 correctly rounded).
- Output ends with exactly one `\n`, no trailing spaces.

The output is tiny (~413 stations).

## 3. Data and ground truth

| Path | What |
|---|---|
| `data/measurements_1b.txt` | main benchmark input, 1B rows, ~13.7 GB, seed 42, **immutable** |
| `data/measurements_1b.txt.sha256` | integrity hash of the input |
| `harness/bin/generate` | deterministic generator: `<rows> <seed> <output>` |
| `harness/bin/reference` | independent single-threaded reference implementation |
| `harness/src/*.c`, `harness/src/stations.h` | readable sources of the above (do not modify) |
| `harness/.cache/` | cached reference outputs, keyed by input size+mtime (do not modify) |

`harness/bin/reference` is the single source of truth. Read its source if the
semantics are unclear. Fresh random data (same 413-station universe, random
selection and temperatures) is generated at validation time with an
unpredictable seed — the anti-hardcoding gate.

## 4. Tracks and interfaces

Two independent tracks. Both must always be correct; optimize them in turns or
in parallel as you prefer.

| Track | Source (single file) | Build | Binary to run |
|---|---|---|---|
| C++ | `solution/cpp/src/main.cpp` | `bash solution/cpp/build.sh` | `solution/cpp/bin/1brc` |
| Rust | `solution/rust/src/main.rs` | `bash solution/rust/build.sh` | `solution/rust/target/release/onebrc` |

Invocation contract (fixed, enforced by the harness):

```
<binary> <input_file>     # read input_file, write result to stdout, exit 0
```

- Nothing else may be written to stdout.
- stderr is ignored (use it for diagnostics only).
- Build scripts and `Cargo.toml` profile settings may be edited; keep builds
  offline and dependency-free.

## 5. Hard rules

1. **No external dependencies.** C++: standard library only. Rust: std only,
   no crates (`--offline` build). No network access ever.
2. **Single source file per track** (one translation unit; Rust `main.rs` only).
   Build scripts may not inject additional hand-written source files.
3. **Runtime isolation.** The solution reads only `argv[1]`. It must not read
   any other file, must not write any file, must not spawn subprocesses.
4. **No cheating.** No hardcoded outputs, no caching results between runs, no
   detecting the known 1B file / its size / its hash, no reading
   `harness/.cache` or expected output inside the solution. Every validation
   generates fresh random data; hardcoding is pointless and is a failure of the
   task even if it passes.
5. Anything not explicitly forbidden by these rules is permitted. The input is
   immutable; never write to it.
6. Limits: wall time ≤ 600 s per run, peak RSS ≤ 12 GB. No other resource
   limits.
7. **Frozen files:** `AGENTS.md`, everything under `harness/` and `data/`, and
   the other track's directory. Never modify them. `results/` is written only
   by `harness/bench.py`. If you believe the harness has a bug, record it in
   `results/ISSUES.md` and continue; do not patch the harness yourself.
8. **Git discipline.** Small commits, one accepted improvement each, message
   like `cpp: <change> (median X.XXXs, -Y%)`. Revert rejected
   experiments (`git restore solution/<track>`). Never `git push`, never
   rewrite history. Keep `EXPERIMENTS.md` append-only and honest. Do not commit
   `data/`, `harness/bin/`, `harness/.cache/`, `solution/*/target/`,
   `solution/cpp/bin/` (already gitignored).

## 6. Measurement protocol

```bash
python3 harness/bench.py bench --track cpp     # the score
```

A `bench` run performs, in order:

1. build (must succeed),
2. fresh anti-hardcoding check: 1M rows, random seed, byte-exact diff,
3. one full-file run validated byte-exact against the reference output,
4. 1 warmup run (page cache warm),
5. 5 timed runs, each re-validated byte-exact.

**Score = median of the 5 timed runs.** Peak RSS comes from `/usr/bin/time -l`.
A result is recorded as `valid` only if all checks pass, RSS ≤ 12 GB and no
run exceeds 600 s. Records are appended to `results/raw/*.json` and summarized
in `results/leaderboard.md` — read them to track progress.

Other commands:

```bash
python3 harness/bench.py fresh --track cpp             # quick correctness gate (~seconds)
python3 harness/bench.py check --track both            # one run per track, byte-exact
python3 harness/bench.py summary                       # current bests
python3 harness/bench.py bench --track cpp --runs 9    # more samples when noise matters
```

Noise on this machine is typically 1–3%. Before trusting a small delta, re-run
`bench`; compare medians of the repeated runs. Never benchmark while another
build/bench runs concurrently.

## 7. Iteration loop (per track)

1. **Baseline.** If the track has no valid record yet, run `bench` once and
   record the naive baseline in `EXPERIMENTS.md`.
2. **Hypothesis.** Inspect the current implementation, identify the single
   biggest bottleneck, and write down one falsifiable, quantified prediction
   about the effect of your next change.
3. **Minimal change.** Implement exactly that change. One variable at a time.
4. **Quick gate.** `python3 harness/bench.py fresh --track <track>`.
5. **Measure.** `python3 harness/bench.py bench --track <track>`.
6. **Decide.**
   - accepted: correctness passes and median improves ≥ 1% → commit with the
     measured number, append a row to `solution/<track>/EXPERIMENTS.md`.
   - rejected / noise: revert the code change, log the failed hypothesis and
     the measured number anyway, try the next idea.
7. **Repeat** until the finishing condition below.

### Finishing condition ("optimal")

A track has plateaued when **3 consecutive experiments each improve the median
by < 1%** (measurement failures do not count as experiments). When a track
plateaus: stop optimizing it, freeze the best commit, and write a short
retrospective in `solution/<track>/EXPERIMENTS.md` (what worked, what did not,
remaining known headroom). When **both** tracks have plateaued:

- write `results/FINAL.md`: per-track best median/min/peak RSS/commit,
  a description of the final architecture of each solution, a C++ vs Rust
  comparison grounded in the recorded numbers, and an argument for why the
  result is at or near the hardware limit;
- leave the working tree clean (`git status` shows no modified tracked files).

## 8. Files you own vs. frozen

| Own (edit freely) | Frozen (never edit) |
|---|---|
| `solution/cpp/**` (except shared scaffolding rules above) | `AGENTS.md` |
| `solution/rust/**` | `harness/**` |
| `results/ISSUES.md` | `data/**` |
| | `results/leaderboard.md`, `results/raw/**` (bench.py writes these) |

## 9. Cheat sheet

```bash
# environment (already done at setup, for reference)
bash harness/setup.sh

# correctness + score
python3 harness/bench.py fresh --track cpp
python3 harness/bench.py bench --track both
python3 harness/bench.py summary

# git flow for one accepted experiment
git add -A solution/cpp && git commit -m "cpp: <change> (median X.XXXs, -Y%)"
```
