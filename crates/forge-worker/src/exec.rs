//! Action execution: hermetic scratch directories, local subprocesses, and
//! optional Docker containers.
//!
//! Every action runs in a fresh scratch directory under the worker's `workdir`.
//! The declared inputs are materialized there, the command runs with `cwd` set
//! to that directory, the declared outputs are collected and hashed, and the
//! scratch directory is deleted. Nothing is shared between two actions, so a
//! worker can run many concurrently without cross-talk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use forge_core::action_cache::ActionResult;
use forge_core::digest::bytes_digest;
use forge_core::protocol::{Blob, ExecError, ExecSuccess};
use forge_core::Action;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{debug, warn};

/// Default `PATH` used inside a hermetic local execution. A fixed value (rather
/// than the worker's inherited `PATH`) keeps executions reproducible across
/// machines; an action may override it via `env`.
#[cfg(unix)]
const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin";
#[cfg(not(unix))]
const DEFAULT_PATH: &str = "C:\\Windows\\System32";

/// Maximum captured stdout/stderr in bytes before the stream is truncated.
pub const STREAM_CAP: usize = 1 << 20;

/// Which execution backend to use.
#[derive(Debug, Clone)]
pub enum Executor {
    /// Run the program directly on the host with process-group isolation.
    Local,
    /// Run the program inside a Docker container sharing the scratch dir.
    Docker(DockerConfig),
}

/// Settings for the Docker executor.
#[derive(Debug, Clone)]
pub struct DockerConfig {
    /// Container image to run (must contain the action's program).
    pub image: String,
    /// CPU limit passed to `docker run --cpus`.
    pub cpus: f32,
    /// Memory limit in MiB passed to `docker run --memory`.
    pub memory_mb: u64,
}

impl Default for DockerConfig {
    fn default() -> Self {
        DockerConfig {
            image: "gcc:13".to_string(),
            cpus: 1.0,
            memory_mb: 1024,
        }
    }
}

/// Worker-wide configuration shared by every connection.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Stable worker id reported in `Hello`.
    pub worker_id: String,
    /// Maximum concurrent actions.
    pub slots: u32,
    /// Parent directory for scratch and (optionally) the worker CAS mirror.
    pub workdir: PathBuf,
    /// Execution backend.
    pub executor: Executor,
}

impl WorkerConfig {
    /// Creates a local executor configuration with sensible defaults.
    pub fn local(worker_id: impl Into<String>, slots: u32, workdir: impl Into<PathBuf>) -> Self {
        WorkerConfig {
            worker_id: worker_id.into(),
            slots,
            workdir: workdir.into(),
            executor: Executor::Local,
        }
    }
}

/// A scratch directory that is removed on drop (best-effort, async removal is
/// performed explicitly by [`execute`]).
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn create(root: &Path) -> std::io::Result<Scratch> {
        std::fs::create_dir_all(root)?;
        let path = root.join(format!("run-{}", unique_token()));
        std::fs::create_dir_all(&path)?;
        Ok(Scratch { path })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn unique_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", std::process::id(), nanos, n)
}

/// Executes one action, returning captured streams, outputs, and timing.
///
/// Worker-level failures (spawn errors, timeouts, missing declared outputs,
/// digest mismatches) are [`ExecError`]s and are candidate retries. A process
/// that runs and exits non-zero is *not* an error at this layer: it is returned
/// as an [`ExecSuccess`] with a non-zero `exit_code`.
pub async fn execute(
    config: &WorkerConfig,
    action: &Action,
    inputs: &BTreeMap<PathBuf, Blob>,
) -> Result<ExecSuccess, ExecError> {
    let scratch = Scratch::create(&config.workdir)
        .map_err(|e| ExecError::new("io", format!("create scratch: {e}")))?;
    let workdir = scratch.path.clone();

    materialize_inputs(&workdir, inputs)?;
    prepare_output_dirs(&workdir, action)?;

    let started = Instant::now();
    let outcome = match &config.executor {
        Executor::Local => run_local(&workdir, action).await,
        Executor::Docker(docker) => run_docker(&workdir, action, docker).await,
    };
    let wall_ms = started.elapsed().as_millis() as u64;

    let (exit_code, stdout, stderr) = outcome?;

    let mut outputs: BTreeMap<PathBuf, Blob> = BTreeMap::new();
    let mut output_digests: BTreeMap<PathBuf, forge_core::Digest> = BTreeMap::new();
    for rel in &action.outputs {
        let path = workdir.join(rel);
        let data = std::fs::read(&path).map_err(|_| {
            ExecError::new(
                "missing_output",
                format!(
                    "action '{}' did not produce declared output '{}'",
                    action.id,
                    rel.display()
                ),
            )
        })?;
        let blob = Blob::new(data);
        output_digests.insert(rel.clone(), blob.digest.clone());
        outputs.insert(rel.clone(), blob);
    }

    let result = ActionResult::new(exit_code, &stdout, &stderr, output_digests, wall_ms);
    debug!(
        action = %action.id,
        exit_code,
        wall_ms,
        outputs = outputs.len(),
        "action executed"
    );
    Ok(ExecSuccess {
        result,
        stdout,
        stderr,
        outputs,
    })
}

fn materialize_inputs(workdir: &Path, inputs: &BTreeMap<PathBuf, Blob>) -> Result<(), ExecError> {
    for (rel, blob) in inputs {
        let dest = workdir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ExecError::new("io", format!("mkdir {}: {e}", parent.display())))?;
        }
        let actual = bytes_digest(&blob.data);
        if actual != blob.digest {
            return Err(ExecError::new(
                "bad_input",
                format!(
                    "input '{}' digest mismatch: declared {}, actual {}",
                    rel.display(),
                    blob.digest,
                    actual
                ),
            ));
        }
        std::fs::write(&dest, &blob.data)
            .map_err(|e| ExecError::new("io", format!("write {}: {e}", dest.display())))?;
    }
    Ok(())
}

/// Creates the parent directory of every declared output before running, so a
/// command like `gcc -o build/x.o` does not have to create directories itself.
fn prepare_output_dirs(workdir: &Path, action: &Action) -> Result<(), ExecError> {
    for rel in &action.outputs {
        if let Some(parent) = workdir.join(rel).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ExecError::new("io", format!("mkdir {}: {e}", parent.display())))?;
        }
    }
    Ok(())
}

async fn run_local(workdir: &Path, action: &Action) -> Result<(i32, Vec<u8>, Vec<u8>), ExecError> {
    let (program, args) = action
        .command
        .split_first()
        .ok_or_else(|| ExecError::new("invalid_action", "empty command"))?;

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_env(&mut cmd, workdir, action);

    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    spawn_and_wait(cmd, action, workdir, None).await
}

async fn run_docker(
    workdir: &Path,
    action: &Action,
    docker: &DockerConfig,
) -> Result<(i32, Vec<u8>, Vec<u8>), ExecError> {
    let (program, args) = action
        .command
        .split_first()
        .ok_or_else(|| ExecError::new("invalid_action", "empty command"))?;

    let name = format!("forge-{}", unique_token());
    let mut cmd = Command::new("docker");
    cmd.arg("run")
        .arg("--rm")
        .arg("--network=none")
        .arg(format!("--cpus={}", docker.cpus))
        .arg(format!("--memory={}m", docker.memory_mb))
        .arg("--name")
        .arg(&name)
        .arg("-v")
        .arg(format!("{}:/work", workdir.display()))
        .arg("-w")
        .arg("/work");
    for (key, value) in &action.env {
        cmd.arg("-e").arg(format!("{key}={value}"));
    }
    cmd.arg(&docker.image).arg(program).args(args);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    spawn_and_wait(cmd, action, workdir, Some(name)).await
}

fn configure_env(cmd: &mut Command, workdir: &Path, action: &Action) {
    cmd.env_clear();
    let path = action
        .env
        .get("PATH")
        .cloned()
        .unwrap_or_else(|| DEFAULT_PATH.to_string());
    cmd.env("PATH", path);
    cmd.env("HOME", workdir);
    cmd.env("TMPDIR", workdir);
    for (key, value) in &action.env {
        cmd.env(key, value);
    }
}

async fn spawn_and_wait(
    mut cmd: Command,
    action: &Action,
    workdir: &Path,
    container: Option<String>,
) -> Result<(i32, Vec<u8>, Vec<u8>), ExecError> {
    let mut child = cmd
        .spawn()
        .map_err(|e| ExecError::new("spawn", format!("spawn failed: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExecError::new("internal", "no stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ExecError::new("internal", "no stderr pipe"))?;
    let out_task = tokio::spawn(read_capped(stdout, STREAM_CAP));
    let err_task = tokio::spawn(read_capped(stderr, STREAM_CAP));

    let timeout = Duration::from_millis(action.timeout_ms);
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            return Err(ExecError::new("io", format!("wait failed: {e}")));
        }
        Err(_) => {
            kill_process_group(&child);
            if let Some(name) = &container {
                // Best-effort: the docker CLI's process group is not the container.
                let _ = tokio::process::Command::new("docker")
                    .arg("kill")
                    .arg(name)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
            }
            let _ = child.wait().await;
            let _ = out_task.await;
            let _ = err_task.await;
            warn!(action = %action.id, timeout_ms = action.timeout_ms, "action timed out; killed process group");
            return Err(ExecError::new(
                "timeout",
                format!("action '{}' exceeded {} ms", action.id, action.timeout_ms),
            ));
        }
    };

    let stdout = out_task.await.unwrap_or_default();
    let stderr = err_task.await.unwrap_or_default();

    // If no outputs are declared, the action's success is its exit status.
    let _ = workdir;
    Ok((status.code().unwrap_or(-1), stdout, stderr))
}

/// Sends `SIGKILL` to the child's whole process group so grandchildren die too.
#[cfg(unix)]
fn kill_process_group(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        // Safe: `kill` on a pid we own; negative pid targets the group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(child: &tokio::process::Child) {
    if let Some(_pid) = child.id() {
        // On Windows, process-group killing is not wired up in this project.
    }
}

async fn read_capped<R>(mut reader: R, cap: usize) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = (cap - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
            Err(_) => break,
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn action(id: &str, argv: &[&str], outputs: &[&str], timeout_ms: u64) -> Action {
        Action {
            id: id.into(),
            command: argv.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
            inputs: vec![],
            outputs: outputs.iter().map(PathBuf::from).collect(),
            deps: vec![],
            timeout_ms,
            tags: vec![],
        }
    }

    fn config(dir: &Path) -> WorkerConfig {
        WorkerConfig::local("test", 2, dir)
    }

    #[tokio::test]
    async fn echo_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let a = action("echo", &["/bin/sh", "-c", "echo hello"], &[], 5000);
        let out = execute(&cfg, &a, &BTreeMap::new()).await.unwrap();
        assert_eq!(out.result.exit_code, 0);
        assert_eq!(out.stdout, b"hello\n");
    }

    #[tokio::test]
    async fn nonzero_exit_is_success_with_code() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let a = action("fail", &["/bin/sh", "-c", "exit 7"], &[], 5000);
        let out = execute(&cfg, &a, &BTreeMap::new()).await.unwrap();
        assert_eq!(out.result.exit_code, 7);
    }

    #[tokio::test]
    async fn timeout_kills_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let a = action("sleep", &["sleep", "10"], &[], 400);
        let started = Instant::now();
        let err = execute(&cfg, &a, &BTreeMap::new()).await.unwrap_err();
        assert_eq!(err.kind, "timeout");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn declared_output_is_collected() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let a = action(
            "write",
            &["/bin/sh", "-c", "printf abc > out.txt"],
            &["out.txt"],
            5000,
        );
        let out = execute(&cfg, &a, &BTreeMap::new()).await.unwrap();
        assert_eq!(out.outputs[Path::new("out.txt")].data, b"abc");
    }

    #[tokio::test]
    async fn missing_output_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let a = action("nope", &["/bin/sh", "-c", "true"], &["missing.txt"], 5000);
        let err = execute(&cfg, &a, &BTreeMap::new()).await.unwrap_err();
        assert_eq!(err.kind, "missing_output");
    }
}
