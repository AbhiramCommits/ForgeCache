//! Integration tests that boot a real worker on an ephemeral port and drive it
//! through the wire protocol.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::{action_digest_at, Action, WorkerClient};
use forge_worker::{start, WorkerConfig};

fn action(id: &str, argv: &[&str], outputs: &[&str], timeout_ms: u64) -> Action {
    Action {
        id: id.to_string(),
        command: argv.iter().map(|s| s.to_string()).collect(),
        env: BTreeMap::new(),
        inputs: vec![],
        outputs: outputs.iter().map(PathBuf::from).collect(),
        deps: vec![],
        timeout_ms,
        tags: vec![],
    }
}

async fn boot(slots: u32, dir: &Path) -> forge_worker::RunningWorker {
    start(
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        WorkerConfig::local("test-worker", slots, dir),
    )
    .await
    .expect("worker starts")
}

#[tokio::test]
async fn echo_action_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let worker = boot(2, dir.path()).await;
    let client = WorkerClient::connect(worker.addr()).await.unwrap();
    assert_eq!(client.slots(), 2);

    let a = action("echo", &["/bin/sh", "-c", "echo hello"], &[], 5000);
    let digest = action_digest_at(&a, &BTreeMap::new(), dir.path()).unwrap();
    let out = client
        .exec(&a, &digest, BTreeMap::new(), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(out.result.exit_code, 0);
    assert_eq!(out.stdout, b"hello\n");
    worker.shutdown().await;
}

#[tokio::test]
async fn nonzero_exit_is_propagated() {
    let dir = tempfile::tempdir().unwrap();
    let worker = boot(1, dir.path()).await;
    let client = WorkerClient::connect(worker.addr()).await.unwrap();

    let a = action("fail", &["/bin/sh", "-c", "exit 42"], &[], 5000);
    let digest = action_digest_at(&a, &BTreeMap::new(), dir.path()).unwrap();
    let out = client
        .exec(&a, &digest, BTreeMap::new(), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(out.result.exit_code, 42);
    worker.shutdown().await;
}

#[tokio::test]
async fn timeout_kills_action_quickly() {
    let dir = tempfile::tempdir().unwrap();
    let worker = boot(2, dir.path()).await;
    let client = WorkerClient::connect(worker.addr()).await.unwrap();

    let a = action("slow", &["sleep", "10"], &[], 500);
    let digest = action_digest_at(&a, &BTreeMap::new(), dir.path()).unwrap();
    let started = Instant::now();
    let err = client
        .exec(&a, &digest, BTreeMap::new(), Duration::from_secs(30))
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    match err {
        forge_core::ClientError::Remote { kind, .. } => assert_eq!(kind, "timeout"),
        other => panic!("expected timeout remote error, got {other:?}"),
    }
    worker.shutdown().await;
}

#[tokio::test]
async fn ping_pong_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let worker = boot(1, dir.path()).await;
    let client = WorkerClient::connect(worker.addr()).await.unwrap();
    client.ping(Duration::from_secs(5)).await.unwrap();
    worker.shutdown().await;
}

/// A counter action: acquire a lock, bump a shared counter recording the
/// maximum observed value, sleep, then decrement. Used to prove that the worker
/// never exceeds its slot limit.
fn write_counter_script(dir: &Path) -> PathBuf {
    let script = dir.join("counter.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
DIR="$1"
lock() { while ! mkdir "$DIR/lock" 2>/dev/null; do sleep 0.005; done; }
unlock() { rmdir "$DIR/lock"; }
lock
cur=$(cat "$DIR/cur" 2>/dev/null || echo 0)
cur=$((cur+1))
echo "$cur" > "$DIR/cur"
max=$(cat "$DIR/max" 2>/dev/null || echo 0)
if [ "$cur" -gt "$max" ]; then echo "$cur" > "$DIR/max"; fi
unlock
sleep 0.2
lock
cur=$(cat "$DIR/cur")
cur=$((cur-1))
echo "$cur" > "$DIR/cur"
unlock
"#,
    )
    .unwrap();
    script
}

#[tokio::test]
async fn slots_are_never_exceeded() {
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    let counter_dir = shared.join("counter");
    std::fs::create_dir_all(&counter_dir).unwrap();
    let script = write_counter_script(dir.path());

    let slots = 2;
    let worker = boot(slots, &dir.path().join("scratch")).await;
    let client = Arc::new(WorkerClient::connect(worker.addr()).await.unwrap());

    let mut set = tokio::task::JoinSet::new();
    for i in 0..8u32 {
        let client = Arc::clone(&client);
        let script = script.clone();
        let counter_dir = counter_dir.clone();
        let base = dir.path().to_path_buf();
        set.spawn(async move {
            let a = Action {
                id: format!("counter-{i}"),
                command: vec![
                    "/bin/sh".to_string(),
                    script.to_string_lossy().to_string(),
                    counter_dir.to_string_lossy().to_string(),
                ],
                env: BTreeMap::new(),
                inputs: vec![],
                outputs: vec![],
                deps: vec![],
                timeout_ms: 30_000,
                tags: vec![],
            };
            let digest = action_digest_at(&a, &BTreeMap::new(), &base).unwrap();
            client
                .exec(&a, &digest, BTreeMap::new(), Duration::from_secs(30))
                .await
        });
    }

    let mut completed = 0;
    while let Some(joined) = set.join_next().await {
        let out = joined.unwrap().unwrap();
        assert_eq!(out.result.exit_code, 0);
        completed += 1;
    }
    assert_eq!(completed, 8);

    let max: u32 = std::fs::read_to_string(counter_dir.join("max"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        max <= slots,
        "observed max concurrency {max} exceeded slots {slots}"
    );
    assert!(max >= 1);
    worker.shutdown().await;
}
