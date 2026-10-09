//! Coordinator integration tests: cold/warm caching, precise invalidation, and
//! resilience to a worker dying mid-build.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use forge_coord::{run_build, ActionStatus, BuildOptions};
use forge_worker::{start, WorkerConfig};

fn action(
    id: &str,
    command: Vec<String>,
    inputs: &[&str],
    outputs: &[&str],
    deps: &[&str],
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "command": command,
        "env": {},
        "inputs": inputs,
        "outputs": outputs,
        "deps": deps,
        "timeout_ms": 30000,
        "tags": []
    })
}

fn write_manifest(dir: &Path, actions: Vec<serde_json::Value>) -> PathBuf {
    let path = dir.join("build.json");
    let manifest = serde_json::json!({ "actions": actions });
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    path
}

async fn boot(workdir: &Path, slots: u32) -> forge_worker::RunningWorker {
    start(
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        WorkerConfig::local("w", slots, workdir),
    )
    .await
    .unwrap()
}

fn shell(script: &str) -> Vec<String> {
    vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()]
}

/// A four-action pipeline: two compiles, a link, and a sim.
fn pipeline_manifest(dir: &Path) -> PathBuf {
    std::fs::write(dir.join("src1.txt"), "alpha-v1").unwrap();
    std::fs::write(dir.join("src2.txt"), "beta-v1").unwrap();
    let actions = vec![
        action(
            "compile:1",
            shell("cp src1.txt obj1.o"),
            &["src1.txt"],
            &["obj1.o"],
            &[],
        ),
        action(
            "compile:2",
            shell("cp src2.txt obj2.o"),
            &["src2.txt"],
            &["obj2.o"],
            &[],
        ),
        action(
            "link",
            shell("cat obj1.o obj2.o > firmware.elf"),
            &["obj1.o", "obj2.o"],
            &["firmware.elf"],
            &["compile:1", "compile:2"],
        ),
        action(
            "sim",
            shell("cat firmware.elf > sim.out"),
            &["firmware.elf"],
            &["sim.out"],
            &["link"],
        ),
    ];
    write_manifest(dir, actions)
}

fn misses(report: &forge_coord::Report) -> HashSet<String> {
    report
        .actions
        .iter()
        .filter(|a| a.status == ActionStatus::Miss)
        .map(|a| a.id.clone())
        .collect()
}

#[tokio::test]
async fn cold_then_warm_is_all_hits() {
    let dir = tempfile::tempdir().unwrap();
    let worker = boot(&dir.path().join("scratch"), 4).await;
    let manifest = pipeline_manifest(dir.path());

    let mut options = BuildOptions::new(&manifest, vec![worker.addr()]);
    options.jobs = 4;

    let cold = run_build(&options).await.unwrap();
    assert_eq!(cold.actions_total, 4);
    assert_eq!(cold.actions_miss, 4);
    assert_eq!(cold.actions_hit, 0);

    let warm = run_build(&options).await.unwrap();
    assert_eq!(warm.actions_hit, 4, "warm build should be all hits");
    assert_eq!(warm.actions_miss, 0, "warm build must execute zero actions");
    assert!((warm.cache_hit_rate - 1.0).abs() < f64::EPSILON);

    worker.shutdown().await;
}

#[tokio::test]
async fn touching_a_leaf_invalidates_exactly_its_dependents() {
    let dir = tempfile::tempdir().unwrap();
    let worker = boot(&dir.path().join("scratch"), 4).await;
    let manifest = pipeline_manifest(dir.path());

    let mut options = BuildOptions::new(&manifest, vec![worker.addr()]);
    options.jobs = 4;
    run_build(&options).await.unwrap();

    // Change exactly one leaf source.
    std::fs::write(dir.path().join("src1.txt"), "alpha-v2-EDITED").unwrap();

    let report = run_build(&options).await.unwrap();
    let expected: HashSet<String> = ["compile:1", "link", "sim"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(misses(&report), expected);
    assert_eq!(report.actions_hit, 1, "compile:2 should still hit");

    worker.shutdown().await;
}

#[tokio::test]
async fn worker_death_mid_build_retries_and_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch");
    let worker_a = boot(&scratch.join("a"), 2).await;
    let worker_b = boot(&scratch.join("b"), 2).await;
    let addr_a = worker_a.addr();
    let addr_b = worker_b.addr();

    // Six independent, slow-ish actions so the build is still running when we
    // kill worker A.
    let mut actions = Vec::new();
    for i in 0..6 {
        actions.push(action(
            &format!("job{i}"),
            shell("sleep 0.2; printf done > out.txt"),
            &[],
            &["out.txt"],
            &[],
        ));
    }
    let manifest = write_manifest(dir.path(), actions);

    let killer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        worker_a.shutdown().await;
    });

    let mut options = BuildOptions::new(&manifest, vec![addr_a, addr_b]);
    options.jobs = 4;
    options.eject_after = 1;
    let report = run_build(&options)
        .await
        .expect("build survives worker death");
    assert_eq!(report.actions_miss, 6);

    killer.await.unwrap();
    worker_b.shutdown().await;
}

#[tokio::test]
async fn local_fallback_runs_without_workers() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = pipeline_manifest(dir.path());
    let mut options = BuildOptions::new(&manifest, vec![]);
    options.local_fallback = true;
    let report = run_build(&options).await.unwrap();
    assert_eq!(report.actions_miss, 4);
}

#[tokio::test]
async fn no_cache_forces_execution_every_time() {
    let dir = tempfile::tempdir().unwrap();
    let worker = boot(&dir.path().join("scratch"), 4).await;
    let manifest = pipeline_manifest(dir.path());
    let mut options = BuildOptions::new(&manifest, vec![worker.addr()]);
    options.no_cache = true;

    let first = run_build(&options).await.unwrap();
    let second = run_build(&options).await.unwrap();
    assert_eq!(first.actions_miss, 4);
    assert_eq!(second.actions_miss, 4);
    assert_eq!(second.actions_hit, 0);

    worker.shutdown().await;
}

#[tokio::test]
async fn report_json_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let worker = boot(&dir.path().join("scratch"), 4).await;
    let manifest = pipeline_manifest(dir.path());
    let report_path = dir.path().join("report.json");
    let mut options = BuildOptions::new(&manifest, vec![worker.addr()]);
    options.report = Some(report_path.clone());
    run_build(&options).await.unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
    assert_eq!(value["actions_total"], 4);
    assert!(value["cache_hit_rate"].is_number());
    worker.shutdown().await;
}
