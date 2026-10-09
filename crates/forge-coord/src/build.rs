//! The cache-aware, bounded-concurrency build scheduler.
//!
//! # Scheduling model
//!
//! * The planner walks the dependency graph in topological order. An action
//!   becomes *ready* once every dependency has completed; only then is its
//!   digest computed (a parent's inputs may be a dependency's outputs, so those
//!   bytes must exist first).
//! * Before dispatching, the action cache is probed. A hit restores the declared
//!   outputs from the CAS and records `hit` with no worker round trip.
//! * A miss is dispatched to a worker. Global in-flight is capped by `--jobs`;
//!   per-worker in-flight is capped by the worker's advertised slots. Both are
//!   enforced by [`WorkerPool`], so the ready queue never grows unbounded.
//! * Retryable failures (transport errors, timeouts, worker faults) are retried
//!   up to two times with exponential backoff plus jitter, possibly on a
//!   different worker. A genuine non-zero exit is a build failure and is not
//!   retried.
//! * After `eject_after` consecutive faults a worker is ejected; if every worker
//!   is gone and `--local-fallback` is set, actions run in-process.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::action_cache::{ActionCache, ActionResult};
use forge_core::protocol::{Blob, ExecSuccess};
use forge_core::{
    ac_dir, action_digest_at, cas_dir, Action, BuildGraph, Cas, Digest, WorkerClient,
};
use forge_worker::{execute as execute_local, WorkerConfig};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tracing::{info, instrument, warn};

use crate::planner;
use crate::report::{ActionReport, ActionStatus, Report};
use crate::state::{state_path, BuildState, TargetState};
use crate::Result;

/// Maximum number of retries after the first attempt.
pub const MAX_RETRIES: u32 = 2;

/// Options controlling a build.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Path to the JSON manifest.
    pub manifest: PathBuf,
    /// Worker addresses.
    pub workers: Vec<SocketAddr>,
    /// Global in-flight cap.
    pub jobs: usize,
    /// Do not read or write the action cache.
    pub no_cache: bool,
    /// Remove `.forge/` before building.
    pub clean_cache: bool,
    /// Execute in-process if every worker is unavailable.
    pub local_fallback: bool,
    /// Consecutive worker faults before ejecting it.
    pub eject_after: u32,
    /// Optional path to write the JSON report to.
    pub report: Option<PathBuf>,
}

impl BuildOptions {
    /// Reasonable defaults for a manifest with the given workers.
    pub fn new(manifest: impl Into<PathBuf>, workers: Vec<SocketAddr>) -> Self {
        BuildOptions {
            manifest: manifest.into(),
            workers,
            jobs: 4,
            no_cache: false,
            clean_cache: false,
            local_fallback: false,
            eject_after: 3,
            report: None,
        }
    }
}

struct WorkerEntry {
    client: Arc<WorkerClient>,
    slots: usize,
    in_flight: usize,
    failures: u32,
    ejected: bool,
}

/// A pool of connected workers with per-worker slot accounting.
pub struct WorkerPool {
    entries: Vec<WorkerEntry>,
    eject_after: u32,
}

impl WorkerPool {
    /// Builds a pool from already-connected clients.
    pub fn new(clients: Vec<Arc<WorkerClient>>, eject_after: u32) -> Self {
        let entries = clients
            .into_iter()
            .map(|client| WorkerEntry {
                slots: client.slots() as usize,
                client,
                in_flight: 0,
                failures: 0,
                ejected: false,
            })
            .collect();
        WorkerPool {
            entries,
            eject_after,
        }
    }

    fn try_acquire(&mut self) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.ejected || !entry.client.is_alive() {
                continue;
            }
            if entry.in_flight >= entry.slots {
                continue;
            }
            match best {
                Some(b) if self.entries[b].in_flight <= entry.in_flight => {}
                _ => best = Some(i),
            }
        }
        if let Some(i) = best {
            self.entries[i].in_flight += 1;
        }
        best
    }

    fn release(&mut self, idx: usize) {
        if self.entries[idx].in_flight > 0 {
            self.entries[idx].in_flight -= 1;
        }
    }

    fn record(&mut self, idx: usize, healthy: bool) {
        if healthy {
            self.entries[idx].failures = 0;
        } else {
            self.entries[idx].failures += 1;
            if self.entries[idx].failures >= self.eject_after {
                self.entries[idx].ejected = true;
                warn!(
                    worker = %self.entries[idx].client.worker_id(),
                    "ejecting worker after consecutive faults"
                );
            }
        }
    }

    fn live_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| !e.ejected && e.client.is_alive())
            .count()
    }

    fn client(&self, idx: usize) -> Arc<WorkerClient> {
        Arc::clone(&self.entries[idx].client)
    }

    fn worker_ids(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|e| e.client.worker_id().to_string())
            .collect()
    }
}

enum Acquire {
    Worker(usize),
    Local,
    None,
}

struct DispatchResult {
    id: String,
    digest: Digest,
    worker: String,
    retries: u32,
    outcome: std::result::Result<ExecSuccess, String>,
}

async fn acquire_slot(pool: &Mutex<WorkerPool>, local_fallback: bool) -> Acquire {
    loop {
        let live = {
            let mut guard = pool.lock().await;
            if let Some(idx) = guard.try_acquire() {
                return Acquire::Worker(idx);
            }
            guard.live_count()
        };
        if live == 0 {
            return if local_fallback {
                Acquire::Local
            } else {
                Acquire::None
            };
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn dispatch_action(
    id: String,
    action: Action,
    digest: Digest,
    inputs: BTreeMap<PathBuf, Blob>,
    pool: Arc<Mutex<WorkerPool>>,
    local_fallback: bool,
) -> DispatchResult {
    let client_timeout = Duration::from_millis(action.timeout_ms) + Duration::from_secs(10);
    let mut retries = 0u32;

    loop {
        match acquire_slot(&pool, local_fallback).await {
            Acquire::None => {
                return DispatchResult {
                    id,
                    digest,
                    worker: "none".to_string(),
                    retries,
                    outcome: Err("no workers available (and --local-fallback not set)".to_string()),
                };
            }
            Acquire::Local => {
                let config = WorkerConfig::local(
                    "coord-local",
                    1,
                    std::env::temp_dir().join("forge-coord-local"),
                );
                match execute_local(&config, &action, &inputs).await {
                    Ok(success) => {
                        return DispatchResult {
                            id,
                            digest,
                            worker: "local".to_string(),
                            retries,
                            outcome: Ok(success),
                        };
                    }
                    Err(e) if e.is_retryable() && retries < MAX_RETRIES => {
                        retries += 1;
                        backoff(retries).await;
                    }
                    Err(e) => {
                        return DispatchResult {
                            id,
                            digest,
                            worker: "local".to_string(),
                            retries,
                            outcome: Err(format!("{e:?}")),
                        };
                    }
                }
            }
            Acquire::Worker(idx) => {
                let client = {
                    let guard = pool.lock().await;
                    guard.client(idx)
                };
                let worker = client.worker_id().to_string();
                let result = client
                    .exec(&action, &digest, inputs.clone(), client_timeout)
                    .await;
                let fault = result
                    .as_ref()
                    .err()
                    .map(|e| e.is_retryable())
                    .unwrap_or(false);
                {
                    let mut guard = pool.lock().await;
                    guard.release(idx);
                    guard.record(idx, !fault);
                }
                match result {
                    Ok(success) => {
                        return DispatchResult {
                            id,
                            digest,
                            worker,
                            retries,
                            outcome: Ok(success),
                        };
                    }
                    Err(e) if e.is_retryable() && retries < MAX_RETRIES => {
                        retries += 1;
                        backoff(retries).await;
                    }
                    Err(e) => {
                        return DispatchResult {
                            id,
                            digest,
                            worker,
                            retries,
                            outcome: Err(format!("{e}")),
                        };
                    }
                }
            }
        }
    }
}

async fn backoff(attempt: u32) {
    let base = (50u64 << attempt.min(5)).min(1000);
    let jitter = rand::random::<u64>() % base.max(1);
    tokio::time::sleep(Duration::from_millis(base + jitter.min(200))).await;
}

fn read_inputs(action: &Action, base_dir: &Path) -> Result<BTreeMap<PathBuf, Blob>> {
    let mut inputs = BTreeMap::new();
    for input in &action.inputs {
        let path = forge_core::resolve(base_dir, input);
        let data = std::fs::read(&path)
            .map_err(|e| anyhow::anyhow!("read input {}: {e}", path.display()))?;
        inputs.insert(input.clone(), Blob::new(data));
    }
    Ok(inputs)
}

fn restore_outputs(
    cas: &Cas,
    base_dir: &Path,
    result: &ActionResult,
    bytes_from_cas: &mut u64,
) -> Result<bool> {
    for digest in result.output_digests.values() {
        if !cas.has(digest) {
            return Ok(false);
        }
    }
    for (rel, digest) in &result.output_digests {
        let dest = base_dir.join(rel);
        cas.get_file(digest, &dest)?;
        *bytes_from_cas += std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    }
    Ok(true)
}

fn store_outputs(
    cas: &Cas,
    base_dir: &Path,
    success: &ExecSuccess,
    bytes_to_cas: &mut u64,
) -> Result<()> {
    for (rel, blob) in &success.outputs {
        let outcome = cas.put_bytes(&blob.data)?;
        if !outcome.deduplicated {
            *bytes_to_cas += blob.data.len() as u64;
        }
        let dest = base_dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &blob.data)?;
    }
    Ok(())
}

/// Runs a full build and returns its [`Report`].
#[instrument(skip_all, fields(manifest = %options.manifest.display(), jobs = options.jobs))]
pub async fn run_build(options: &BuildOptions) -> Result<Report> {
    let started = Instant::now();
    let graph = BuildGraph::from_manifest(&options.manifest)?;
    let base_dir = graph.base_dir().to_path_buf();

    if options.clean_cache {
        let forge = base_dir.join(forge_core::FORGE_DIR);
        if forge.exists() {
            std::fs::remove_dir_all(&forge)?;
        }
    }
    let cas = Cas::new(cas_dir(&base_dir))?;
    let action_cache = ActionCache::new(ac_dir(&base_dir))?;

    let mut clients = Vec::new();
    for addr in &options.workers {
        match WorkerClient::connect(*addr).await {
            Ok(client) => {
                info!(%addr, worker = client.worker_id(), slots = client.slots(), "connected");
                clients.push(Arc::new(client));
            }
            Err(e) => warn!(%addr, error = %e, "failed to connect to worker"),
        }
    }
    if clients.is_empty() && !options.local_fallback {
        warn!("no workers connected; build will fail unless --local-fallback is set");
    }
    let pool = Arc::new(Mutex::new(WorkerPool::new(
        clients,
        options.eject_after.max(1),
    )));
    let worker_ids = pool.lock().await.worker_ids();

    let total = graph.len();
    let mut completed: HashSet<String> = HashSet::new();
    let mut digests: HashMap<String, Digest> = HashMap::new();
    let mut started_ids: HashSet<String> = HashSet::new();
    let mut ready_queue: VecDeque<String> = VecDeque::new();
    let mut reports: HashMap<String, ActionReport> = HashMap::new();
    let mut inflight = 0usize;
    let mut bytes_from_cas = 0u64;
    let mut bytes_to_cas = 0u64;
    let mut set: JoinSet<DispatchResult> = JoinSet::new();

    loop {
        // Discover ready actions, resolving digests and cache hits. A cache hit
        // completes synchronously, which can make further actions ready in the
        // same pass, so we repeat until no new work is discovered.
        loop {
            let newly_ready: Vec<String> = graph
                .ready(&completed)
                .filter(|action| !started_ids.contains(&action.id))
                .map(|action| action.id.clone())
                .collect();
            if newly_ready.is_empty() {
                break;
            }
            for id in newly_ready {
                let action = graph
                    .get(&id)
                    .ok_or_else(|| anyhow::anyhow!("action '{id}' missing"))?
                    .clone();
                let dep_digests: BTreeMap<String, Digest> = action
                    .deps
                    .iter()
                    .map(|dep| {
                        digests
                            .get(dep)
                            .cloned()
                            .map(|d| (dep.clone(), d))
                            .ok_or_else(|| anyhow::anyhow!("missing digest for dep '{dep}'"))
                    })
                    .collect::<Result<_>>()?;
                let digest = action_digest_at(&action, &dep_digests, &base_dir)?;
                digests.insert(id.clone(), digest.clone());

                if !options.no_cache {
                    if let Some(result) = action_cache.get(&digest) {
                        if restore_outputs(&cas, &base_dir, &result, &mut bytes_from_cas)? {
                            completed.insert(id.clone());
                            started_ids.insert(id.clone());
                            reports.insert(
                                id.clone(),
                                ActionReport {
                                    id: id.clone(),
                                    digest: digest.as_hex().to_string(),
                                    status: ActionStatus::Hit,
                                    worker: "cache".to_string(),
                                    wall_ms: 0,
                                    retries: 0,
                                },
                            );
                            continue;
                        }
                    }
                }
                started_ids.insert(id.clone());
                ready_queue.push_back(id);
            }
        }

        // Dispatch ready actions within the global in-flight cap.
        while inflight < options.jobs.max(1) && !ready_queue.is_empty() {
            let id = ready_queue.pop_front().expect("checked non-empty");
            let action = graph
                .get(&id)
                .ok_or_else(|| anyhow::anyhow!("action '{id}' missing"))?
                .clone();
            let digest = digests[&id].clone();
            let inputs = read_inputs(&action, &base_dir)?;
            let pool = Arc::clone(&pool);
            let local_fallback = options.local_fallback;
            inflight += 1;
            set.spawn(dispatch_action(
                id,
                action,
                digest,
                inputs,
                pool,
                local_fallback,
            ));
        }

        if inflight == 0 {
            if completed.len() == total {
                break;
            }
            if ready_queue.is_empty() {
                return Err(anyhow::anyhow!(
                    "build stalled with {} of {total} actions complete (no runnable actions)",
                    completed.len()
                ));
            }
            continue;
        }

        let joined = set
            .join_next()
            .await
            .ok_or_else(|| anyhow::anyhow!("scheduler task set ended unexpectedly"))?;
        let result = joined.map_err(|e| anyhow::anyhow!("scheduler task panicked: {e}"))?;
        inflight -= 1;

        match result.outcome {
            Ok(success) => {
                store_outputs(&cas, &base_dir, &success, &mut bytes_to_cas)?;
                if !options.no_cache {
                    action_cache.put(&result.digest, &success.result)?;
                }
                completed.insert(result.id.clone());
                reports.insert(
                    result.id.clone(),
                    ActionReport {
                        id: result.id.clone(),
                        digest: result.digest.as_hex().to_string(),
                        status: ActionStatus::Miss,
                        worker: result.worker.clone(),
                        wall_ms: success.result.wall_ms,
                        retries: result.retries,
                    },
                );
            }
            Err(msg) => {
                reports.insert(
                    result.id.clone(),
                    ActionReport {
                        id: result.id.clone(),
                        digest: result.digest.as_hex().to_string(),
                        status: ActionStatus::Failed,
                        worker: result.worker.clone(),
                        wall_ms: 0,
                        retries: result.retries,
                    },
                );
                let _ = finalize_report(
                    &graph,
                    &digests,
                    &reports,
                    &worker_ids,
                    started.elapsed().as_millis() as u64,
                    bytes_from_cas,
                    bytes_to_cas,
                    &options.report,
                )?;
                return Err(anyhow::anyhow!("action '{}' failed: {msg}", result.id));
            }
        }
    }

    let state = build_state(&graph, &digests)?;
    state.save(&state_path(&base_dir))?;

    let total_wall_ms = started.elapsed().as_millis() as u64;
    let report = finalize_report(
        &graph,
        &digests,
        &reports,
        &worker_ids,
        total_wall_ms,
        bytes_from_cas,
        bytes_to_cas,
        &options.report,
    )?;
    info!(summary = %report.summary(), "build complete");
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn finalize_report(
    graph: &BuildGraph,
    digests: &HashMap<String, Digest>,
    reports: &HashMap<String, ActionReport>,
    worker_ids: &[String],
    total_wall_ms: u64,
    bytes_from_cas: u64,
    bytes_to_cas: u64,
    report_path: &Option<PathBuf>,
) -> Result<Report> {
    let mut actions: Vec<ActionReport> = graph
        .actions()
        .map(|action| {
            reports
                .get(&action.id)
                .cloned()
                .unwrap_or_else(|| ActionReport {
                    id: action.id.clone(),
                    digest: digests
                        .get(&action.id)
                        .map(|d| d.as_hex().to_string())
                        .unwrap_or_default(),
                    status: ActionStatus::Failed,
                    worker: "none".to_string(),
                    wall_ms: 0,
                    retries: 0,
                })
        })
        .collect();
    actions.sort_by(|a, b| a.id.cmp(&b.id));

    let actions_hit = actions
        .iter()
        .filter(|a| a.status == ActionStatus::Hit)
        .count();
    let actions_miss = actions
        .iter()
        .filter(|a| a.status == ActionStatus::Miss)
        .count();
    let actions_failed = actions
        .iter()
        .filter(|a| a.status == ActionStatus::Failed)
        .count();
    let denom = actions_hit + actions_miss;
    let cache_hit_rate = if denom == 0 {
        0.0
    } else {
        actions_hit as f64 / denom as f64
    };

    // Total CPU and critical path use executed (miss) wall times; hits are free.
    let total_cpu_ms: u64 = actions
        .iter()
        .filter(|a| a.status == ActionStatus::Miss)
        .map(|a| a.wall_ms)
        .sum();

    let mut critical: HashMap<String, u64> = HashMap::new();
    let mut critical_path_ms = 0u64;
    if let Ok(layers) = graph.topo_layers() {
        for layer in &layers {
            for id in layer {
                let action = graph.get(id).expect("layer id present");
                let dep_max = action
                    .deps
                    .iter()
                    .map(|d| critical.get(d).copied().unwrap_or(0))
                    .max()
                    .unwrap_or(0);
                let eff = reports
                    .get(id)
                    .filter(|r| r.status == ActionStatus::Miss)
                    .map(|r| r.wall_ms)
                    .unwrap_or(0);
                let value = dep_max + eff;
                critical.insert(id.clone(), value);
                critical_path_ms = critical_path_ms.max(value);
            }
        }
    }

    let achieved_parallelism = if total_wall_ms == 0 {
        0.0
    } else {
        total_cpu_ms as f64 / total_wall_ms as f64
    };

    let report = Report {
        workers: worker_ids.to_vec(),
        total_wall_ms,
        actions_total: graph.len(),
        actions_hit,
        actions_miss,
        actions_failed,
        cache_hit_rate,
        bytes_from_cas,
        bytes_to_cas,
        total_cpu_ms,
        critical_path_ms,
        achieved_parallelism,
        actions,
    };

    if let Some(path) = report_path {
        report.write_json(path)?;
    }
    Ok(report)
}

fn build_state(graph: &BuildGraph, digests: &HashMap<String, Digest>) -> Result<BuildState> {
    let mut state = BuildState::default();
    for action in graph.actions() {
        let inputs = planner::input_digests(action, graph.base_dir())?;
        let inputs = inputs
            .into_iter()
            .map(|(k, v)| (k.to_string_lossy().to_string(), v.as_hex().to_string()))
            .collect();
        let deps = action
            .deps
            .iter()
            .filter_map(|dep| {
                digests
                    .get(dep)
                    .map(|d| (dep.clone(), d.as_hex().to_string()))
            })
            .collect();
        state.targets.insert(
            action.id.clone(),
            TargetState {
                digest: digests
                    .get(&action.id)
                    .map(|d| d.as_hex().to_string())
                    .unwrap_or_default(),
                inputs,
                deps,
            },
        );
    }
    Ok(state)
}
