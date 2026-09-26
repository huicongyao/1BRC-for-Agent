# Rust track experiments

One row per experiment. Keep it append-only and honest (including failed ideas
and measurements that looked like noise). `median` is the score from
`python3 harness/bench.py bench --track rust`.

| # | date (UTC) | hypothesis / change | median (s) | delta vs best | peak RSS (GB) | decision | commit |
|---|---|---|---|---|---|---|---|
| 0 | 2026-09-26 | naive baseline (BufReader + BTreeMap + manual parse), 1-run record | 76.616 | - | 0.003 | starting point | - |
| 1 | 2026-09-27 | read_at-based parallel readers + SWAR parser + per-thread hash tables + exact integer aggregation | 6.156 | -92.0% | 0.035 | accepted | see log |

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
