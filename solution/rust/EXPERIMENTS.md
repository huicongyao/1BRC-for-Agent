# Rust track experiments

One row per experiment. Keep it append-only and honest (including failed ideas
and measurements that looked like noise). `median` is the score from
`python3 harness/bench.py bench --track rust`.

| # | date (UTC) | hypothesis / change | median (s) | delta vs best | peak RSS (GB) | decision | commit |
|---|---|---|---|---|---|---|---|
| 0 | 2026-09-26 | naive baseline (BufReader + BTreeMap + manual parse), 1-run record | 76.616 | - | 0.003 | starting point | - |
| 1 | 2026-09-27 | read_at-based parallel readers + SWAR parser + per-thread hash tables + exact integer aggregation | 6.156 | -92.0% | 0.035 | accepted | see log |
| 2 | 2026-09-27 | same aligned-reader pipeline via std read_at + extern fcntl(F_NOCACHE) | 6.143 | -0.2% vs #1 bench, -17% interleaved A/B | 0.131 | accepted | see log |
| 3 | 2026-09-27 | parser pool 6 -> 10 threads, block pool 8 -> 12 | 5.624 | -8.4% vs #2 bench, -3% interleaved A/B | 0.196 | accepted | see log |

## Notes

### #1 architecture (accepted, median 6.156 s)

Port of the C++ track design (see `solution/cpp/EXPERIMENTS.md` for the
measurements that drove it), using std only:

* `std::os::unix::fs::FileExt::read_at` (pread) on a shared `File`, so several
  threads read their own newline-aligned slice without a seek race.
* `std::thread::scope` for the workers; each owns a `Table` (open addressing,
  64-byte slots) and returns it for the merge.
* Same 8-byte SWAR line parser and the same tolerant scalar fallback; raw
  pointer loads (`read_unaligned`) avoid bounds-check overhead.
* Same exact-integer mean with the sequential-double fallback for stations that
  land on a rounding boundary, so the output is byte-identical to the reference
  including the `A;-90.7 / A;6.7 / A;67.0 / A;-22.9` style ties.

Rust vs the C++ binary on the same machine: 6.156 s vs 6.226 s median (both
within the ±1-3% run-to-run noise), peak RSS 34.5 MB vs 50.2 MB (Rust uses
`KBLOCK`-sized per-thread buffers only; no mmap, no page table overhead).

Bugs found and fixed while porting (recorded because they are easy to repeat):
* The streaming loop's "bytes consumed" return value must be the parser's stop
  position, not the input length; returning the length skipped the last
  `input_len - stop` bytes of every block and re-parsed a fragment as a station
  (`l Aviv`, `ich`).
* Stale bytes past the carry must never be re-parsed: the previous C++ tail
  path appended a synthetic newline and then parsed past it, picking up
  fragments of the previous buffer content.

### #2 single sequential F_NOCACHE reader (accepted)

Same change as the C++ track, in Rust: one reader thread streaming 16 MB
4096-aligned blocks with `F_NOCACHE`, 6 parser threads over a bounded
block queue, and the partial line parked in front of the aligned read area.

* Buffers come from `std::alloc::alloc` with `Layout::from_size_align(.., 4096)`
  (no crate, no libc malloc wrapper).
* `F_NOCACHE` is the one thing std does not wrap, so it is a direct
  `extern "C" { fn fcntl(..) }` declaration against the C library that std
  already links; no crate is involved and the build stays `--offline`.
* Reader and parsers use the same `File` through `FileExt::read_at`.

Result: interleaved A/B against the #1 binary, same ambient conditions,
3 rounds each - #1: 6.15 / 6.31 / 6.41 s (median 6.31), #2: 5.18 / 5.26 /
5.29 s (median 5.26), i.e. **-17%**; the official bench recorded 6.143 s median
in a busier window (raw device 2.30 GB/s then, vs 3.3 GB/s during the quiet
window that produced the C++ 5.875 s record).  Ambient load on this shared
machine moves the device between ~2.3 and ~3.4 GB/s, so cross-track comparisons
must be made from interleaved runs, not from records taken minutes apart.

### #3 parser pool sizing (accepted)

Same experiment as the C++ track.  Interleaved sweep, 5 rounds each, medians:

| parsers | 6 | 8 | 10 | 12 |
|---|---|---|---|---|
| median (s) | 5.70 | 5.54 | **5.53** | 5.53 |

10 parsers and 12 block slots kept (matching the C++ track).  Official bench
after the change: 5.624 s median, peak RSS 196 MB.  Extra parsers are cheap
because they spend most of their time waiting on the queue - the reader is the
bottleneck - but they drain the pool faster, which shortens the reader's
`acquire()` stalls.
