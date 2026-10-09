# ForgeCache benchmark results

_Generated 2026-10-09T21:54:33.847761+00:00 on Apple M3 (8 cores)._

Workload: **71 actions** (62 compile, 1 link, 8 simulation).

## Configuration comparison

| config | workers | jobs | trials | min (s) | median (s) | mean (s) | speedup | hit rate | actions executed | parallelism |
|--------|---------|------|--------|---------|------------|----------|---------|----------|------------------|-------------|
| serial-local | 1 | 1 | 3 | 2.013 | 2.021 | 2.152 | 1.00x | 0% | 71 | 0.91x |
| parallel-cold | 4 | 8 | 3 | 0.549 | 0.605 | 0.590 | 3.67x | 0% | 71 | 5.89x |
| parallel-warm | 4 | 8 | 3 | 0.034 | 0.036 | 0.036 | 58.67x | 100% | 0 | 0.00x |
| incremental | 4 | 8 | 3 | 0.169 | 0.173 | 0.179 | 11.89x | 86% | 10 | 3.22x |

## Speedup vs. serial-local

```
serial-local     [#                                       ] 1.00x
parallel-cold    [###                                     ] 3.67x
parallel-warm    [########################################] 58.67x
incremental      [########                                ] 11.89x
```

## `--sweep-jobs` scaling curve

| jobs | wall (s) | achieved parallelism |
|------|----------|----------------------|
| 1 | 2.079 | 0.89x |
| 2 | 1.111 | 1.68x |
| 4 | 0.735 | 3.12x |
| 8 | 0.636 | 5.77x |

```
jobs=1     [########################################] 2.079s
jobs=2     [#####################                   ] 1.111s
jobs=4     [##############                          ] 0.735s
jobs=8     [############                            ] 0.636s
```

## Machine and toolchain

- **cpu**: Apple M3
- **cores**: 8
- **os**: Darwin Abhirams-MacBook-Air.local 27.0.0 Darwin Kernel Version 27.0.0: Tue Aug 11 21:05:41 PDT 2026; root:xnu-13432.1.9~1/RELEASE_ARM64_T8122 arm64
- **platform**: darwin
- **rustc**: rustc 1.85.0 (4d91de4e4 2025-02-17)
- **python**: 3.13.9

## Notes

- Docker: `forgecache-worker:latest` and `forgecache-coord:latest` built; `docker compose up --abort-on-container-exit` ran one cold build across 3 worker containers (71 actions, 0 hit / 71 miss, 314 ms, parallelism 3.02x). See bench/results/docker-report.json.
- Kubernetes: k8s/ manifests validated with `kubeconform -strict` (3 resources, 3 valid). No cluster was available for a live apply; `kubectl --dry-run=client` requires API discovery and could not reach a server.
- Sweep run with 4 workers, 2 slots each; cache cleaned before each measured build.

