#!/usr/bin/env python3
"""Bench gate: an A/B of the base revision and the PR on the same runner (M5, DECISIONS.md #32).

Adapted from pragma.core's tools/bench_gate.py (its DECISIONS #34, docs/perf.md §6).

Both revisions are built up front, then measurements are interleaved: base → head → base → head …
(`--rounds` rounds). For each measurement the best (minimum) criterion median across rounds
is taken: shared-runner noise only slows things down, so the minimum is the most
stable estimate.

A regression is computed as follows:
  ratio      = head / base for the measurement itself;
  ref_ratio  = head / base for its reference (a plain Vec, sequential queries, direct world
               operations, a plain read in the same bench): the change does not affect it, so
               its shift is runner noise;
  normalized = ratio / ref_ratio.
A measurement with a reference fails if ratio > 1 + threshold AND normalized > 1 + threshold.
A measurement without a reference (ecs_sim ms/tick) fails if ratio > 1 + threshold + noise, where
noise is its own noise: median / minimum − 1 over the runs of each revision, the larger of the two.

A measurement missing on the base revision (a new benchmark) is reported, not gated.

Both revisions are built with `-C llvm-args=-align-loops=64` added to RUSTFLAGS (DECISIONS #43),
so that code placement does not decide a measurement.

Usage:
  python3 tools/bench_gate.py --base <base worktree> --head <head worktree>
                              [--rounds 3] [--threshold 0.10] [--summary FILE]
Exit code 1 on a regression, 2 on a launch error.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# (package, bench target, criterion filter).
BENCHES = [
    ("pragma_ecs", "query", r"^query_iter/"),
    ("pragma_ecs", "schedule", r"^schedule_frame/"),
    ("pragma_ecs", "commands", r"^commands/"),
    ("pragma_ecs", "change", r"^change/"),
]
CRITERION_ARGS = ["--warm-up-time", "1", "--measurement-time", "3", "--noplot"]

# Measurement → reference that pragma_ecs changes do not affect.
REFERENCES = {
    "query_iter/query_1_tables/100000": "query_iter/vec/100000",
    "query_iter/query_8_tables/100000": "query_iter/vec/100000",
    "query_iter/query_read_1_tables/100000": "query_iter/vec/100000",
    "query_iter/query_read_8_tables/100000": "query_iter/vec/100000",
    "query_iter/query_parallel_1_tables/100000": "query_iter/vec_parallel/100000",
    "query_iter/query_parallel_8_tables/100000": "query_iter/vec_parallel/100000",
    "schedule_frame/schedule/100000": "schedule_frame/queries/100000",
    "commands/spawn_commands/10000": "commands/spawn_direct/10000",
    "commands/spawn_bundle_commands/10000": "commands/spawn_bundle_direct/10000",
    "commands/despawn_commands/10000": "commands/despawn_direct/10000",
    "change/read_changed_none/100000": "change/read_plain/100000",
    "change/read_changed_10_percent/100000": "change/read_plain/100000",
    "change/read_changed_all/100000": "change/read_plain/100000",
    "change/mut_write_all/100000": "change/mut_read_only/100000",
}
# Both sides are built with hot loops aligned to 64 bytes (DECISIONS #43): otherwise where a
# loop lands in the binary decides some measurements on a shared runner (change/mut_write_all
# 83 vs 110 µs on AMD Zen 4 and Intel for the same source), and any unrelated code change moves it.
ALIGN_LOOPS = "-C llvm-args=-align-loops=64"
ECS_SIM_ENTITIES = 100_000
ECS_SIM_TICKS = 200
ECS_SIM_RUNS = 3


@dataclass
class Side:
    name: str
    root: Path
    target: Path

    def env(self) -> dict[str, str]:
        env = dict(os.environ)
        env["CARGO_TARGET_DIR"] = str(self.target)
        env["RUSTFLAGS"] = f"{env.get('RUSTFLAGS', '')} {ALIGN_LOOPS}".strip()
        return env


def run(cmd: list[str], side: Side) -> str:
    print(f"[{side.name}] $ {' '.join(cmd)}", flush=True)
    done = subprocess.run(
        cmd, cwd=side.root, env=side.env(), capture_output=True, text=True, encoding="utf-8"
    )
    if done.returncode != 0:
        sys.stderr.write(done.stdout + done.stderr)
        raise SystemExit(2)
    return done.stdout


def criterion_results(side: Side) -> dict[str, float]:
    """Medians (ns) of the last run: target/criterion/<id>/new/estimates.json."""
    base = side.target / "criterion"
    results = {}
    for path in base.glob("**/new/estimates.json"):
        bench_id = path.parent.parent.relative_to(base).as_posix()
        data = json.loads(path.read_text(encoding="utf-8"))
        results[bench_id] = float(data["median"]["point_estimate"])
    return results


# `… N ticks on W workers (default D): 1.23s, …` — the same line on every revision since M3.
DURATION = re.compile(r"(\d+) ticks on \d+ workers \(default \d+\): ([\d.]+)(ns|µs|us|ms|s)\b")
SCALE_MS = {"ns": 1e-6, "µs": 1e-3, "us": 1e-3, "ms": 1.0, "s": 1e3}


def ecs_sim_ms_per_tick(output: str) -> float:
    match = DURATION.search(output)
    if not match:
        sys.stderr.write(output)
        raise SystemExit(2)
    ticks, value, unit = int(match[1]), float(match[2]), match[3]
    return value * SCALE_MS[unit] / ticks


def ecs_sim_workers() -> list[int]:
    cores = os.cpu_count() or 1
    return sorted({1, max(1, cores - 1)})


def build(side: Side) -> None:
    for package, bench, _ in BENCHES:
        run(["cargo", "bench", "-p", package, "--bench", bench, "--no-run"], side)
    run(["cargo", "build", "-p", "ecs_sim", "--release"], side)


def measure(side: Side, samples: dict[str, list[float]]) -> dict[str, float]:
    """The measurements of one round; every ecs_sim run is appended to `samples` (for the noise estimate)."""
    results: dict[str, float] = {}
    for package, bench, pattern in BENCHES:
        run(
            ["cargo", "bench", "-p", package, "--bench", bench, "--", pattern, *CRITERION_ARGS],
            side,
        )
    results.update(criterion_results(side))
    exe = side.target / "release" / ("ecs_sim.exe" if os.name == "nt" else "ecs_sim")
    for workers in ecs_sim_workers():
        key = f"ecs_sim/workers_{workers}"
        # Several runs in a row, the best one wins: the time includes pool creation and is noisier
        # than criterion. In ns, like criterion, so the table is uniform.
        runs = [
            ecs_sim_ms_per_tick(
                run(
                    [
                        str(exe),
                        "--entities", str(ECS_SIM_ENTITIES),
                        "--ticks", str(ECS_SIM_TICKS),
                        "--workers", str(workers),
                    ],
                    side,
                )
            )
            * 1e6
            for _ in range(ECS_SIM_RUNS)
        ]
        samples.setdefault(key, []).extend(runs)
        results[key] = min(runs)
    return results


def spread(values: list[float]) -> float:
    """A revision's noise from its own runs: median / minimum − 1 (a single outlier does not inflate it)."""
    ordered = sorted(values)
    return ordered[len(ordered) // 2] / ordered[0] - 1.0


def best(rounds: list[dict[str, float]]) -> dict[str, float]:
    merged: dict[str, float] = {}
    for result in rounds:
        for key, value in result.items():
            merged[key] = min(value, merged.get(key, value))
    return merged


def fmt_ns(ns: float) -> str:
    if ns >= 1e6:
        return f"{ns / 1e6:.3f} ms"
    if ns >= 1e3:
        return f"{ns / 1e3:.2f} µs"
    return f"{ns:.1f} ns"


def main() -> int:
    # The Windows runner console is cp1252: non-Latin-1 report characters (≤, →) would crash the script.
    for stream in (sys.stdout, sys.stderr):
        stream.reconfigure(encoding="utf-8", errors="replace")
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", type=Path, required=True)
    parser.add_argument("--head", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--threshold", type=float, default=0.10)
    parser.add_argument("--summary", type=Path)
    args = parser.parse_args()

    head_root = args.head.resolve()
    sides = [
        Side("base", args.base.resolve(), head_root / "target" / "bench-gate-base"),
        Side("head", head_root, head_root / "target" / "bench-gate-head"),
    ]
    for side in sides:
        build(side)
    rounds: dict[str, list[dict[str, float]]] = {"base": [], "head": []}
    samples: dict[str, dict[str, list[float]]] = {"base": {}, "head": {}}
    for r in range(args.rounds):
        for side in sides:
            print(f"--- round {r + 1}/{args.rounds}: {side.name}", flush=True)
            rounds[side.name].append(measure(side, samples[side.name]))
    base, head = best(rounds["base"]), best(rounds["head"])

    ref_ratios = {
        ref: head[ref] / base[ref] for ref in set(REFERENCES.values()) if ref in base and ref in head
    }
    noise = max((abs(r - 1.0) for r in ref_ratios.values()), default=0.0)
    own_noise = {
        key: max(spread(samples["base"][key]), spread(samples["head"][key]))
        for key in samples["head"]
        if key in samples["base"]
    }
    limit = 1.0 + args.threshold

    rows, failures = [], []
    gated = sorted(k for k in head if k in REFERENCES or k.startswith("ecs_sim/"))
    for key in gated:
        if key not in base:
            rows.append(f"| `{key}` | — | {fmt_ns(head[key])} | — | new | — | not gated |")
            continue
        ratio = head[key] / base[key]
        ref = REFERENCES.get(key)
        if ref in ref_ratios:
            normalized = ratio / ref_ratios[ref]
            failed = ratio > limit and normalized > limit
            note = f"{ref_ratios[ref]:.3f} ({ref.split('/', 2)[1]})"
        else:
            normalized = ratio
            key_noise = own_noise.get(key, noise)
            failed = ratio > limit + key_noise
            note = f"limit {limit + key_noise:.3f} (run noise {key_noise:.3f})"
        verdict = "REGRESSION" if failed else "ok"
        if failed:
            failures.append(key)
        rows.append(
            f"| `{key}` | {fmt_ns(base[key])} | {fmt_ns(head[key])} | {ratio:.3f} | "
            f"{note} | {normalized:.3f} | {verdict} |"
        )

    lines = [
        f"### Bench gate ({os.cpu_count()} cores, {args.rounds} A/B rounds, threshold +{args.threshold:.0%})",
        "",
        "| Measurement | base | head | head/base | reference head/base | normalized | verdict |",
        "|---|---|---|---|---|---|---|",
        *rows,
        "",
        f"Runner noise (largest reference shift): {noise:.1%}.",
    ]
    report = "\n".join(lines)
    print(report)
    if args.summary:
        with args.summary.open("a", encoding="utf-8") as fh:
            fh.write(report + "\n")
    if failures:
        print(f"::error::regression > {args.threshold:.0%}: {', '.join(failures)}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
