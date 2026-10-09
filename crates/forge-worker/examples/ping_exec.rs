//! Minimal end-to-end example: start a local worker, send one echo action, and
//! print the result. Run with:
//!
//! ```sh
//! cargo run -p forge-worker --example ping_exec
//! ```

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use forge_core::{action_digest_at, Action, WorkerClient};
use forge_worker::{start, WorkerConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workdir = std::env::temp_dir().join("forge-worker-example");
    let worker = start(
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        WorkerConfig::local("example", 1, &workdir),
    )
    .await?;
    println!(
        "worker {} listening on {} (slots={})",
        worker.worker_id(),
        worker.addr(),
        worker.slots()
    );

    let client = WorkerClient::connect(worker.addr()).await?;

    let action = Action {
        id: "echo".to_string(),
        command: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo forge > msg.txt".to_string(),
        ],
        env: BTreeMap::new(),
        inputs: vec![],
        outputs: vec![PathBuf::from("msg.txt")],
        deps: vec![],
        timeout_ms: 5000,
        tags: vec!["example".to_string()],
    };
    let digest = action_digest_at(&action, &BTreeMap::new(), &workdir)?;

    let out = client
        .exec(&action, &digest, BTreeMap::new(), Duration::from_secs(10))
        .await?;

    println!("action digest : {digest}");
    println!("exit code     : {}", out.result.exit_code);
    println!(
        "stdout        : {}",
        String::from_utf8_lossy(&out.stdout).trim_end()
    );
    for (path, blob) in &out.outputs {
        println!(
            "output {} -> {} ({} bytes)",
            path.display(),
            blob.digest,
            blob.data.len()
        );
    }

    worker.shutdown().await;
    Ok(())
}
