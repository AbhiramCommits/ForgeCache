//! Build report types.
//!
//! A [`Report`] captures everything the benchmark harness and the `--report`
//! flag need: wall time, per-action outcomes, cache hit rate, CAS byte traffic,
//! critical-path vs. total CPU time, and achieved parallelism.

use serde::{Deserialize, Serialize};

/// The status of a single action in a build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    /// Outputs restored from the action cache; no worker round trip.
    Hit,
    /// Executed on a worker (or locally) and written to the cache.
    Miss,
    /// The action failed; the build aborted.
    Failed,
}

impl ActionStatus {
    /// Lowercase string form used in reports.
    pub fn as_str(&self) -> &'static str {
        match self {
            ActionStatus::Hit => "hit",
            ActionStatus::Miss => "miss",
            ActionStatus::Failed => "failed",
        }
    }
}

/// Per-action result recorded during a build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionReport {
    /// Action id from the manifest.
    pub id: String,
    /// Content-addressed digest of the action.
    pub digest: String,
    /// Hit, miss, or failed.
    pub status: ActionStatus,
    /// Worker id (or `"cache"` / `"local"`).
    pub worker: String,
    /// Wall time of the original execution, in milliseconds.
    pub wall_ms: u64,
    /// Number of retries consumed before success/failure.
    pub retries: u32,
}

/// The aggregate result of one build invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    /// Ids of the workers used (in connection order).
    pub workers: Vec<String>,
    /// Total wall time of the build, in milliseconds.
    pub total_wall_ms: u64,
    /// Total number of actions in the graph.
    pub actions_total: usize,
    /// Number of cache hits.
    pub actions_hit: usize,
    /// Number of actions actually executed.
    pub actions_miss: usize,
    /// Number of failed actions (normally 0 for a successful build).
    pub actions_failed: usize,
    /// `actions_hit / (actions_hit + actions_miss)`.
    pub cache_hit_rate: f64,
    /// Bytes of output restored from the CAS on cache hits.
    pub bytes_from_cas: u64,
    /// Bytes of new output written to the CAS.
    pub bytes_to_cas: u64,
    /// Sum of wall time over *executed* actions, in milliseconds.
    pub total_cpu_ms: u64,
    /// Longest dependency path by sum of executed action wall times.
    pub critical_path_ms: u64,
    /// `total_cpu_ms / total_wall_ms`.
    pub achieved_parallelism: f64,
    /// Per-action detail, in deterministic id order.
    pub actions: Vec<ActionReport>,
}

impl Report {
    /// Number of actions that were not served from cache.
    pub fn actions_executed(&self) -> usize {
        self.actions_miss
    }

    /// Renders a compact, human-readable summary.
    pub fn summary(&self) -> String {
        format!(
            "{} actions | {} hit / {} miss | hit rate {:.1}% | wall {} ms | parallelism {:.2}x",
            self.actions_total,
            self.actions_hit,
            self.actions_miss,
            self.cache_hit_rate * 100.0,
            self.total_wall_ms,
            self.achieved_parallelism
        )
    }

    /// Writes the report as pretty JSON to `path`.
    pub fn write_json(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(path, bytes)
    }
}
