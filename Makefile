# ForgeCache developer entry points.
.PHONY: build release test fmt lint ci workload smoke bench up down clean

# Debug build of the whole workspace.
build:
	cargo build

# Optimized build (used by the benchmark harness).
release:
	cargo build --release

test:
	cargo test --workspace

fmt:
	cargo fmt --all

lint:
	cargo clippy --all-targets -- -D warnings

# Full local CI (fmt check, clippy, tests, cold+warm smoke build).
ci:
	bash scripts/ci-local.sh

# Generate the synthetic firmware workload (60 modules) and its manifest.
workload:
	python3 tools/gen_workload.py --modules 60 --fanout 3 --out workloads/firmware
	python3 tools/gen_manifest.py --workload workloads/firmware

# Cold + warm build smoke test against one local worker.
smoke: build
	bash scripts/smoke_build.sh 12 95

# Full benchmark (serial-local vs parallel-cold vs parallel-warm vs incremental).
bench:
	python3 bench/run_bench.py --trials 3

# Bring up the docker-compose cluster and run one build through it.
up:
	docker compose up --build --abort-on-container-exit

down:
	docker compose down --remove-orphans

clean:
	cargo clean
	rm -rf workloads/firmware/generated workloads/firmware/build workloads/firmware/build.json
	rm -rf bench/results/.scratch bench/results/results.json bench/results/REPORT.md
