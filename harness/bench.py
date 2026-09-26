#!/usr/bin/env python3
"""1BRC benchmark + validation harness.

Commands:
  expected [--input PATH]                 precompute/cache reference (ground truth) output
  check    [--track cpp|rust|both] ...   build, run once, compare byte-exact vs reference
  fresh    [--track ...] [--rows N] ...  anti-hardcoding gate: random seed, fresh generated input
  bench    [--track ...] [--runs N] ...  build + fresh + full validation + timed runs
  summary                                 print current best results

The reference implementation (harness/bin/reference) is the single source of truth.
A run is only valid if (a) fresh random-data check passes, (b) full-file output
matches the reference byte-for-byte, (c) peak RSS <= 12 GB, (d) it finishes <= 600 s.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import random
import re
import statistics
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HARNESS = ROOT / "harness"
BIN = HARNESS / "bin"
CACHE = HARNESS / ".cache"
RESULTS = ROOT / "results"
RAW = RESULTS / "raw"
DATA_DEFAULT = ROOT / "data" / "measurements_1b.txt"

MAX_RSS_BYTES = 12 * 1024**3
RUN_TIMEOUT = 600
BUILD_TIMEOUT = 900
REFERENCE_TIMEOUT = 1800
DEFAULT_RUNS = 5
DEFAULT_WARMUP = 1
DEFAULT_FRESH_ROWS = 1_000_000

TRACKS = {
    "cpp": {
        "dir": ROOT / "solution" / "cpp",
        "bin": ROOT / "solution" / "cpp" / "bin" / "1brc",
        "build": ROOT / "solution" / "cpp" / "build.sh",
    },
    "rust": {
        "dir": ROOT / "solution" / "rust",
        "bin": ROOT / "solution" / "rust" / "target" / "release" / "onebrc",
        "build": ROOT / "solution" / "rust" / "build.sh",
    },
}

TIME_RE = re.compile(r"(\d+)\s+maximum resident set size")


def die(msg: str) -> None:
    print(f"ERROR: {msg}", file=sys.stderr)
    sys.exit(1)


def now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def resolve_tracks(value: str) -> list[str]:
    if value == "both":
        return list(TRACKS)
    if value in TRACKS:
        return [value]
    die(f"unknown track {value!r}")


def git(*args: str) -> str | None:
    try:
        r = subprocess.run(
            ["git", "-C", str(ROOT), *args],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=30,
        )
        return r.stdout.strip() if r.returncode == 0 else None
    except Exception:
        return None


def git_commit() -> str:
    return git("rev-parse", "--short", "HEAD") or "no-git"


def git_dirty(track: str) -> bool:
    out = git("status", "--porcelain", "--", f"solution/{track}")
    return bool(out)


def build_track(track: str) -> None:
    t = TRACKS[track]
    if not t["build"].exists():
        die(f"[{track}] missing build script: {t['build']}")
    print(f"[{track}] building ...", flush=True)
    t0 = time.perf_counter()
    try:
        r = subprocess.run(
            ["bash", str(t["build"])],
            cwd=t["dir"],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=BUILD_TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        die(f"[{track}] build timed out after {BUILD_TIMEOUT}s")
    dt = time.perf_counter() - t0
    if r.returncode != 0:
        print(r.stdout)
        die(f"[{track}] build failed (exit {r.returncode})")
    if not t["bin"].exists():
        die(f"[{track}] build succeeded but binary not found: {t['bin']}")
    print(f"[{track}] build ok ({dt:.1f}s) -> {t['bin'].relative_to(ROOT)}", flush=True)


def run_capture(cmd: list[str], timeout: int) -> tuple[int, float, int, bytes, str]:
    """Run cmd, return (rc, elapsed, max_rss_bytes, stdout, stderr)."""
    with tempfile.TemporaryDirectory(prefix="1brc-run-") as td:
        out = Path(td) / "stdout"
        err = Path(td) / "stderr"
        t0 = time.perf_counter()
        try:
            with open(out, "wb") as fo, open(err, "wb") as fe:
                rc = subprocess.run(cmd, stdout=fo, stderr=fe, timeout=timeout).returncode
        except subprocess.TimeoutExpired:
            return 124, float(timeout), -1, b"", f"timeout after {timeout}s"
        elapsed = time.perf_counter() - t0
        err_text = err.read_text(errors="replace")
        m = TIME_RE.search(err_text)
        rss = int(m.group(1)) if m else -1
        return rc, elapsed, rss, out.read_bytes(), err_text


def run_solution(track: str, input_path: Path) -> dict:
    t = TRACKS[track]
    rc, elapsed, rss, stdout, stderr = run_capture(
        ["/usr/bin/time", "-l", str(t["bin"]), str(input_path)], RUN_TIMEOUT
    )
    return {
        "rc": rc,
        "elapsed": elapsed,
        "max_rss": rss,
        "stdout": stdout,
        "stderr": stderr,
        "timeout": rc == 124,
    }


def run_reference(input_path: Path, output_path: Path) -> int:
    r = subprocess.run(
        [str(BIN / "reference"), str(input_path)],
        stdout=open(output_path, "wb"),
        stderr=subprocess.PIPE,
        timeout=REFERENCE_TIMEOUT,
    )
    return r.returncode


def expected_cache_path(input_path: Path) -> Path:
    st = input_path.stat()
    return CACHE / f"{input_path.name}.{st.st_size}.{st.st_mtime_ns}.expected"


def get_expected(input_path: Path) -> bytes:
    p = expected_cache_path(input_path)
    if not p.exists():
        if not BIN.joinpath("reference").exists():
            die("harness tools not built; run: bash harness/build.sh")
        CACHE.mkdir(parents=True, exist_ok=True)
        print(f"[harness] computing reference output for {input_path} (one-time, cached) ...", flush=True)
        tmp = p.with_suffix(".tmp")
        if run_reference(input_path, tmp) != 0:
            tmp.unlink(missing_ok=True)
            die(f"reference failed on {input_path}")
        tmp.replace(p)
        print(f"[harness] cached {p.relative_to(ROOT)}", flush=True)
    return p.read_bytes()


def diff_summary(expected: bytes, actual: bytes, limit: int = 3) -> str:
    exp_lines = expected.decode("utf-8", "replace").rstrip("\n").split(", ")
    act_lines = actual.decode("utf-8", "replace").rstrip("\n").split(", ")
    problems = []
    if len(exp_lines) != len(act_lines):
        problems.append(f"station count differs: expected {len(exp_lines)}, got {len(act_lines)}")
    for i in range(min(len(exp_lines), len(act_lines))):
        if exp_lines[i] != act_lines[i]:
            problems.append(f"first mismatch: expected {exp_lines[i]!r}, got {act_lines[i]!r}")
            break
    detail = "; ".join(problems[:limit]) or "output differs"
    return detail


def fresh_check(track: str, rows: int, seed: int) -> tuple[bool, str, float]:
    """Generate a fresh random input and compare solution vs reference byte-exactly."""
    with tempfile.TemporaryDirectory(prefix="1brc-fresh-") as td:
        td_path = Path(td)
        gen = td_path / "measurements.txt"
        subprocess.run(
            [str(BIN / "generate"), str(rows), str(seed), str(gen)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=True,
            timeout=300,
        )
        exp = td_path / "expected.txt"
        if run_reference(gen, exp) != 0:
            return False, "reference failed on fresh input", 0.0
        expected = exp.read_bytes()
        res = run_solution(track, gen)
        if res["timeout"]:
            return False, f"solution timed out on {rows} fresh rows", 0.0
        if res["rc"] != 0:
            tail = res["stderr"].strip().splitlines()[-1:] or [""]
            return False, f"solution exit code {res['rc']}: {tail[0]}", 0.0
        if res["stdout"] != expected:
            return False, diff_summary(expected, res["stdout"]), 0.0
        return True, "match", res["elapsed"]


def record_result(record: dict) -> Path:
    RAW.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    path = RAW / f"{stamp}-{record['track']}.json"
    with open(path, "w") as f:
        json.dump(record, f, indent=2)
    return path


def load_records() -> list[dict]:
    records = []
    if not RAW.exists():
        return records
    for p in sorted(RAW.glob("*.json")):
        try:
            records.append(json.loads(p.read_text()))
        except Exception:
            continue
    return records


def fmt(x, spec=".3f", dash="-"):  # type: ignore[no-untyped-def]
    return dash if x is None else format(x, spec)


def fmt_rss(nbytes):  # type: ignore[no-untyped-def]
    if nbytes is None or nbytes < 0:
        return "?"
    if nbytes >= 1024**3:
        return f"{nbytes / 1024**3:.2f} GB"
    if nbytes >= 1024**2:
        return f"{nbytes / 1024**2:.1f} MB"
    return f"{nbytes / 1024:.0f} KB"


def write_leaderboard() -> None:
    records = load_records()
    valid = [r for r in records if r.get("status") == "valid"]
    bests: dict[str, dict] = {}
    for r in valid:
        cur = bests.get(r["track"])
        if cur is None or r["median"] < cur["median"]:
            bests[r["track"]] = r

    lines = [
        "# 1BRC Leaderboard",
        "",
        "> Auto-generated by `harness/bench.py`. Do not edit by hand.",
        "",
        "Machine: Apple M4 (10 cores, 16 GB RAM), macOS. Score = median wall time over runs on `data/measurements_1b.txt`.",
        "",
        "## Best per track",
        "",
        "| track | median (s) | min (s) | max (s) | peak RSS | commit | date (UTC) |",
        "|---|---|---|---|---|---|---|",
    ]
    if bests:
        for track in sorted(bests):
            r = bests[track]
            lines.append(
                f"| {track} | {r['median']:.3f} | {r['min']:.3f} | {r['max']:.3f} "
                f"| {fmt_rss(r['max_rss_bytes'])} | `{r['commit']}`{'*' if r.get('dirty') else ''} "
                f"| {r['ts']} |"
            )
    else:
        lines.append("| - | - | - | - | - | - | - |")

    lines += [
        "",
        "## All runs",
        "",
        "| date (UTC) | track | status | median (s) | min (s) | max (s) | runs | peak RSS | commit | note |",
        "|---|---|---|---|---|---|---|---|---|---|",
    ]
    for r in sorted(records, key=lambda x: x.get("ts", ""), reverse=True):
        note = r.get("reason") or ("ok" if r.get("status") == "valid" else "")
        if r.get("fresh"):
            note = f"fresh:{'pass' if r['fresh'].get('ok') else 'FAIL'} seed={r['fresh'].get('seed')}" + (
                f"; {note}" if note and note != "ok" else ""
            )
        lines.append(
            f"| {r.get('ts', '-')} | {r.get('track', '-')} | {r.get('status', '-')} "
            f"| {fmt(r.get('median'))} | {fmt(r.get('min'))} | {fmt(r.get('max'))} "
            f"| {len(r.get('times') or [])} | "
            f"{fmt_rss(r.get('max_rss_bytes'))} "
            f"| `{r.get('commit', '-')}`{'*' if r.get('dirty') else ''} | {note} |"
        )
    lines.append("")
    RESULTS.mkdir(parents=True, exist_ok=True)
    (RESULTS / "leaderboard.md").write_text("\n".join(lines) + "\n")


def cmd_expected(args: argparse.Namespace) -> None:
    input_path = Path(args.input)
    if not input_path.exists():
        die(f"input not found: {input_path}")
    data = get_expected(input_path)
    out = expected_cache_path(input_path)
    print(f"expected output : {out.relative_to(ROOT)}")
    print(f"size            : {len(data)} bytes")
    print(f"sha256          : {hashlib.sha256(data).hexdigest()}")


def cmd_check(args: argparse.Namespace) -> None:
    input_path = Path(args.input)
    if not input_path.exists():
        die(f"input not found: {input_path}")
    expected = get_expected(input_path)
    failed = False
    for track in resolve_tracks(args.track):
        if not args.no_build:
            build_track(track)
        res = run_solution(track, input_path)
        if res["timeout"]:
            print(f"[{track}] FAIL timeout after {RUN_TIMEOUT}s")
            failed = True
            continue
        if res["rc"] != 0:
            print(f"[{track}] FAIL exit code {res['rc']}")
            print("\n".join(res["stderr"].strip().splitlines()[-5:]))
            failed = True
            continue
        if res["stdout"] != expected:
            print(f"[{track}] FAIL output mismatch: {diff_summary(expected, res['stdout'])}")
            failed = True
            continue
        print(f"[{track}] PASS  {res['elapsed']:.3f}s  peak RSS {fmt_rss(res['max_rss'])}")
    sys.exit(1 if failed else 0)


def cmd_fresh(args: argparse.Namespace) -> None:
    seed = args.seed if args.seed is not None else random.randrange(1, 2**63)
    print(f"fresh input: {args.rows} rows, seed={seed} (regenerate anytime with the same seed)")
    failed = False
    for track in resolve_tracks(args.track):
        if not args.no_build:
            build_track(track)
        ok, msg, elapsed = fresh_check(track, args.rows, seed)
        status = "PASS" if ok else "FAIL"
        extra = f" ({elapsed:.3f}s)" if ok else f" - {msg}"
        print(f"[{track}] {status}{extra}")
        failed = failed or not ok
    sys.exit(1 if failed else 0)


def cmd_bench(args: argparse.Namespace) -> None:
    input_path = Path(args.input)
    if not input_path.exists():
        die(f"input not found: {input_path} (run: bash harness/setup.sh)")
    expected = get_expected(input_path)
    expected_sha = hashlib.sha256(expected).hexdigest()
    failed = False

    for track in resolve_tracks(args.track):
        record: dict = {
            "ts": now_iso(),
            "track": track,
            "status": "invalid",
            "reason": None,
            "input": str(input_path.relative_to(ROOT)),
            "input_bytes": input_path.stat().st_size,
            "expected_sha256": expected_sha,
            "runs": args.runs,
            "warmup": args.warmup,
            "times": [],
            "median": None,
            "min": None,
            "max": None,
            "max_rss_bytes": None,
            "commit": git_commit(),
            "dirty": git_dirty(track),
            "fresh": None,
        }

        if not args.no_build:
            build_track(track)

        if not args.skip_fresh:
            seed = args.seed if args.seed is not None else random.randrange(1, 2**63)
            ok, msg, fresh_elapsed = fresh_check(track, args.fresh_rows, seed)
            record["fresh"] = {"rows": args.fresh_rows, "seed": seed, "ok": ok, "detail": msg}
            if not ok:
                record["reason"] = f"fresh check failed: {msg}"
                record_result(record)
                print(f"[{track}] INVALID: {record['reason']}")
                failed = True
                continue
            print(f"[{track}] fresh check PASS (seed={seed}, {args.fresh_rows} rows, {fresh_elapsed:.3f}s)")

        res = run_solution(track, input_path)
        if res["timeout"]:
            record["reason"] = f"timeout after {RUN_TIMEOUT}s"
        elif res["rc"] != 0:
            record["reason"] = f"exit code {res['rc']}"
        elif res["stdout"] != expected:
            record["reason"] = f"output mismatch: {diff_summary(expected, res['stdout'])}"
        if record["reason"] is None:
            print(f"[{track}] full-file validation PASS ({res['elapsed']:.3f}s)")

        if record["reason"] is None:
            for _ in range(args.warmup):
                run_solution(track, input_path)
            times: list[float] = []
            rss_values: list[int] = []
            for i in range(args.runs):
                res = run_solution(track, input_path)
                if res["timeout"]:
                    record["reason"] = f"timeout on timed run {i + 1}"
                    break
                if res["stdout"] != expected:
                    record["reason"] = f"output mismatch on timed run {i + 1}"
                    break
                times.append(res["elapsed"])
                rss_values.append(res["max_rss"])
                print(f"[{track}] run {i + 1}/{args.runs}: {res['elapsed']:.3f}s", flush=True)
            if record["reason"] is None:
                if max(rss_values) > MAX_RSS_BYTES:
                    record["reason"] = (
                        f"peak RSS {fmt_rss(max(rss_values))} exceeds limit "
                        f"{MAX_RSS_BYTES / 1024**3:.0f} GB"
                    )
                else:
                    record["status"] = "valid"
                    record["times"] = [round(t, 4) for t in times]
                    record["median"] = statistics.median(times)
                    record["min"] = min(times)
                    record["max"] = max(times)
                    record["max_rss_bytes"] = max(rss_values)

        path = record_result(record)
        if record["status"] == "valid":
            print(
                f"[{track}] VALID  median {record['median']:.3f}s  min {record['min']:.3f}s  "
                f"max {record['max']:.3f}s  peak RSS {fmt_rss(record['max_rss_bytes'])}  "
                f"({path.relative_to(ROOT)})"
            )
        else:
            print(f"[{track}] INVALID: {record['reason']}")
            failed = True

    write_leaderboard()
    print(f"leaderboard: {(RESULTS / 'leaderboard.md').relative_to(ROOT)}")
    if failed:
        sys.exit(1)


def cmd_summary(args: argparse.Namespace) -> None:
    records = load_records()
    valid = [r for r in records if r.get("status") == "valid"]
    if not valid:
        print("no valid benchmark runs recorded yet")
        print("run: python3 harness/bench.py bench --track both")
        return
    bests: dict[str, dict] = {}
    for r in valid:
        cur = bests.get(r["track"])
        if cur is None or r["median"] < cur["median"]:
            bests[r["track"]] = r
    print(f"{'track':6} {'median(s)':>10} {'min(s)':>8} {'peak RSS':>9} {'commit':>9}  date")
    for track in sorted(bests):
        r = bests[track]
        print(
            f"{track:6} {r['median']:>10.3f} {r['min']:>8.3f} {fmt_rss(r['max_rss_bytes']):>9} "
            f"{r['commit']:>9}  {r['ts']}"
        )


def main() -> None:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("expected", help="precompute/cache reference output")
    p.add_argument("--input", default=str(DATA_DEFAULT))
    p.set_defaults(func=cmd_expected)

    for name, func, help_text in [
        ("check", cmd_check, "build + run once + byte-exact comparison"),
        ("fresh", cmd_fresh, "anti-hardcoding gate on freshly generated random data"),
    ]:
        p = sub.add_parser(name, help=help_text)
        p.add_argument("--track", default="both", choices=["cpp", "rust", "both"])
        p.add_argument("--input", default=str(DATA_DEFAULT))
        p.add_argument("--no-build", action="store_true", help="skip build step")
        if name == "fresh":
            p.add_argument("--rows", type=int, default=DEFAULT_FRESH_ROWS)
            p.add_argument("--seed", type=int, default=None, help="default: random")
        p.set_defaults(func=func)

    p = sub.add_parser("bench", help="full benchmark: build + fresh + validation + timed runs")
    p.add_argument("--track", default="both", choices=["cpp", "rust", "both"])
    p.add_argument("--input", default=str(DATA_DEFAULT))
    p.add_argument("--runs", type=int, default=DEFAULT_RUNS)
    p.add_argument("--warmup", type=int, default=DEFAULT_WARMUP)
    p.add_argument("--fresh-rows", type=int, default=DEFAULT_FRESH_ROWS)
    p.add_argument("--seed", type=int, default=None, help="fresh-check seed (default: random)")
    p.add_argument("--skip-fresh", action="store_true", help="skip anti-hardcoding fresh check (discouraged)")
    p.add_argument("--no-build", action="store_true")
    p.set_defaults(func=cmd_bench)

    p = sub.add_parser("summary", help="print current best results")
    p.set_defaults(func=cmd_summary)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
