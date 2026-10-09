//! End-to-end cache-correctness test.
//!
//! Generates a 12-module firmware workload with the Python tools, then runs
//! cold, warm, and incremental builds through the real library API against two
//! in-process workers, asserting:
//!
//! * the warm build is a 100% cache hit and executes zero actions,
//! * an incremental edit to one leaf re-runs exactly that leaf's compile action,
//!   the link action, and the simulation matrix — nothing else,
//! * every simulation action's stdout digest is identical across all three runs.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;

use forge_coord::{run_build, ActionStatus, BuildOptions, Report};
use forge_worker::{start, WorkerConfig};

const MODULES: usize = 12;
const SIM_CONFIGS: [(u32, u32); 8] = [
    (1, 1000),
    (1, 5000),
    (2, 1000),
    (2, 5000),
    (3, 1000),
    (3, 5000),
    (4, 1000),
    (4, 5000),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn run_python(repo: &Path, args: &[&str]) {
    let output = Command::new("python3")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("python3 available");
    assert!(
        output.status.success(),
        "python3 {:?} failed:\n{}\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout_digest(report: &Report, id: &str) -> String {
    report
        .actions
        .iter()
        .find(|a| a.id == id)
        .unwrap_or_else(|| panic!("action {id} present in report"))
        .stdout_digest
        .clone()
}

fn misses(report: &Report) -> HashSet<String> {
    report
        .actions
        .iter()
        .filter(|a| a.status == ActionStatus::Miss)
        .map(|a| a.id.clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_warm_incremental_are_cache_correct() {
    let repo = repo_root();
    let tmp = tempfile::tempdir().unwrap();
    let workload = tmp.path().join("firmware");
    std::fs::create_dir_all(&workload).unwrap();

    run_python(
        &repo,
        &[
            "tools/gen_workload.py",
            "--modules",
            &MODULES.to_string(),
            "--fanout",
            "2",
            "--out",
            workload.to_str().unwrap(),
        ],
    );
    run_python(
        &repo,
        &[
            "tools/gen_manifest.py",
            "--workload",
            workload.to_str().unwrap(),
        ],
    );
    let manifest = workload.join("build.json");

    let scratch = tmp.path().join("scratch");
    let worker_a = start(
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        WorkerConfig::local("e2e-a", 2, scratch.join("a")),
    )
    .await
    .unwrap();
    let worker_b = start(
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        WorkerConfig::local("e2e-b", 2, scratch.join("b")),
    )
    .await
    .unwrap();

    let mut options = BuildOptions::new(&manifest, vec![worker_a.addr(), worker_b.addr()]);
    options.jobs = 4;

    // 12 modules + main.c + sim_runner.c = 14 compiles, 1 link, 8 sims.
    let expected_total = MODULES + 2 + 1 + SIM_CONFIGS.len();

    let cold = run_build(&options).await.unwrap();
    assert_eq!(cold.actions_total, expected_total);
    assert_eq!(
        cold.actions_miss, expected_total,
        "cold build executes everything"
    );
    assert_eq!(cold.actions_hit, 0);

    let warm = run_build(&options).await.unwrap();
    assert_eq!(warm.actions_hit, expected_total, "warm build is 100% hits");
    assert_eq!(warm.actions_miss, 0, "warm build executes zero actions");
    assert!((warm.cache_hit_rate - 1.0).abs() < f64::EPSILON);

    // Incremental: edit one leaf with a semantically-neutral comment. The object
    // code is unchanged, but the compile input (and therefore its digest, the
    // link dep digest, and the sim dep digests) change.
    let leaf = workload.join("generated/src/mod_000.c");
    let original = std::fs::read_to_string(&leaf).unwrap();
    std::fs::write(
        &leaf,
        format!("{original}\n/* e2e incremental comment */\n"),
    )
    .unwrap();

    let incremental = run_build(&options).await.unwrap();
    let mut expected_misses: HashSet<String> = HashSet::new();
    expected_misses.insert("compile:mod_000".to_string());
    expected_misses.insert("link".to_string());
    for (seed, ticks) in SIM_CONFIGS {
        expected_misses.insert(format!("sim:{seed}:{ticks}"));
    }
    assert_eq!(
        misses(&incremental),
        expected_misses,
        "incremental edit must invalidate exactly the leaf, link, and sims"
    );

    // All simulation checksums are identical across cold, warm, and incremental.
    for (seed, ticks) in SIM_CONFIGS {
        let id = format!("sim:{seed}:{ticks}");
        let cold_digest = stdout_digest(&cold, &id);
        let warm_digest = stdout_digest(&warm, &id);
        let incr_digest = stdout_digest(&incremental, &id);
        assert!(!cold_digest.is_empty(), "{id} produced stdout");
        assert_eq!(
            cold_digest, warm_digest,
            "{id} checksum changed on warm hit"
        );
        assert_eq!(
            cold_digest, incr_digest,
            "{id} checksum changed on incremental"
        );
    }

    std::fs::write(&leaf, original).unwrap();
    worker_a.shutdown().await;
    worker_b.shutdown().await;
}
