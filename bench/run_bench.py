#!/usr/bin/env python3
"""ForgeCache benchmark harness (Python standard library only).

Measures, on the machine it runs on:

1. ``serial-local``  : one worker, ``--jobs 1``, ``--no-cache`` (baseline)
2. ``parallel-cold`` : K workers, empty cache
3. ``parallel-warm`` : same workers, fully populated cache
4. ``incremental``   : one leaf ``.c`` changed, rebuild

Each configuration runs several trials. The harness writes
``bench/results/results.json`` and renders ``bench/results/REPORT.md`` with a
Markdown table and an ASCII bar chart of speedups.

``--sweep-jobs 1,2,4,8`` produces a scaling curve instead of the four configs.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MANIFEST = os.path.join(REPO, "workloads", "firmware", "build.json")
LEAF_SOURCE = os.path.join(
    REPO, "workloads", "firmware", "generated", "src", "mod_000.c"
)
RESULT_DIR = os.path.join(REPO, "bench", "results")
SCRATCH = os.path.join(RESULT_DIR, ".scratch")


def log(msg: str) -> None:
    print(f"[bench] {msg}", flush=True)


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_port(port: int, timeout: float = 15.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.settimeout(0.2)
            try:
                sock.connect(("127.0.0.1", port))
                return
            except OSError:
                time.sleep(0.05)
    raise RuntimeError(f"worker on port {port} did not start")


def ensure_binaries() -> tuple[str, str]:
    worker = os.path.join(REPO, "target", "release", "forge-worker")
    coord = os.path.join(REPO, "target", "release", "forge-coord")
    if not (os.path.exists(worker) and os.path.exists(coord)):
        log("building release binaries (cargo build --release)")
        subprocess.run(["cargo", "build", "--release"], cwd=REPO, check=True)
    return worker, coord


def ensure_workload() -> None:
    """Generate the firmware workload if it is not already present."""
    if os.path.exists(MANIFEST) and os.path.isdir(os.path.join(REPO, "workloads", "firmware", "generated")):
        return
    log("generating firmware workload")
    subprocess.run(
        [sys.executable, os.path.join(REPO, "tools", "gen_workload.py"), "--out", "workloads/firmware"],
        cwd=REPO,
        check=True,
    )
    subprocess.run(
        [sys.executable, os.path.join(REPO, "tools", "gen_manifest.py"), "--workload", "workloads/firmware"],
        cwd=REPO,
        check=True,
    )


def start_workers(worker_bin: str, n: int, slots: int) -> tuple[list, list[str]]:
    procs = []
    addrs = []
    for i in range(n):
        port = free_port()
        workdir = os.path.join(SCRATCH, f"worker{i}")
        shutil.rmtree(workdir, ignore_errors=True)
        os.makedirs(workdir, exist_ok=True)
        proc = subprocess.Popen(
            [
                worker_bin,
                "--listen",
                f"127.0.0.1:{port}",
                "--slots",
                str(slots),
                "--workdir",
                workdir,
                "--id",
                f"w{i}",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        procs.append(proc)
        addrs.append(f"127.0.0.1:{port}")
    for addr in addrs:
        wait_port(int(addr.split(":")[1]))
    return procs, addrs


def stop_workers(procs: list) -> None:
    for proc in procs:
        proc.terminate()
    for proc in procs:
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


def run_build(coord_bin: str, addrs: list[str], jobs: int, extra: list[str]) -> tuple[float, dict]:
    report_path = os.path.join(SCRATCH, "report.json")
    if os.path.exists(report_path):
        os.remove(report_path)
    cmd = [
        coord_bin,
        "build",
        "--manifest",
        MANIFEST,
        "--workers",
        ",".join(addrs),
        "--jobs",
        str(jobs),
        "--report",
        report_path,
        "--log-format",
        "json",
    ] + extra
    start = time.perf_counter()
    proc = subprocess.run(cmd, cwd=REPO, capture_output=True, text=True)
    wall = time.perf_counter() - start
    if proc.returncode != 0:
        sys.stderr.write(proc.stdout)
        sys.stderr.write(proc.stderr)
        raise RuntimeError(f"build failed (rc={proc.returncode})")
    with open(report_path) as fh:
        report = json.load(fh)
    return wall, report


def summarize(name: str, description: str, trials: list[tuple[float, dict]]) -> dict:
    walls = [w for w, _ in trials]
    reports = [r for _, r in trials]
    return {
        "name": name,
        "description": description,
        "trials": [round(w, 4) for w in walls],
        "min_s": round(min(walls), 4),
        "median_s": round(statistics.median(walls), 4),
        "mean_s": round(statistics.mean(walls), 4),
        "cache_hit_rate": round(statistics.mean(r["cache_hit_rate"] for r in reports), 4),
        "actions_executed": int(round(statistics.mean(r["actions_miss"] for r in reports))),
        "achieved_parallelism": round(
            statistics.mean(r["achieved_parallelism"] for r in reports), 3
        ),
        "workers": reports[0]["workers"],
    }


def measure_config(worker_bin, coord_bin, name, description, nworkers, slots, jobs, trials, mode):
    procs, addrs = start_workers(worker_bin, nworkers, slots)
    try:
        results = []
        if mode == "serial":
            for _ in range(trials):
                results.append(run_build(coord_bin, addrs, jobs, ["--no-cache"]))
        elif mode == "cold":
            for _ in range(trials):
                results.append(run_build(coord_bin, addrs, jobs, ["--clean-cache"]))
        elif mode == "warm":
            run_build(coord_bin, addrs, jobs, ["--clean-cache"])
            for _ in range(trials):
                results.append(run_build(coord_bin, addrs, jobs, []))
        elif mode == "incremental":
            with open(LEAF_SOURCE) as fh:
                original = fh.read()
            for trial in range(trials):
                with open(LEAF_SOURCE, "w") as fh:
                    fh.write(original)
                run_build(coord_bin, addrs, jobs, ["--clean-cache"])
                with open(LEAF_SOURCE, "w") as fh:
                    fh.write(original + f"\n/* benchmark edit trial {trial} */\n")
                results.append(run_build(coord_bin, addrs, jobs, []))
                with open(LEAF_SOURCE, "w") as fh:
                    fh.write(original)
        else:
            raise ValueError(mode)
        return summarize(name, description, results)
    finally:
        stop_workers(procs)


def sweep_jobs(worker_bin, coord_bin, jobs_values, nworkers, slots):
    curve = []
    for jobs in jobs_values:
        procs, addrs = start_workers(worker_bin, nworkers, slots)
        try:
            # Warm up process/toolchain, then measure a cold (cache-cleared) build
            # so the curve reflects actual scheduling scale-out.
            run_build(coord_bin, addrs, jobs, ["--clean-cache"])
            wall, report = run_build(coord_bin, addrs, jobs, ["--clean-cache"])
            curve.append(
                {
                    "jobs": jobs,
                    "wall_s": round(wall, 4),
                    "achieved_parallelism": round(report["achieved_parallelism"], 3),
                    "total_cpu_ms": report["total_cpu_ms"],
                }
            )
            log(f"sweep jobs={jobs}: {wall:.3f}s parallelism={report['achieved_parallelism']:.2f}x")
        finally:
            stop_workers(procs)
    return curve


def machine_spec() -> dict:
    def run(cmd):
        try:
            return subprocess.run(cmd, capture_output=True, text=True).stdout.strip()
        except Exception:
            return ""

    cpu = ""
    if sys.platform == "darwin":
        cpu = run(["sysctl", "-n", "machdep.cpu.brand_string"])
    if not cpu:
        cpu = run(["sh", "-c", "grep -m1 'model name' /proc/cpuinfo | cut -d: -f2"]).strip()
    return {
        "cpu": cpu or "unknown",
        "cores": os.cpu_count(),
        "os": run(["uname", "-a"]),
        "platform": sys.platform,
        "rustc": run(["rustc", "--version"]),
        "python": sys.version.split()[0],
    }


def ascii_bar(value: float, maximum: float, width: int = 40) -> str:
    if maximum <= 0:
        return ""
    filled = int(round(width * value / maximum))
    return "#" * max(filled, 0)


def render_report(results: dict) -> str:
    lines = []
    lines.append("# ForgeCache benchmark results")
    lines.append("")
    lines.append(f"_Generated {results['generated_at']} on {results['machine']['cpu']} "
                 f"({results['machine']['cores']} cores)._")
    lines.append("")
    lines.append(f"Workload: **{results['workload']['actions']} actions** "
                 f"({results['workload']['compile']} compile, "
                 f"{results['workload']['link']} link, "
                 f"{results['workload']['sim']} simulation).")
    lines.append("")

    if results.get("configs"):
        baseline = next(c for c in results["configs"] if c["name"] == "serial-local")
        base_min = baseline["min_s"]
        for config in results["configs"]:
            config["speedup_vs_serial"] = round(base_min / config["min_s"], 2)

        lines.append("## Configuration comparison")
        lines.append("")
        lines.append("| config | workers | jobs | trials | min (s) | median (s) | mean (s) | speedup | hit rate | actions executed | parallelism |")
        lines.append("|--------|---------|------|--------|---------|------------|----------|---------|----------|------------------|-------------|")
        for c in results["configs"]:
            lines.append(
                f"| {c['name']} | {len(c['workers'])} | {c.get('jobs', '-')} | {len(c['trials'])} | "
                f"{c['min_s']:.3f} | {c['median_s']:.3f} | {c['mean_s']:.3f} | "
                f"{c['speedup_vs_serial']:.2f}x | {c['cache_hit_rate']*100:.0f}% | "
                f"{c['actions_executed']} | {c['achieved_parallelism']:.2f}x |"
            )
        lines.append("")
        lines.append("## Speedup vs. serial-local")
        lines.append("")
        lines.append("```")
        max_speed = max(c["speedup_vs_serial"] for c in results["configs"])
        for c in results["configs"]:
            label = c["name"].ljust(16)
            bar = ascii_bar(c["speedup_vs_serial"], max_speed)
            lines.append(f"{label} [{bar:<40}] {c['speedup_vs_serial']:.2f}x")
        lines.append("```")
        lines.append("")

    if results.get("sweep"):
        lines.append("## `--sweep-jobs` scaling curve")
        lines.append("")
        lines.append("| jobs | wall (s) | achieved parallelism |")
        lines.append("|------|----------|----------------------|")
        for point in results["sweep"]:
            lines.append(
                f"| {point['jobs']} | {point['wall_s']:.3f} | {point['achieved_parallelism']:.2f}x |"
            )
        lines.append("")
        max_wall = max(p["wall_s"] for p in results["sweep"])
        lines.append("```")
        for point in results["sweep"]:
            label = f"jobs={point['jobs']}".ljust(10)
            bar = ascii_bar(point["wall_s"], max_wall)
            lines.append(f"{label} [{bar:<40}] {point['wall_s']:.3f}s")
        lines.append("```")
        lines.append("")

    lines.append("## Machine and toolchain")
    lines.append("")
    for key, value in results["machine"].items():
        lines.append(f"- **{key}**: {value}")
    lines.append("")
    if results.get("notes"):
        lines.append("## Notes")
        lines.append("")
        for note in results["notes"]:
            lines.append(f"- {note}")
        lines.append("")
    return "\n".join(lines) + "\n"


def workload_counts() -> dict:
    with open(MANIFEST) as fh:
        manifest = json.load(fh)
    counts = {"actions": len(manifest["actions"]), "compile": 0, "link": 0, "sim": 0}
    for action in manifest["actions"]:
        for tag in action.get("tags", []):
            counts[tag] = counts.get(tag, 0) + 1
    return counts


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--workers", type=int, default=min(4, os.cpu_count() or 1))
    parser.add_argument("--slots", type=int, default=None, help="slots per worker")
    parser.add_argument("--jobs", type=int, default=None, help="global jobs cap")
    parser.add_argument("--sweep-jobs", default=None, help="comma-separated jobs values")
    parser.add_argument("--out", default=RESULT_DIR)
    args = parser.parse_args()

    os.makedirs(SCRATCH, exist_ok=True)
    ensure_workload()
    worker_bin, coord_bin = ensure_binaries()

    slots = args.slots or max(1, (os.cpu_count() or 2) // max(args.workers, 1))
    jobs = args.jobs or max(args.workers, 1) * slots

    machine = machine_spec()
    workload = workload_counts()
    log(f"machine: {machine['cpu']} ({machine['cores']} cores)")
    log(f"workload: {workload['actions']} actions; K={args.workers} workers, slots={slots}, jobs={jobs}")

    results = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "machine": machine,
        "workload": workload,
        "notes": [],
        "configs": [],
        "sweep": [],
    }

    if args.sweep_jobs:
        jobs_values = [int(x) for x in args.sweep_jobs.split(",") if x.strip()]
        results["sweep"] = sweep_jobs(worker_bin, coord_bin, jobs_values, args.workers, slots)
        results["notes"].append(
            f"Sweep run with {args.workers} workers, {slots} slots each; "
            "cache cleaned before each measured build."
        )
    else:
        configs = [
            ("serial-local", "single worker, jobs=1, no cache", 1, 1, 1, "serial"),
            ("parallel-cold", f"{args.workers} workers, empty cache", args.workers, slots, jobs, "cold"),
            ("parallel-warm", f"{args.workers} workers, populated cache", args.workers, slots, jobs, "warm"),
            ("incremental", "touch one leaf .c, rebuild", args.workers, slots, jobs, "incremental"),
        ]
        for name, desc, nworkers, wslots, wjobs, mode in configs:
            log(f"running {name} ({desc}) ...")
            summary = measure_config(
                worker_bin, coord_bin, name, desc, nworkers, wslots, wjobs, args.trials, mode
            )
            summary["jobs"] = wjobs
            results["configs"].append(summary)
            log(
                f"  {name}: min={summary['min_s']:.3f}s hit={summary['cache_hit_rate']*100:.0f}% "
                f"executed={summary['actions_executed']}"
            )

    os.makedirs(args.out, exist_ok=True)
    json_path = os.path.join(args.out, "results.json")
    md_path = os.path.join(args.out, "REPORT.md")

    # Merge into any previous results so `--sweep-jobs` can augment the
    # configuration comparison produced by a normal run.
    existing = {}
    if os.path.exists(json_path):
        with open(json_path) as fh:
            existing = json.load(fh)
    merged = dict(existing)
    merged.update(
        {
            "generated_at": results["generated_at"],
            "machine": results["machine"],
            "workload": results["workload"],
        }
    )
    if results["configs"]:
        merged["configs"] = results["configs"]
    if results["sweep"]:
        merged["sweep"] = results["sweep"]
    merged["notes"] = list(
        dict.fromkeys(existing.get("notes", []) + results["notes"])
    )

    with open(json_path, "w") as fh:
        json.dump(merged, fh, indent=2)
        fh.write("\n")
    log(f"wrote {json_path}")

    with open(md_path, "w") as fh:
        fh.write(render_report(merged))
    log(f"wrote {md_path}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
