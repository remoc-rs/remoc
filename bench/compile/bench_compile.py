#!/usr/bin/env python3
"""
Compile-time benchmark for remoc.

Builds a binary that creates mpsc, oneshot and watch channels for many complex
nested types and sends channel endpoints through channels, forcing
monomorphization of the remoc channel and Serialize/Deserialize machinery.

Besides wall-clock build time it collects machine-independent metrics that
track the amount of code handed to LLVM, which is what actually drives the
build time of this crate.

Usage:
  bench_compile.py                        run and compare against baseline.json
  bench_compile.py --save baseline.json   run and store the result (updates baseline)
  bench_compile.py --print a.json b.json  compare stored results (1 file just prints)
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import NoReturn

HERE = Path(__file__).resolve().parent
BIN = "remoc_channels"
SRC = HERE / "src" / f"{BIN}.rs"
DEFAULT_BASELINE = HERE / "baseline.json"

SCHEMA = 1
MONO_ITEMS_KEPT = 200
PASSES_KEPT = 20

# Sections shown in the report; all sections are stored in the JSON.
SHOWN_SECTIONS = (".text", ".rodata", ".gcc_except_table", ".eh_frame")

METRICS = [
    (("build_time_s",), "build time (s)", "time"),
    (("binary_size",), "binary size", "int"),
    *[(("sections", s), f"  {s}", "int") for s in SHOWN_SECTIONS],
    (("symbols_total",), "defined symbols", "int"),
    (("symbols_remoc",), "  mentioning remoc", "int"),
    (("llvm_ir_lines",), "LLVM IR lines (pre-opt)", "int"),
    (("mono", "definitions"), "mono definitions", "int"),
    (("mono", "instantiations"), "mono instantiations", "int"),
    (("mono", "size_estimate"), "mono size estimate", "int"),
]

LABEL_W = 26
PASS_LABEL_W = 40
CELL_W = 22


# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------


def die(msg: str) -> NoReturn:
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


def dig(data, path):
    """Look up a nested key path, returning None if any level is missing."""
    for key in path:
        if not isinstance(data, dict) or key not in data:
            return None
        data = data[key]
    return data


def cargo_env() -> dict:
    env = os.environ.copy()
    env["CARGO_INCREMENTAL"] = "0"
    return env


def run(cmd: list[str], *, target_dir: Path | None = None, capture: bool = True) -> subprocess.CompletedProcess:
    env = cargo_env()
    if target_dir is not None:
        env["CARGO_TARGET_DIR"] = str(target_dir)
    return subprocess.run(
        cmd, cwd=HERE, env=env, text=True, capture_output=capture, errors="replace"
    )


def check(proc: subprocess.CompletedProcess, what: str) -> subprocess.CompletedProcess:
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr or "")
        die(f"{what} failed with exit code {proc.returncode}")
    return proc


def tool_version(cmd: list[str]) -> str | None:
    try:
        proc = subprocess.run(cmd, text=True, capture_output=True)
    except OSError:
        return None
    return proc.stdout.strip() if proc.returncode == 0 else None


def git_info() -> dict:
    def git(*args):
        proc = subprocess.run(["git", *args], cwd=HERE, text=True, capture_output=True)
        return proc.stdout.strip() if proc.returncode == 0 else None

    commit = git("rev-parse", "--short", "HEAD")
    status = git("status", "--porcelain")
    return {"commit": commit, "dirty": bool(status)} if commit else {}


# --------------------------------------------------------------------------
# measurement
# --------------------------------------------------------------------------


def build_flags(profile: str) -> list[str]:
    return ["--release"] if profile == "release" else []


def timed_build(profile: str, target_dir: Path) -> tuple[float, Path]:
    """Touch the benchmark source and time a rebuild of just that crate."""
    SRC.touch()
    cmd = [
        "cargo", "build", *build_flags(profile),
        "--bin", BIN, "--message-format=json-render-diagnostics",
    ]
    start = time.monotonic()
    proc = check(run(cmd, target_dir=target_dir), "timed build")
    elapsed = time.monotonic() - start

    executable = None
    for line in proc.stdout.splitlines():
        if not line.startswith("{"):
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("reason") == "compiler-artifact" and msg.get("executable"):
            if msg.get("target", {}).get("name") == BIN:
                executable = Path(msg["executable"])
    if executable is None:
        die("could not determine the benchmark executable path from cargo output")
    return elapsed, executable


def binary_metrics(executable: Path) -> dict:
    """Section sizes and count of monomorphized symbols in the linked binary."""
    out: dict = {"binary_size": executable.stat().st_size, "sections": {}}

    if shutil.which("size"):
        proc = subprocess.run(["size", "-A", str(executable)], text=True, capture_output=True)
        for line in proc.stdout.splitlines():
            m = re.match(r"^(\.\S+)\s+(\d+)\s+\d+", line)
            if m:
                out["sections"][m.group(1)] = int(m.group(2))

    if shutil.which("nm"):
        proc = subprocess.run(
            ["nm", "--defined-only", "-C", str(executable)],
            text=True, capture_output=True, errors="replace",
        )
        if proc.returncode == 0:
            lines = proc.stdout.splitlines()
            out["symbols_total"] = len(lines)
            out["symbols_remoc"] = sum(1 for line in lines if "remoc" in line)

    return out


def llvm_ir_lines(profile: str, target_dir: Path) -> int:
    """Count pre-optimization LLVM IR lines.

    -Cno-prepopulate-passes is essential: it measures the IR handed to LLVM
    rather than what survives optimization, and codegen-units=1 removes
    partitioning noise. The emitted IR is hundreds of MB, so it is written to a
    temporary file and discarded after counting.
    """
    SRC.touch()
    with tempfile.TemporaryDirectory(prefix="remoc-bench-ir-") as tmp:
        ir = Path(tmp) / "bench.ll"
        cmd = [
            "cargo", "rustc", *build_flags(profile), "--bin", BIN, "--",
            f"--emit=llvm-ir={ir}", "-Ccodegen-units=1", "-Cno-prepopulate-passes",
        ]
        check(run(cmd, target_dir=target_dir), "LLVM IR build")
        with ir.open("rb") as f:
            return sum(chunk.count(b"\n") for chunk in iter(lambda: f.read(1 << 20), b""))


def parse_time_passes(stderr: str) -> dict:
    passes: dict[str, float] = {}
    for line in stderr.splitlines():
        m = re.match(r"\s*time:\s+([\d.]+)(?:;.*?)?\s+([\w\-]+)\s*$", line)
        if m:
            passes[m.group(2)] = passes.get(m.group(2), 0.0) + float(m.group(1))
    top = sorted(passes.items(), key=lambda kv: -kv[1])[:PASSES_KEPT]
    return {name: round(secs, 3) for name, secs in top}


def nightly_metrics(profile: str, target_dir: Path, toolchain: str) -> dict:
    """Monomorphization stats and compiler phase breakdown (nightly only).

    Both come from a single compilation, so the phase breakdown is free.
    """
    SRC.touch()
    with tempfile.TemporaryDirectory(prefix="remoc-bench-mono-") as tmp:
        cmd = [
            "cargo", f"+{toolchain}", "rustc", *build_flags(profile), "--bin", BIN, "--",
            f"-Zdump-mono-stats={tmp}", "-Zdump-mono-stats-format=json", "-Ztime-passes",
        ]
        proc = check(run(cmd, target_dir=target_dir), "mono stats build")

        dumps = list(Path(tmp).glob("*.mono_items.json"))
        if not dumps:
            die("nightly build produced no monomorphization stats")
        items = json.loads(dumps[0].read_text())

    items.sort(key=lambda i: -i["total_estimate"])
    return {
        "mono": {
            "definitions": len(items),
            "instantiations": sum(i["instantiation_count"] for i in items),
            "size_estimate": sum(i["total_estimate"] for i in items),
        },
        "mono_items": [
            {
                "name": i["name"],
                "instantiations": i["instantiation_count"],
                "size_estimate": i["total_estimate"],
            }
            for i in items[:MONO_ITEMS_KEPT]
        ],
        "passes": parse_time_passes(proc.stderr),
    }


def measure(profile: str, toolchain: str | None, repeat: int) -> dict:
    target_dir = HERE / "target"
    # A separate target dir keeps the nightly artifacts from invalidating the
    # stable ones (and vice versa) on every run.
    nightly_target_dir = target_dir / "nightly"

    print(f"[{profile}] pre-building dependencies", flush=True)
    check(
        run(["cargo", "build", *build_flags(profile), "--bin", BIN], target_dir=target_dir),
        "dependency build",
    )

    print(f"[{profile}] timing rebuild ({repeat}x)", flush=True)
    times = []
    for _ in range(repeat):
        elapsed, executable = timed_build(profile, target_dir)
        times.append(round(elapsed, 2))

    print(f"[{profile}] verifying binary", flush=True)
    check(subprocess.run([str(executable)], capture_output=True, text=True), "benchmark binary")

    result: dict = {"build_time_s": min(times)}
    if repeat > 1:
        result["build_times_s"] = times
    result.update(binary_metrics(executable))

    print(f"[{profile}] counting LLVM IR lines", flush=True)
    result["llvm_ir_lines"] = llvm_ir_lines(profile, target_dir)

    if toolchain:
        print(f"[{profile}] collecting monomorphization stats", flush=True)
        check(
            run(
                ["cargo", f"+{toolchain}", "build", *build_flags(profile), "--bin", BIN],
                target_dir=nightly_target_dir,
            ),
            "nightly dependency build",
        )
        result.update(nightly_metrics(profile, nightly_target_dir, toolchain))

    return result


def run_benchmark(profiles: list[str], toolchain: str | None, repeat: int, label: str | None) -> dict:
    return {
        "schema": SCHEMA,
        "label": label,
        "timestamp": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "git": git_info(),
        "toolchain": {
            "rustc": tool_version(["rustc", "--version"]),
            "nightly": tool_version(["rustc", f"+{toolchain}", "--version"]) if toolchain else None,
        },
        "profiles": {profile: measure(profile, toolchain, repeat) for profile in profiles},
    }


# --------------------------------------------------------------------------
# reporting
# --------------------------------------------------------------------------


def fmt_value(value, kind: str) -> str:
    if value is None:
        return "-"
    return f"{value:,.2f}" if kind == "time" else f"{value:,}"


def fmt_cell(value, reference, kind: str, show_delta: bool) -> str:
    text = fmt_value(value, kind)
    if show_delta and isinstance(value, (int, float)) and isinstance(reference, (int, float)) and reference:
        text += f" ({(value - reference) / reference * 100:+.1f}%)"
    return text


def result_name(result: dict, path: Path | None) -> str:
    if result.get("label"):
        return result["label"]
    if path is not None:
        return path.stem
    commit = dig(result, ("git", "commit"))
    return commit or "current"


def print_report(results: list[tuple[str, dict]]) -> None:
    names = [name for name, _ in results]
    multiple = len(results) > 1

    for name, result in results:
        parts = [f"{name}:"]
        commit = dig(result, ("git", "commit"))
        if commit:
            parts.append(f"{commit}{'-dirty' if dig(result, ('git', 'dirty')) else ''}")
        if result.get("timestamp"):
            parts.append(result["timestamp"])
        if dig(result, ("toolchain", "rustc")):
            parts.append(dig(result, ("toolchain", "rustc")))
        print("  ".join(parts))

    profiles = []
    for _, result in results:
        for profile in result.get("profiles", {}):
            if profile not in profiles:
                profiles.append(profile)

    for profile in profiles:
        data = [result.get("profiles", {}).get(profile) or {} for _, result in results]
        print(f"\n=== {profile} ===")
        print("metric".ljust(LABEL_W) + "".join(n[:CELL_W].rjust(CELL_W) for n in names))
        print("-" * (LABEL_W + CELL_W * len(names)))
        for path, label, kind in METRICS:
            values = [dig(d, path) for d in data]
            if all(v is None for v in values):
                continue
            row = label.ljust(LABEL_W)
            for index, value in enumerate(values):
                row += fmt_cell(value, values[0], kind, multiple and index > 0).rjust(CELL_W)
            print(row)

        print_passes(names, data)
        if multiple:
            print_mono_diff(names[0], data[0], names[-1], data[-1])


def print_passes(names: list[str], data: list[dict]) -> None:
    if not any(d.get("passes") for d in data):
        return
    ordered: list[str] = []
    for d in data:
        for name in d.get("passes", {}):
            if name not in ordered:
                ordered.append(name)
    ordered.sort(key=lambda p: -max(d.get("passes", {}).get(p, 0.0) for d in data))

    print("\nphase breakdown (seconds, machine-dependent)")
    print("pass".ljust(PASS_LABEL_W) + "".join(n[:CELL_W].rjust(CELL_W) for n in names))
    print("-" * (PASS_LABEL_W + CELL_W * len(names)))
    for name in ordered:
        row = name[: PASS_LABEL_W - 1].ljust(PASS_LABEL_W)
        for d in data:
            secs = d.get("passes", {}).get(name)
            row += ("-" if secs is None else f"{secs:,.3f}").rjust(CELL_W)
        print(row)


def print_mono_diff(first_name: str, first: dict, last_name: str, last: dict, top: int = 15) -> None:
    a = {i["name"]: i for i in first.get("mono_items", [])}
    b = {i["name"]: i for i in last.get("mono_items", [])}
    if not a or not b:
        return

    def est(items, name):
        return items.get(name, {}).get("size_estimate", 0)

    def inst(items, name):
        return items.get(name, {}).get("instantiations", 0)

    names = sorted(set(a) | set(b), key=lambda n: -max(est(a, n), est(b, n)))[:top]
    print(f"\ntop monomorphized items ({first_name} -> {last_name})")
    print(f"{'item':<58}{'instantiations':>22}{'size estimate':>22}")
    print("-" * 102)
    for name in names:
        change = f"{inst(a, name):,} -> {inst(b, name):,}"
        size = f"{est(a, name):,} -> {est(b, name):,}"
        print(f"{name[:57]:<58}{change:>22}{size:>22}")


# --------------------------------------------------------------------------
# entry point
# --------------------------------------------------------------------------


def load(path: Path) -> dict:
    try:
        result = json.loads(path.read_text())
    except FileNotFoundError:
        die(f"{path} does not exist")
    except json.JSONDecodeError as err:
        die(f"{path} is not valid JSON: {err}")
    if result.get("schema") != SCHEMA:
        die(f"{path} has schema version {result.get('schema')}, expected {SCHEMA}")
    return result


def detect_nightly(toolchain: str) -> str | None:
    if tool_version(["cargo", f"+{toolchain}", "--version"]) is None:
        print(
            f"warning: toolchain '{toolchain}' not available, "
            "skipping monomorphization stats and phase breakdown",
            file=sys.stderr,
        )
        return None
    return toolchain


def main() -> None:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "--print", dest="print_files", nargs="+", metavar="FILE", type=Path,
        help="compare stored result files instead of running the benchmark",
    )
    parser.add_argument(
        "--profile", choices=["release", "debug", "both"], default="release",
        help="build profile to benchmark (default: release)",
    )
    parser.add_argument(
        "--baseline", type=Path, default=DEFAULT_BASELINE,
        help=f"result file to compare against (default: {DEFAULT_BASELINE.name})",
    )
    parser.add_argument("--no-baseline", action="store_true", help="do not compare against a baseline")
    parser.add_argument("--save", type=Path, metavar="FILE", help="write the result to FILE")
    parser.add_argument("--label", help="name for this result in the report")
    parser.add_argument(
        "--repeat", type=int, default=1, metavar="N",
        help="time the rebuild N times and report the fastest (default: 1)",
    )
    parser.add_argument(
        "--toolchain", default="nightly",
        help="toolchain providing the unstable stats flags (default: nightly)",
    )
    args = parser.parse_args()

    if args.print_files:
        print_report([(result_name(load(p), p), load(p)) for p in args.print_files])
        return

    if args.repeat < 1:
        die("--repeat must be at least 1")

    save = args.save.resolve() if args.save else None
    baseline = args.baseline.resolve()

    profiles = ["debug", "release"] if args.profile == "both" else [args.profile]
    result = run_benchmark(profiles, detect_nightly(args.toolchain), args.repeat, args.label)

    reports = []
    if not args.no_baseline and baseline != save:
        if baseline.exists():
            stored = load(baseline)
            reports.append((result_name(stored, baseline), stored))
        else:
            print(f"\nnote: no baseline at {baseline}, showing results only")
    reports.append((result_name(result, None), result))

    if save:
        save.parent.mkdir(parents=True, exist_ok=True)
        save.write_text(json.dumps(result, indent=1) + "\n")
        print(f"\nwrote {save}")

    print()
    print_report(reports)


if __name__ == "__main__":
    main()
