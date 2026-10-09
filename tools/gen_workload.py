#!/usr/bin/env python3
"""Generate a synthetic firmware-like C target tree for ForgeCache.

The tree is deliberately non-trivial: every module includes a couple of other
modules' headers (so compile actions have real header inputs), there is a single
firmware entry point, and a deterministic simulation harness that can be driven
by setting SIM_SEED / SIM_TICKS.

Only the Python standard library is used.

Example:
    python3 tools/gen_workload.py --modules 60 --fanout 3 --out workloads/firmware
"""

from __future__ import annotations

import argparse
import os

HEADER_TEMPLATE = """#ifndef MOD_{i:03d}_H
#define MOD_{i:03d}_H

#define MOD_{i:03d}_BIAS {i}UL

unsigned long mod_{i:03d}_step(unsigned long x);

#endif /* MOD_{i:03d}_H */
"""

SOURCE_TEMPLATE = """/* Generated module {i:03d}: depends on {deps}. */
#include "mod_{i:03d}.h"
{dep_includes}
unsigned long mod_{i:03d}_step(unsigned long x) {{
    x = x * 1103515245UL + 12345UL + (MOD_{i:03d}_BIAS{dep_biases});
    x ^= (x >> 13);
    x = x * 2654435761UL;
    x += {i}UL;
    return x;
}}
"""

FIRMWARE_H = """#ifndef FIRMWARE_H
#define FIRMWARE_H

/* Deterministic firmware entry point: run `ticks` simulation steps from `seed`. */
unsigned long firmware_run(unsigned long seed, long ticks);

#endif /* FIRMWARE_H */
"""

MAIN_TEMPLATE = """/* Firmware entry logic. No main() here: the simulation harness supplies it. */
#include "firmware.h"
{module_includes}
unsigned long firmware_run(unsigned long seed, long ticks) {{
    unsigned long x = seed ^ 0x9E3779B97F4A7C15UL;
    for (long t = 0; t < ticks; ++t) {{
{xor_lines}
    }}
    return x;
}}
"""

SIM_RUNNER = """/* Deterministic simulation harness: reads SIM_SEED and SIM_TICKS, prints a
 * checksum. Same seed/ticks always produce byte-identical stdout, which is what
 * makes ForgeCache cache hits provably correct for simulation actions. */
#include <stdio.h>
#include <stdlib.h>

#include "firmware.h"

int main(void) {
    const char *s = getenv("SIM_SEED");
    const char *t = getenv("SIM_TICKS");
    unsigned long seed = s ? strtoul(s, NULL, 10) : 1UL;
    long ticks = t ? strtol(t, NULL, 10) : 1000L;
    unsigned long checksum = firmware_run(seed, ticks);
    printf("seed=%lu ticks=%ld checksum=%lu\\n", seed, ticks, checksum);
    return 0;
}
"""


def deps_for(i: int, fanout: int) -> list[int]:
    """Return up to `fanout` distinct earlier module indices."""
    deps: list[int] = []
    for k in range(1, fanout + 1):
        d = i - (2 * k - 1)
        if d >= 0:
            deps.append(d)
    return sorted(set(deps))


def generate(modules: int, fanout: int, out: str) -> None:
    gen = os.path.join(out, "generated")
    inc = os.path.join(gen, "include")
    src = os.path.join(gen, "src")
    sim = os.path.join(gen, "sim")
    for d in (inc, src, sim):
        os.makedirs(d, exist_ok=True)

    with open(os.path.join(inc, "firmware.h"), "w") as fh:
        fh.write(FIRMWARE_H)

    module_includes = []
    xor_lines = []
    for i in range(modules):
        deps = deps_for(i, fanout)
        dep_includes = "".join(f'#include "mod_{d:03d}.h"\n' for d in deps)
        dep_biases = "".join(f" + MOD_{d:03d}_BIAS" for d in deps)

        with open(os.path.join(inc, f"mod_{i:03d}.h"), "w") as fh:
            fh.write(HEADER_TEMPLATE.format(i=i))
        with open(os.path.join(src, f"mod_{i:03d}.c"), "w") as fh:
            fh.write(
                SOURCE_TEMPLATE.format(
                    i=i, deps=deps, dep_includes=dep_includes, dep_biases=dep_biases
                )
            )

        module_includes.append(f'#include "mod_{i:03d}.h"')
        xor_lines.append(f"        x = mod_{i:03d}_step(x);")

    with open(os.path.join(src, "main.c"), "w") as fh:
        fh.write(
            MAIN_TEMPLATE.format(
                module_includes="\n".join(module_includes),
                xor_lines="\n".join(xor_lines),
            )
        )
    with open(os.path.join(sim, "sim_runner.c"), "w") as fh:
        fh.write(SIM_RUNNER)

    total_sources = modules + 2
    print(f"generated {modules} modules (+ main.c, sim_runner.c) = {total_sources} C sources")
    print(f"output tree: {gen}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--modules", type=int, default=60, help="number of modules")
    parser.add_argument("--fanout", type=int, default=3, help="headers included per module")
    parser.add_argument("--out", default="workloads/firmware", help="output directory")
    args = parser.parse_args()
    if args.modules < 1:
        parser.error("--modules must be >= 1")
    generate(args.modules, args.fanout, args.out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
