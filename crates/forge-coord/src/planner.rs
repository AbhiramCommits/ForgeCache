//! Planning helpers: bottom-up digest computation and input fingerprinting.
//!
//! The scheduler computes a parent's digest only once all of its dependencies
//! have finished, because a parent's declared inputs may be the *outputs* of its
//! dependencies, and those bytes must exist before they can be hashed. This
//! module exposes that same computation as a batch operation for `explain` and
//! tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use forge_core::{action_digest_at, file_digest, Action, BuildGraph, Digest};

use crate::Result;

/// Computes the digest of every action in topological order.
///
/// All declared inputs must exist under `graph.base_dir()`.
pub fn compute_all_digests(graph: &BuildGraph) -> Result<BTreeMap<String, Digest>> {
    let layers = graph.topo_layers()?;
    let mut digests: BTreeMap<String, Digest> = BTreeMap::new();
    for layer in &layers {
        for id in layer {
            let action = graph
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("action '{id}' vanished"))?;
            let dep_digests = dep_digest_map(action, &digests)?;
            let digest = action_digest_at(action, &dep_digests, graph.base_dir())?;
            digests.insert(id.clone(), digest);
        }
    }
    Ok(digests)
}

/// Digests of an action's inputs, keyed by the declared (relative) path.
pub fn input_digests(action: &Action, base_dir: &Path) -> Result<BTreeMap<PathBuf, Digest>> {
    let mut out = BTreeMap::new();
    for input in &action.inputs {
        let path = forge_core::resolve(base_dir, input);
        let digest = file_digest(&path)?;
        out.insert(input.clone(), digest);
    }
    Ok(out)
}

fn dep_digest_map(
    action: &Action,
    digests: &BTreeMap<String, Digest>,
) -> Result<BTreeMap<String, Digest>> {
    let mut map = BTreeMap::new();
    for dep in &action.deps {
        let digest = digests
            .get(dep)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("dependency '{dep}' has no digest yet"))?;
        map.insert(dep.clone(), digest);
    }
    Ok(map)
}
