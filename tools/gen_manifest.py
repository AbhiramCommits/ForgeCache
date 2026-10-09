#!/usr/bin/env python3
"""Emit a ForgeCache manifest for the generated firmware tree.

Walks a tree produced by ``tools/gen_workload.py`` and writes ``build.json``
with:

* one compile action per ``.c`` file (``gcc -c``; inputs = source + every local
  header it includes; output = a ``.o``),
* one link action (all ``.o`` -> ``build/firmware.elf``), and
* a matrix of deterministic simulation actions driven by SIM_SEED / SIM_TICKS,
  each depending on the link output.

A ``--check`` mode re-runs every simulation action twice and compares stdout
digests, proving the simulation is deterministic (which is what makes cache hits
correct).

Only the Python standard library is used.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys

INCLUDE_RE = re.compile(r'#\s*include\s*"([^"]+)"')

# (seed, ticks) -> 8 deterministic simulation configurations.
SIM_CONFIGS = [(seed, ticks) for seed in (1, 2, 3, 4) for ticks in (1000, 5000)]

GCC = "gcc"
INCLUDE_DIR = "generated/include"
SRC_DIR = "generated/src"
SIM_DIR = "generated/sim"
BUILD_DIR = "build"


def local_headers(source_path: str, workload: str) -> list[str]:
    """Return workload-relative local headers included by ``source_path``."""
    with open(os.path.join(workload, source_path)) as fh:
        text = fh.read()
    headers = []
    for match in INCLUDE_RE.finditer(text):
        candidate = os.path.join(INCLUDE_DIR, match.group(1))
        if os.path.exists(os.path.join(workload, candidate)):
            headers.append(candidate)
    return sorted(set(headers))


def find_sources(workload: str) -> list[str]:
    sources: list[str] = []
    for subdir in (SRC_DIR, SIM_DIR):
        full = os.path.join(workload, subdir)
        if not os.path.isdir(full):
            continue
        for name in sorted(os.listdir(full)):
            if name.endswith(".c"):
                sources.append(os.path.join(subdir, name))
    return sources


def compile_actions(workload: str) -> list[dict]:
    actions = []
    for source in find_sources(workload):
        stem = os.path.basename(source)[:-2]
        obj = os.path.join(BUILD_DIR, stem + ".o")
        inputs = sorted(set([source] + local_headers(source, workload)))
        actions.append(
            {
                "id": f"compile:{stem}",
                "command": [GCC, "-c", "-O1", "-I", INCLUDE_DIR, source, "-o", obj],
                "env": {},
                "inputs": inputs,
                "outputs": [obj],
                "deps": [],
                "timeout_ms": 120000,
                "tags": ["compile"],
            }
        )
    return actions


def link_action(workload: str) -> dict:
    objects = []
    deps = []
    for source in find_sources(workload):
        stem = os.path.basename(source)[:-2]
        objects.append(os.path.join(BUILD_DIR, stem + ".o"))
        deps.append(f"compile:{stem}")
    elf = os.path.join(BUILD_DIR, "firmware.elf")
    return {
        "id": "link",
        "command": [GCC, "-o", elf] + objects,
        "env": {},
        "inputs": list(objects),
        "outputs": [elf],
        "deps": deps,
        "timeout_ms": 120000,
        "tags": ["link"],
    }


def sim_actions() -> list[dict]:
    actions = []
    for seed, ticks in SIM_CONFIGS:
        actions.append(
            {
                "id": f"sim:{seed}:{ticks}",
                "command": [
                    "/bin/sh",
                    "-c",
                    "chmod +x build/firmware.elf && ./build/firmware.elf",
                ],
                "env": {"SIM_SEED": str(seed), "SIM_TICKS": str(ticks)},
                "inputs": [os.path.join(BUILD_DIR, "firmware.elf")],
                "outputs": [],
                "deps": ["link"],
                "timeout_ms": 120000,
                "tags": ["sim"],
            }
        )
    return actions


def build_manifest(workload: str) -> dict:
    return {
        "actions": compile_actions(workload) + [link_action(workload)] + sim_actions()
    }


def check_determinism(workload: str) -> int:
    elf = os.path.join(workload, BUILD_DIR, "firmware.elf")
    if not os.path.exists(elf):
        print(f"[check] {elf} not found; run a build first", file=sys.stderr)
        return 2
    ok = True
    print("[check] re-running each simulation action twice and comparing stdout")
    for seed, ticks in SIM_CONFIGS:
        env = dict(os.environ)
        env["SIM_SEED"] = str(seed)
        env["SIM_TICKS"] = str(ticks)
        outputs = []
        for _ in range(2):
            proc = subprocess.run(
                ["/bin/sh", "-c", "chmod +x build/firmware.elf && ./build/firmware.elf"],
                cwd=workload,
                env=env,
                capture_output=True,
            )
            outputs.append((proc.returncode, proc.stdout))
        digest_a = hashlib.sha256(outputs[0][1]).hexdigest()[:16]
        digest_b = hashlib.sha256(outputs[1][1]).hexdigest()[:16]
        same = outputs[0] == outputs[1]
        ok = ok and same
        status = "OK " if same else "FAIL"
        print(
            f"  [{status}] sim:{seed}:{ticks} stdout={outputs[0][1].decode().strip()!r} "
            f"digest={digest_a} (rerun {digest_b})"
        )
    print(f"[check] {'all simulation actions deterministic' if ok else 'NON-DETERMINISM DETECTED'}")
    return 0 if ok else 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workload", default="workloads/firmware", help="workload directory")
    parser.add_argument("--out", default=None, help="manifest path (default <workload>/build.json)")
    parser.add_argument("--check", action="store_true", help="check simulation determinism")
    args = parser.parse_args()

    workload = args.workload
    if not os.path.isdir(workload):
        print(f"workload directory not found: {workload}", file=sys.stderr)
        return 2

    if args.check:
        return check_determinism(workload)

    manifest = build_manifest(workload)
    out = args.out or os.path.join(workload, "build.json")
    with open(out, "w") as fh:
        json.dump(manifest, fh, indent=2, sort_keys=False)
        fh.write("\n")

    counts = {}
    for action in manifest["actions"]:
        for tag in action["tags"]:
            counts[tag] = counts.get(tag, 0) + 1
    print(f"wrote {out} with {len(manifest['actions'])} actions {counts}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
