//! Persistent per-target state used by `forge-coord explain` to describe *why*
//! an action would rebuild.
//!
//! At the end of every build we record, per action id, its digest plus the
//! digests of each of its inputs and each of its dependencies. The `explain`
//! command recomputes the current values and diffs them against this snapshot,
//! naming the file or dependency that changed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

/// The recorded digest fingerprint of one target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetState {
    /// The action's digest at the last build.
    pub digest: String,
    /// Input path -> content digest at the last build.
    pub inputs: BTreeMap<String, String>,
    /// Dependency id -> digest at the last build.
    pub deps: BTreeMap<String, String>,
}

/// The full state file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildState {
    /// Map of action id -> fingerprint.
    pub targets: BTreeMap<String, TargetState>,
}

impl BuildState {
    /// Loads state from `path`, returning an empty state if it does not exist.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(BuildState::default());
        }
        let bytes = std::fs::read(path)?;
        let state = serde_json::from_slice(&bytes)?;
        Ok(state)
    }

    /// Writes state to `path` atomically enough for our purposes.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

/// Path of the state file for a working directory.
pub fn state_path(workdir: &Path) -> PathBuf {
    workdir.join(forge_core::FORGE_DIR).join("state.json")
}
