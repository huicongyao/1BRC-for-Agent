# C++ track experiments

One row per experiment. Keep it append-only and honest (including failed ideas
and measurements that looked like noise). `median` is the score from
`python3 harness/bench.py bench --track cpp`.

| # | date (UTC) | hypothesis / change | median (s) | delta vs best | peak RSS (GB) | decision | commit |
|---|---|---|---|---|---|---|---|
| 0 | 2026-09-26 | naive baseline (fgets + std::map + manual parse), 1-run record | 83.845 | - | 0.002 | starting point | - |
| 1 | 2026-09-27 | pread-based parallel readers + SWAR parser + per-thread hash tables + exact integer aggregation | 6.226 | -92.6% | 0.050 | accepted | see log |

## Notes

### #1 architecture (accepted, median 6.226 s)

Measurements that drove the design (probe programs, `/usr/bin/time -l`):

* The 1B input (13.8 GB) does **not** stay in the page cache on this 16 GB
  machine: two back-to-back sequential read passes both take 5.96 s
  (2.31 GB/s), i.e. the SSD, not RAM, is the limit.
* `read(2)` into a 4-8 MB buffer sustains ~2.3 GB/s; `mmap` + page faults
  sustains only ~0.59 GB/s (23.4 s) and its resident pages inflate RSS to
  4.4 GB.  Parallel streams do **not** raise device throughput
  (1/2/4/8 threads all land at 2.0-2.3 GB/s).
* Consequence: wall clock has a hard floor of ~6.0 s for 13.8 GB, so the
  implementation must keep the device busy and stay out of the way otherwise.

Implementation:

* 4 threads, each `pread`s its own contiguous, newline-aligned slice in 4 MB
  blocks and parses the block in place (parse/IO overlap, no copy besides the
  kernel read).
* 8-byte SWAR: `haszero` on the 8 bytes XOR `';'` locates the name separator,
  and one 8-byte load decodes the canonical `[-]D.D / [-]DD.D / [-]DDD.D`
  temperature.  Anything else (long names, odd formats, missing final newline,
  `\r\n`) falls back to a scalar parser that mirrors `harness/src/reference.c`
  exactly.
* Aggregation: per-thread open-addressing tables of 64-byte slots keyed by a
  multiply-shift hash of the name; min/max/sum kept as exact integers of tenths.
* Output is formatted from integers (`min`, `mean`, `max` are all exact tenths),
  which is byte-identical to `%.1f` for these values.

**Mean rounding.**  The reference sums `t10/10.0` in file order as a `double`
and prints `floor((sum/count)*10 + 0.5)/10`; parallel accumulation cannot
reproduce that rounding *order*.  Instead the mean is computed exactly as
`floor((2S + n) / (2n))` with `S` the exact integer sum of tenths, which equals
what the reference's doubles evaluate to except when the exact value lands
inside the reference's own rounding error of a `.5` boundary.  That window is
bounded by `8*u*(n*max|t10| + 8*(|S|/n + 1))`, `u = 2^-53` (per-add bound +
per-value conversion + final ops); stations inside it are recomputed with a
sequential double scan in file order, i.e. exactly like the reference.  This is
not theoretical: for `A;-90.7 / A;6.7 / A;67.0 / A;-22.9` (n=4, S=58) the exact
value is `1.5` while the reference prints `1.4` - the fallback reproduces the
reference byte-exactly where plain exact arithmetic would fail.  For the harness
inputs the window is ~1e-6 wide, so it triggers with probability ~1e-3 and
costs no measurable time.

Rejected / considered alternatives:

* `mmap` (+`MADV_SEQUENTIAL`) instead of `pread`: 0.59 GB/s vs 2.31 GB/s, and
  RSS accounting counts resident file pages (4.4 GB observed).
* One dedicated reader thread feeding parser threads: no benefit, the device is
  already saturated by a few streams, and it costs a copy.
* Reading with 8/16/32/64 MB blocks or with 2/6/8 threads: all within noise of
  ~2.0-2.3 GB/s (one 4.87 s outlier at 4x16 MB was not reproducible).
