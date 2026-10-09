//! `forge-coord explain` — describes why a target would rebuild.

use std::path::Path;

use forge_core::{ActionCache, BuildGraph};

use crate::planner;
use crate::state::BuildState;
use crate::Result;

/// The result of explaining one target.
#[derive(Debug, Clone)]
pub struct Explanation {
    /// Target id.
    pub id: String,
    /// The target's current content digest.
    pub digest: String,
    /// The digest recorded at the previous build, if any.
    pub previous_digest: Option<String>,
    /// Whether the action cache currently holds this digest.
    pub cached: bool,
    /// Whether the action would be served from cache without rebuilding.
    pub up_to_date: bool,
    /// Human-readable reasons the action would rebuild.
    pub changes: Vec<String>,
}

/// Builds an [`Explanation`] for `target`.
pub fn explain(
    graph: &BuildGraph,
    action_cache: &ActionCache,
    state: &BuildState,
    target: &str,
) -> Result<Explanation> {
    let action = graph
        .get(target)
        .ok_or_else(|| anyhow::anyhow!("unknown target '{target}'"))?;

    let digests = planner::compute_all_digests(graph)?;
    let digest = digests
        .get(target)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no digest computed for '{target}'"))?;
    let inputs = planner::input_digests(action, graph.base_dir())?;

    let cached = action_cache.has(&digest);
    let previous = state.targets.get(target);
    let mut changes = Vec::new();

    if let Some(previous) = previous {
        for (path, digest) in &inputs {
            let key = path.to_string_lossy().to_string();
            match previous.inputs.get(&key) {
                None => changes.push(format!("input added: {key} ({})", short(digest.as_hex()))),
                Some(old) if old != digest.as_hex() => changes.push(format!(
                    "input changed: {key} ({} -> {})",
                    short(old),
                    short(digest.as_hex())
                )),
                Some(_) => {}
            }
        }
        for key in previous.inputs.keys() {
            if !inputs.contains_key(Path::new(key)) {
                changes.push(format!("input removed: {key}"));
            }
        }
        for dep in &action.deps {
            let dep_digest = digests
                .get(dep)
                .ok_or_else(|| anyhow::anyhow!("missing digest for dep '{dep}'"))?;
            match previous.deps.get(dep) {
                None => changes.push(format!("dependency added: {dep}")),
                Some(old) if old != dep_digest.as_hex() => changes.push(format!(
                    "dependency changed: {dep} ({} -> {})",
                    short(old),
                    short(dep_digest.as_hex())
                )),
                Some(_) => {}
            }
        }
        if previous.digest != digest.as_hex() && changes.is_empty() {
            changes.push("command, env, or graph shape changed".to_string());
        }
        if previous.digest == digest.as_hex() && !cached {
            changes.push("inputs unchanged but the cache entry is missing".to_string());
        }
    } else {
        changes.push("no previous build recorded for this target".to_string());
    }

    let up_to_date = cached
        && previous
            .map(|p| p.digest == digest.as_hex())
            .unwrap_or(true);

    Ok(Explanation {
        id: target.to_string(),
        digest: digest.as_hex().to_string(),
        previous_digest: previous.map(|p| p.digest.clone()),
        cached,
        up_to_date,
        changes,
    })
}

fn short(hex: &str) -> String {
    hex.chars().take(12).collect()
}
