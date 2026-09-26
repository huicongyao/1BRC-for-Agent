# C++ track experiments

One row per experiment. Keep it append-only and honest (including failed ideas
and measurements that looked like noise). `median` is the score from
`python3 harness/bench.py bench --track cpp`.

| # | date (UTC) | hypothesis / change | median (s) | delta vs best | peak RSS (GB) | decision | commit |
|---|---|---|---|---|---|---|---|
| 0 | 2026-09-26 | naive baseline (fgets + std::map + manual parse), 1-run record | 83.845 | - | 0.002 | starting point | - |
| 1 | 2026-09-27 | pread-based parallel readers + SWAR parser + per-thread hash tables + exact integer aggregation | 6.226 | -92.6% | 0.050 | accepted | see log |
| 2 | 2026-09-27 | single sequential F_NOCACHE reader (4096-aligned) feeding 6 parser threads | 5.875 | -5.6% vs #1 bench, -20% interleaved A/B | 0.131 | accepted | see log |

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

### #2 single sequential F_NOCACHE reader (accepted)

Hypothesis: if the reader keeps the file offsets and buffers 4096-byte aligned
and uses `F_NOCACHE`, the SSD delivers ~3.2-3.5 GB/s instead of ~2.25 GB/s.

Evidence (probe programs, full 13.8 GB file, same machine state):

| read pattern                                        | time for 13.8 GB |
|-----------------------------------------------------|------------------|
| 1 stream, 32 MB blocks, buffered (page cache)        | 5.9-6.1 s (2.25 GB/s) |
| 1 stream, 16 MB blocks, F_NOCACHE, **misaligned by 32 B** | 6.95 s (1.99 GB/s) |
| 1 stream, 16 MB blocks, F_NOCACHE, 4096-aligned      | 4.04-4.30 s (3.2-3.4 GB/s) |
| 1 stream, 16 MB, F_NOCACHE, file offset 512 (not 4k) | 6.77 s (2.04 GB/s) |
| 4 streams, 4 MB, shared fd                           | ~6.2 s |
| 4 streams, 16 MB, per-fd + F_NOCACHE                 | ~5.3-6.6 s |
| 3 | 2026-09-27 | parser pool 6 -> 10 threads, block pool 8 -> 12 | 5.668 | -3.5% | 0.196 | accepted | see log |

So both the file offset *and* the destination buffer must be 4096-byte aligned
for the uncached DMA path; otherwise the kernel falls back to a ~40% slower
path (this also explained why the first attempt at a "single reader" was
slower than the sliced version: the carry bytes were prepended, shifting the
read destination off alignment).

Changes:
* one reader thread streams the file in 16 MB blocks, `pread` at
  `pos = k*16MB` into a `posix_memalign(4096)` buffer at offset `kCarryArea`
  (a multiple of 4096), with `F_NOCACHE` when the input is >= 256 MB;
* the partial line from the previous block is parked in the kCarryArea bytes
  *in front of* the read area, so parser input stays contiguous while the read
  destination stays aligned;
* 6 parser threads consume blocks through a bounded queue (8 slots);
* lines longer than kCarry (never produced by the generator) switch the reader
  to an accumulating slow path so correctness does not depend on line length.

Result: interleaved A/B against the #1 binary, same ambient conditions,
3 rounds each - #1: 6.37 / 6.72 / 7.75 s (median 6.72), #2: 5.28 / 5.40 /
5.71 s (median 5.40), i.e. **-20%**; the official bench recorded 5.875 s median
in a quieter window.  The reader now runs within 0-4% of a pure `read()` of the
same file measured in the same minute, so the remaining time is the device.

Also measured during this experiment: the single reader reaches the same
throughput with 0, 4 or 6 CPU-heavy competitor threads (2.72 -> 2.65 GB/s), so
the parser pool does not starve it.

### #3 parser pool sizing (accepted)

Hypothesis: the reader blocks on the pool whenever all slots are busy, so more
consumers shorten those stalls; the parsers are otherwise idle (the reader is
the bottleneck), so oversubscribing costs nothing.

Interleaved sweep on the full file, 5 rounds each, medians:

| parsers | 4 | 6 | 8 | 10 | 12 |
|---|---|---|---|---|---|
| median (s) | 6.52 | 5.91 | 5.68 | **5.60** | 5.63 |

Block size at 10 parsers: 8 MB 5.75, 16 MB 5.64, 32 MB 5.61 - 16 MB kept
(same speed, half the buffer memory).  Official bench after the change:
5.668 s median, peak RSS 196 MB.
