//! The filesystem content-addressed store (CAS).
//!
//! Blobs live at `.forge/cas/<first-two-hex>/<full-hex>`. Writing is
//! content-addressed and idempotent: putting the same bytes twice is a no-op.
//! Reads are materialized into a destination path by hard link when possible,
//! falling back to a copy across filesystem boundaries.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

use crate::digest::{bytes_digest, file_digest, Digest};
use crate::error::{ForgeError, Result};

/// A filesystem-backed content-addressed store rooted at a directory.
#[derive(Debug, Clone)]
pub struct Cas {
    root: PathBuf,
}

/// The result of [`Cas::put_file`], reporting whether the blob already existed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutOutcome {
    /// Digest of the stored blob.
    pub digest: Digest,
    /// Number of file bytes stored (not counting the digest metadata).
    pub bytes: u64,
    /// `true` when the blob was already present and nothing was written.
    pub deduplicated: bool,
}

impl Cas {
    /// Opens (creating if needed) a CAS rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("tmp"))?;
        Ok(Cas { root })
    }

    /// Root directory of the store (the `.forge/cas` directory).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the on-disk path a digest would occupy.
    pub fn path_for(&self, digest: &Digest) -> PathBuf {
        let hex = digest.as_hex();
        let (prefix, _) = hex.split_at(2.min(hex.len()));
        self.root.join(prefix).join(hex)
    }

    /// Returns `true` if the blob is already stored.
    pub fn has(&self, digest: &Digest) -> bool {
        self.path_for(digest).is_file()
    }

    /// Stores raw bytes and returns the [`PutOutcome`].
    pub fn put_bytes(&self, bytes: &[u8]) -> Result<PutOutcome> {
        let digest = bytes_digest(bytes);
        self.put_bytes_with_digest(&digest, bytes)
    }

    /// Stores the contents of `src` into the CAS.
    pub fn put_file(&self, src: impl AsRef<Path>) -> Result<PutOutcome> {
        let src = src.as_ref();
        let digest = file_digest(src)?;
        let dest = self.path_for(&digest);
        if dest.is_file() {
            let bytes = std::fs::metadata(&dest)?.len();
            return Ok(PutOutcome {
                digest,
                bytes,
                deduplicated: true,
            });
        }
        let bytes = std::fs::metadata(src)?.len();
        self.atomic_write(&dest, |tmp| {
            std::fs::copy(src, tmp)?;
            Ok(())
        })?;
        Ok(PutOutcome {
            digest,
            bytes,
            deduplicated: false,
        })
    }

    fn put_bytes_with_digest(&self, digest: &Digest, bytes: &[u8]) -> Result<PutOutcome> {
        let dest = self.path_for(digest);
        if dest.is_file() {
            return Ok(PutOutcome {
                digest: digest.clone(),
                bytes: bytes.len() as u64,
                deduplicated: true,
            });
        }
        self.atomic_write(&dest, |tmp| {
            std::fs::write(tmp, bytes)?;
            Ok(())
        })?;
        Ok(PutOutcome {
            digest: digest.clone(),
            bytes: bytes.len() as u64,
            deduplicated: false,
        })
    }

    fn atomic_write<F>(&self, dest: &Path, write: F) -> Result<()>
    where
        F: FnOnce(&Path) -> std::io::Result<()>,
    {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp =
            self.root
                .join("tmp")
                .join(format!("{}.{}.tmp", std::process::id(), unique_suffix()));
        write(&tmp)?;
        match std::fs::rename(&tmp, dest) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                // Another writer may have won the race; that is fine.
                if dest.is_file() {
                    Ok(())
                } else {
                    Err(ForgeError::Io(e))
                }
            }
        }
    }

    /// Loads a blob into memory.
    pub fn get_bytes(&self, digest: &Digest) -> Result<Vec<u8>> {
        let path = self.path_for(digest);
        std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ForgeError::DigestNotFound(digest.as_hex().to_string())
            } else {
                ForgeError::Io(e)
            }
        })
    }

    /// Materializes a blob at `dest`, hard-linking when possible.
    ///
    /// Any existing file at `dest` is replaced. Falls back to a copy when the
    /// CAS and `dest` are on different filesystems (hard links cannot cross
    /// mounts).
    pub fn get_file(&self, digest: &Digest, dest: impl AsRef<Path>) -> Result<()> {
        let src = self.path_for(digest);
        if !src.is_file() {
            return Err(ForgeError::DigestNotFound(digest.as_hex().to_string()));
        }
        let dest = dest.as_ref();
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if dest.exists() {
            std::fs::remove_file(dest)?;
        }
        match std::fs::hard_link(&src, dest) {
            Ok(()) => {
                trace!(digest = %digest, "materialized via hardlink");
                Ok(())
            }
            Err(_) => {
                std::fs::copy(&src, dest)?;
                debug!(digest = %digest, "materialized via copy");
                Ok(())
            }
        }
    }
}

fn unique_suffix() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_get_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::new(dir.path().join("cas")).unwrap();
        let src = dir.path().join("greeting.txt");
        std::fs::write(&src, b"hello forge").unwrap();

        let outcome = cas.put_file(&src).unwrap();
        assert!(!outcome.deduplicated);
        assert!(cas.has(&outcome.digest));

        let restored = dir.path().join("restored.txt");
        cas.get_file(&outcome.digest, &restored).unwrap();
        assert_eq!(std::fs::read(&restored).unwrap(), b"hello forge");
        assert_eq!(file_digest(&restored).unwrap(), outcome.digest);
    }

    #[test]
    fn put_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::new(dir.path().join("cas")).unwrap();
        let src = dir.path().join("x");
        std::fs::write(&src, b"same").unwrap();
        let first = cas.put_file(&src).unwrap();
        assert!(!first.deduplicated);
        let second = cas.put_file(&src).unwrap();
        assert!(second.deduplicated);
        assert_eq!(first.digest, second.digest);
    }

    #[test]
    fn get_missing_digest_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::new(dir.path().join("cas")).unwrap();
        let missing = Digest::from_hex("ab".repeat(32));
        assert!(!cas.has(&missing));
        assert!(matches!(
            cas.get_bytes(&missing).unwrap_err(),
            ForgeError::DigestNotFound(_)
        ));
    }

    #[test]
    fn path_layout_uses_two_char_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::new(dir.path().join("cas")).unwrap();
        let d = Digest::from_hex("abcdef");
        let p = cas.path_for(&d);
        assert!(p.ends_with("ab/abcdef"));
    }
}
