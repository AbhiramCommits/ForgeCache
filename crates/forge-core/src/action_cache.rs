//! The action cache: a persistent map from action digest to execution result.
//!
//! Entries are stored as one JSON file per key at
//! `.forge/ac/<first-two-hex>/<action-digest>.json`. The value,
//! [`ActionResult`], records the exit code, digests of the captured streams and
//! declared outputs, and the wall time of the original execution. Because the
//! output bytes themselves live in the CAS, a cache hit can restore byte-identical
//! outputs without ever contacting a worker.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::trace;

use crate::digest::{bytes_digest, Digest};
use crate::error::{ForgeError, Result};

/// The recorded result of executing an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    /// Process exit code (0 on success).
    pub exit_code: i32,
    /// Digest of the captured stdout.
    pub stdout_digest: Digest,
    /// Digest of the captured stderr.
    pub stderr_digest: Digest,
    /// Digest of each declared output file, keyed by its relative path.
    pub output_digests: BTreeMap<PathBuf, Digest>,
    /// Wall-clock duration of the original execution, in milliseconds.
    pub wall_ms: u64,
}

impl ActionResult {
    /// Convenience constructor used by workers and tests.
    pub fn new(
        exit_code: i32,
        stdout: &[u8],
        stderr: &[u8],
        output_digests: BTreeMap<PathBuf, Digest>,
        wall_ms: u64,
    ) -> Self {
        ActionResult {
            exit_code,
            stdout_digest: bytes_digest(stdout),
            stderr_digest: bytes_digest(stderr),
            output_digests,
            wall_ms,
        }
    }
}

/// A JSON-file-backed action cache.
#[derive(Debug, Clone)]
pub struct ActionCache {
    root: PathBuf,
}

impl ActionCache {
    /// Opens (creating if needed) the action cache rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(ActionCache { root })
    }

    /// Root directory of the action cache.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn entry_path(&self, action_digest: &Digest) -> PathBuf {
        let hex = action_digest.as_hex();
        let (prefix, _) = hex.split_at(2.min(hex.len()));
        self.root.join(prefix).join(format!("{hex}.json"))
    }

    /// Looks up a previously recorded result.
    pub fn get(&self, action_digest: &Digest) -> Option<ActionResult> {
        let path = self.entry_path(action_digest);
        let bytes = std::fs::read(&path).ok()?;
        match serde_json::from_slice::<ActionResult>(&bytes) {
            Ok(result) => {
                trace!(digest = %action_digest, "action cache hit");
                Some(result)
            }
            Err(e) => {
                trace!(digest = %action_digest, error = %e, "corrupt action cache entry ignored");
                None
            }
        }
    }

    /// Returns `true` if an entry exists for `action_digest`.
    pub fn has(&self, action_digest: &Digest) -> bool {
        self.entry_path(action_digest).is_file()
    }

    /// Persists a result, atomically replacing any existing entry.
    pub fn put(&self, action_digest: &Digest, result: &ActionResult) -> Result<()> {
        let path = self.entry_path(action_digest);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(result)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Removes an entry. Missing entries are ignored.
    pub fn remove(&self, action_digest: &Digest) -> Result<()> {
        let path = self.entry_path(action_digest);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ForgeError::Io(e)),
        }
    }

    /// Removes every entry, clearing the cache.
    pub fn clear(&self) -> Result<usize> {
        let mut removed = 0;
        if self.root.is_dir() {
            let mut stack = vec![self.root.clone()];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    if entry.file_type()?.is_dir() {
                        stack.push(path);
                    } else if path.extension().is_some_and(|e| e == "json") {
                        std::fs::remove_file(&path)?;
                        removed += 1;
                    }
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(n: &str) -> Digest {
        bytes_digest(n.as_bytes())
    }

    fn result() -> ActionResult {
        ActionResult::new(
            0,
            b"stdout",
            b"stderr",
            BTreeMap::from([(PathBuf::from("out.o"), digest("obj"))]),
            12,
        )
    }

    #[test]
    fn miss_then_hit() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ActionCache::new(dir.path().join("ac")).unwrap();
        let key = digest("action");
        assert!(!cache.has(&key));
        assert!(cache.get(&key).is_none());

        cache.put(&key, &result()).unwrap();
        assert!(cache.has(&key));
        assert_eq!(cache.get(&key).unwrap(), result());
    }

    #[test]
    fn put_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ActionCache::new(dir.path().join("ac")).unwrap();
        let key = digest("action");
        cache.put(&key, &result()).unwrap();
        let mut updated = result();
        updated.exit_code = 1;
        cache.put(&key, &updated).unwrap();
        assert_eq!(cache.get(&key).unwrap().exit_code, 1);
    }

    #[test]
    fn clear_removes_all() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ActionCache::new(dir.path().join("ac")).unwrap();
        cache.put(&digest("a"), &result()).unwrap();
        cache.put(&digest("b"), &result()).unwrap();
        assert_eq!(cache.clear().unwrap(), 2);
        assert!(!cache.has(&digest("a")));
    }

    #[test]
    fn corrupt_entry_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ac");
        let cache = ActionCache::new(&root).unwrap();
        let key = digest("action");
        cache.put(&key, &result()).unwrap();
        std::fs::write(cache.entry_path(&key), b"{ not json").unwrap();
        assert!(cache.get(&key).is_none());
    }
}
