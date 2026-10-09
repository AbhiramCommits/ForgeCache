//! The hermetic action model and its canonical, content-addressed digest.
//!
//! An [`Action`] is a description of one hermetic step: the command to run, the
//! environment it sees, the input files it consumes, the outputs it promises,
//! and the ids of the actions it depends on. Two actions that agree on all of
//! the fields that affect their output bytes are guaranteed to produce the same
//! digest regardless of the machine, time, or the order the manifest was written
//! in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::digest::{file_digest, update_len_prefixed, Digest};
use crate::error::{ForgeError, Result};

/// A single hermetic build/simulation step.
///
/// The [`id`](Action::id) is a human-readable label used to wire up
/// [`deps`](Action::deps); it is deliberately *not* part of the digest, so two
/// identically-described actions under different labels share a cache entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    /// Human-readable, unique-within-a-manifest identifier (e.g. `"compile:mod3"`).
    pub id: String,
    /// The argv of the command to execute. `command[0]` is the program.
    pub command: Vec<String>,
    /// Extra environment variables. Serialized sorted because `BTreeMap` is ordered.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Input files, relative to the manifest's base directory (absolute allowed).
    #[serde(default)]
    pub inputs: Vec<PathBuf>,
    /// Output files the action promises to create, relative to the sandbox.
    #[serde(default)]
    pub outputs: Vec<PathBuf>,
    /// Ids of actions that must complete before this one.
    #[serde(default)]
    pub deps: Vec<String>,
    /// Wall-clock timeout in milliseconds.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Free-form tags used for reporting and scheduling hints.
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_timeout_ms() -> u64 {
    60_000
}

impl Action {
    /// Returns `true` when the action declares at least one output.
    pub fn has_outputs(&self) -> bool {
        !self.outputs.is_empty()
    }
}

/// Computes the content-addressed digest of an action, resolving input paths
/// relative to the current working directory.
///
/// Prefer [`action_digest_at`] when the manifest has an explicit base directory.
pub fn action_digest(action: &Action, dep_digests: &BTreeMap<String, Digest>) -> Result<Digest> {
    action_digest_at(action, dep_digests, Path::new("."))
}

/// Computes the content-addressed digest of an action with input paths resolved
/// relative to `base_dir`.
///
/// The digest is BLAKE3 over a canonical serialization with these fields, in
/// order, each length-prefixed to avoid ambiguity:
///
/// 1. a schema version tag (`forgecache-action-v1`),
/// 2. the command argv,
/// 3. the environment as sorted `KEY=VALUE` pairs (`BTreeMap` is already sorted),
/// 4. the inputs sorted by path, each as `path` + the BLAKE3 digest of its bytes,
/// 5. the dependency digests in sorted order.
///
/// Because every component is sorted and the file contents are hashed, the
/// result is stable across runs and machines.
#[instrument(skip(action, dep_digests), fields(action = %action.id))]
pub fn action_digest_at(
    action: &Action,
    dep_digests: &BTreeMap<String, Digest>,
    base_dir: &Path,
) -> Result<Digest> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"forgecache-action-v1");

    hasher.update(b"|cmd|");
    for arg in &action.command {
        update_len_prefixed(&mut hasher, arg.as_bytes());
    }

    hasher.update(b"|env|");
    for (key, value) in &action.env {
        update_len_prefixed(&mut hasher, key.as_bytes());
        update_len_prefixed(&mut hasher, value.as_bytes());
    }

    hasher.update(b"|inputs|");
    let mut inputs: Vec<&PathBuf> = action.inputs.iter().collect();
    inputs.sort();
    for input in inputs {
        let resolved = resolve(base_dir, input);
        let digest = file_digest(&resolved).map_err(|e| match e {
            ForgeError::MissingFile(_) => ForgeError::MissingInput {
                action: action.id.clone(),
                path: resolved.clone(),
            },
            other => other,
        })?;
        update_len_prefixed(&mut hasher, input.to_string_lossy().as_bytes());
        update_len_prefixed(&mut hasher, digest.as_hex().as_bytes());
    }

    hasher.update(b"|deps|");
    let mut deps: Vec<&Digest> = dep_digests.values().collect();
    deps.sort();
    for digest in deps {
        update_len_prefixed(&mut hasher, digest.as_hex().as_bytes());
    }

    Ok(Digest::from_hex(hasher.finalize().to_hex().to_string()))
}

/// Joins a possibly-relative path onto `base_dir`.
///
/// Absolute paths and paths with a prefix are returned unchanged.
pub fn resolve(base_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }

    fn sample(id: &str, input: &str) -> Action {
        Action {
            id: id.to_string(),
            command: vec!["gcc".into(), "-c".into(), input.into()],
            env: BTreeMap::from([("CC".to_string(), "gcc".to_string())]),
            inputs: vec![PathBuf::from(input)],
            outputs: vec![PathBuf::from("out.o")],
            deps: vec![],
            timeout_ms: 1000,
            tags: vec![],
        }
    }

    #[test]
    fn digest_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.c", "int a;");
        let a = sample("a", "a.c");
        let deps = BTreeMap::new();
        let d1 = action_digest_at(&a, &deps, dir.path()).unwrap();
        let d2 = action_digest_at(&a, &deps, dir.path()).unwrap();
        assert_eq!(d1, d2);
        assert_eq!(d1.as_hex().len(), 64);
    }

    #[test]
    fn digest_changes_when_input_byte_changes() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.c", "int a;");
        let a = sample("a", "a.c");
        let deps = BTreeMap::new();
        let d1 = action_digest_at(&a, &deps, dir.path()).unwrap();
        write(dir.path(), "a.c", "int a = 2;");
        let d2 = action_digest_at(&a, &deps, dir.path()).unwrap();
        assert_ne!(d1, d2);
    }

    #[test]
    fn digest_changes_when_dep_digest_changes() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.c", "int a;");
        let a = sample("a", "a.c");
        let mut deps = BTreeMap::new();
        deps.insert("dep".to_string(), Digest::from_hex("00".repeat(32)));
        let d1 = action_digest_at(&a, &deps, dir.path()).unwrap();
        deps.insert("dep".to_string(), Digest::from_hex("11".repeat(32)));
        let d2 = action_digest_at(&a, &deps, dir.path()).unwrap();
        assert_ne!(d1, d2);
    }

    #[test]
    fn digest_is_independent_of_input_order() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.c", "int a;");
        write(dir.path(), "b.h", "#define B 1");
        let mut a = sample("a", "a.c");
        a.inputs = vec![PathBuf::from("a.c"), PathBuf::from("b.h")];
        let d1 = action_digest_at(&a, &BTreeMap::new(), dir.path()).unwrap();
        a.inputs = vec![PathBuf::from("b.h"), PathBuf::from("a.c")];
        let d2 = action_digest_at(&a, &BTreeMap::new(), dir.path()).unwrap();
        assert_eq!(d1, d2);
    }

    #[test]
    fn digest_independent_of_action_id() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.c", "int a;");
        let a1 = sample("label-one", "a.c");
        let a2 = sample("label-two", "a.c");
        let deps = BTreeMap::new();
        assert_eq!(
            action_digest_at(&a1, &deps, dir.path()).unwrap(),
            action_digest_at(&a2, &deps, dir.path()).unwrap()
        );
    }

    #[test]
    fn missing_input_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let a = sample("a", "missing.c");
        let err = action_digest_at(&a, &BTreeMap::new(), dir.path()).unwrap_err();
        assert!(matches!(err, ForgeError::MissingInput { .. }));
    }
}
