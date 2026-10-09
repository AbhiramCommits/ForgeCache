# ForgeCache

ForgeCache is an async Rust distributed build and simulation executor in the
spirit of Bazel/buck2 remote execution. It reads a manifest of hermetic actions
(compile C files, link, run a simulation), derives a content-addressed digest for
each action from its command, environment, input file contents, and dependency
digests, and uses a content-addressed store (CAS) plus an action cache to skip
unchanged work entirely. A tokio coordinator schedules ready actions across N
workers with bounded concurrency, per-action timeouts, bounded retries, and
worker ejection, while workers execute actions as sandboxed subprocesses (or in
Docker). The workload is a synthetic firmware tree whose deterministic
simulation matrix makes cache hits provably correct.

## Architecture

```
                         ┌────────────────────────────┐
   build.json ──────────▶│        forge-coord         │
   (actions + deps)      │  planner  +  scheduler     │
                         │                            │
                         │  • topo digest computation │
                         │  • action-cache probe      │
                         │  • bounded in-flight queue │
                         │  • timeouts / retries      │
                         │  • worker ejection         │
                         └───────┬──────────┬─────────┘
                      length-prefixed JSON RPC over TCP
             ┌───────────────────┼──────────────┬───────────────────┐
             ▼                   ▼              ▼                   ▼
      ┌────────────┐     ┌────────────┐  ┌────────────┐      ┌────────────┐
      │ worker 1   │     │ worker 2   │  │ worker 3   │ ...  │ worker N   │
      │ slots=S    │     │ slots=S    │  │ slots=S    │      │ slots=S    │
      │ local/dock │     │ local/dock │  │ local/dock │      │ local/dock │
      └─────┬──────┘     └─────┬──────┘  └─────┬──────┘      └─────┬──────┘
            │                  │               │                   │
            └──────────────────┴───────────────┴───────────────────┘
                                       │
                       ┌───────────────┴────────────────┐
                       ▼                                ▼
              .forge/cas/<aa>/<digest>          .forge/ac/<aa>/<digest>.json
              content-addressed blobs           action digest → ActionResult
```

### Hermetic action model

An `Action` (`crates/forge-core/src/action.rs`) declares an `id`, a `command`
argv, `env`, input files, output files, `deps` (other action ids), a
`timeout_ms`, and free-form `tags`. Actions are hermetic: each runs in a fresh
scratch directory with only its declared inputs materialized, a fixed `PATH`, and
the action's own environment.

### How digests are computed

`action_digest` is BLAKE3 over a canonical, deterministic serialization, in this
order, each field length-prefixed to avoid ambiguity:

1. a schema tag (`forgecache-action-v1`),
2. the command argv,
3. the environment as sorted `KEY=VALUE` pairs (`BTreeMap` is ordered),
4. the inputs sorted by path, each as `path` + the BLAKE3 digest of its bytes,
5. the dependency digests in sorted order.

The action `id` is deliberately excluded, so two identically-described actions
under different labels share a cache entry. Because every component is sorted and
file contents are hashed, **digests are stable across runs and machines**.

A parent's digest is computed only after its dependencies finish, because a
parent's inputs may be a dependency's outputs; the output bytes must exist before
they can be hashed. The CAS materializes blobs by hard link (falling back to a
copy across filesystems), so restoring an output is cheap and byte-identical.

### Scheduling model

- Work becomes *ready* when all of its dependencies complete; digests are
  computed bottom-up at that point.
- Before dispatch the action cache is probed. A hit restores outputs from the CAS
  and records `hit` with no worker round trip.
- Global in-flight is capped by `--jobs`; per-worker in-flight is capped by the
  worker's advertised slots (`WorkerPool` in `crates/forge-coord/src/build.rs`),
  so the ready queue never grows unbounded.
- Each action has a timeout; on expiry the worker kills the whole process group.
- Retryable failures (transport errors, timeouts, worker faults) are retried up
  to two times with exponential backoff plus jitter, possibly on another worker.
  A genuine non-zero process exit is a build failure and is **not** retried.
- After N consecutive faults a worker is ejected; if every worker is gone and
  `--local-fallback` is set, actions run in-process.

### Cache-correctness invariant

> If an action's command, environment, input bytes, and dependency digests are
> unchanged, its digest is unchanged, its cache entry is valid, and restoring its
> declared outputs from the CAS reproduces the exact bytes the action originally
> produced.

Simulation actions are deterministic: the same `SIM_SEED`/`SIM_TICKS` always
produce the same stdout, so a cache hit is provably correct. The test suite and
`tools/gen_manifest.py --check` both assert this.

## How to run

Prerequisites: Rust (2021, tested on 1.85), a C compiler (`gcc`/`clang`),
Python 3 (stdlib only). Docker and `kubectl`/`kubeconform` are optional.

```sh
# Build everything.
cargo build --release

# Generate the synthetic firmware workload (60 modules) and its manifest.
make workload          # or:
python3 tools/gen_workload.py --modules 60 --fanout 3 --out workloads/firmware
python3 tools/gen_manifest.py --workload workloads/firmware

# Start two workers.
./target/release/forge-worker --listen 127.0.0.1:7701 --slots 4 \
    --workdir /tmp/forge-worker --id w1 &
./target/release/forge-worker --listen 127.0.0.1:7702 --slots 4 \
    --workdir /tmp/forge-worker --id w2 &

# Cold build (populates the cache).
./target/release/forge-coord build --manifest workloads/firmware/build.json \
    --workers 127.0.0.1:7701,127.0.0.1:7702 --jobs 8 --clean-cache \
    --report /tmp/cold.json

# Warm build (100% cache hits).
./target/release/forge-coord build --manifest workloads/firmware/build.json \
    --workers 127.0.0.1:7701,127.0.0.1:7702 --jobs 8

# Explain why a target would rebuild.
./target/release/forge-coord explain --manifest workloads/firmware/build.json \
    --target link

# Docker: one coordinator + three workers on a shared network, then one build.
make up                # docker compose up --build --abort-on-container-exit
make down

# Kubernetes (no cluster needed to validate).
kubeconform -summary -strict k8s/
kubectl apply -f k8s/   # on a live cluster

# Benchmark harness (writes bench/results/results.json + REPORT.md).
python3 bench/run_bench.py --trials 3
python3 bench/run_bench.py --sweep-jobs 1,2,4,8
```

`--log-format json` switches the coordinator to structured JSON logs.
`cargo run -p forge-worker --example ping_exec` starts a worker and runs one
action end to end.

## RESULTS

All figures below were measured on this machine by the committed
`bench/run_bench.py`; the raw data is in
[`bench/results/results.json`](bench/results/results.json) and rendered in
[`bench/results/REPORT.md`](bench/results/REPORT.md).

**Machine:** Apple M3 (8 cores) · Darwin 27.0.0 (arm64) · rustc 1.85.0 ·
Python 3.13.9. Runs used the release binaries.

**Workload:** 71 actions = 62 compile (`gcc -c`) + 1 link + 8 simulation,
K = 4 workers with 2 slots each, `--jobs 8`.

| config | workers | jobs | trials | min (s) | median (s) | mean (s) | speedup vs serial | hit rate | actions executed | parallelism |
|--------|---------|------|--------|---------|------------|----------|-------------------|----------|------------------|-------------|
| serial-local  | 1 | 1 | 3 | 2.013 | 2.021 | 2.152 | 1.00x  | 0%   | 71 | 0.91x |
| parallel-cold | 4 | 8 | 3 | 0.549 | 0.605 | 0.590 | 3.67x  | 0%   | 71 | 5.89x |
| parallel-warm | 4 | 8 | 3 | 0.034 | 0.036 | 0.037 | 58.67x | 100% | 0  | 0.00x |
| incremental   | 4 | 8 | 3 | 0.169 | 0.173 | 0.179 | 11.89x | 86%  | 10 | 3.22x |

- **Cache hit rate:** 100% on the warm build (71/71); 86% on the incremental
  build (61/71).
- **Incremental rebuild:** editing one leaf `.c` re-ran exactly **10** actions —
  that module's compile, the link, and the 8 simulations. Nothing else rebuilt.
- **`--sweep-jobs` scaling curve** (cold builds, 4 workers × 2 slots):

  | jobs | wall (s) | achieved parallelism |
  |------|----------|----------------------|
  | 1 | 2.079 | 0.89x |
  | 2 | 1.111 | 1.68x |
  | 4 | 0.735 | 3.12x |
  | 8 | 0.637 | 5.77x |

- **Docker:** both images built
  (`docker/Dockerfile.worker`, `docker/Dockerfile.coord`). `docker compose up
  --abort-on-container-exit` connected the coordinator to 3 worker containers and
  ran a cold build of all 71 actions in **314 ms** at **3.02x** parallelism
  (`bench/results/docker-report.json`).
- **Kubernetes:** `k8s/` manifests validated with `kubeconform -strict`
  (3 resources, 3 valid, 0 errors). No cluster was available for a live apply —
  `kubectl --dry-run=client` requires API discovery and could not reach a server.
- **Tests:** `scripts/ci-local.sh` passes with **51 tests**.

## Limitations and next steps

- **No authentication or transport security.** The RPC is plaintext TCP with no
  TLS and no worker/coordinator authentication.
- **No CAS garbage collection.** Blobs and action-cache entries accumulate
  forever; there is no LRU/refcount eviction.
- **Custom JSON protocol, not gRPC/REAPI.** The wire format is a simple
  length-prefixed JSON protocol, not the Remote Execution API, so it does not
  interoperate with Bazel/buck2 remote backends.
- **Inputs are sent inline.** Input blobs travel with each request instead of
  being fetched from a shared CAS by digest; fine for small targets, wasteful for
  large ones.
- **Timestamp-based incremental edits only re-run when content changes.** The
  cache keys on content, so `touch` alone is a no-op by design.
- **Docker sandboxing is process-level, not hermetic at the kernel level.** Local
  execution uses process groups and rlimits-style timeouts; it does not use
  namespaces or seccomp on macOS.
- **JSON reports are not streaming.** Large graphs materialize the full action
  list in memory.
