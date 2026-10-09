//! Content digests.
//!
//! ForgeCache uses [BLAKE3] to derive 256-bit content digests. Every digest is
//! rendered as a 64-character lowercase hex string, which is the canonical
//! on-disk and on-the-wire representation.
//!
//! # Stability
//!
//! Digests are *stable across runs*: [`file_digest`] hashes the raw bytes of a
//! file and [`action_digest`] hashes a canonical, deterministic serialization of
//! an action (see [`crate::action`]). No timestamps, process ids, absolute paths
//! outside the declared `base_dir`, or hash-map iteration order are allowed to
//! leak into the digest, so two identical builds on two machines produce the
//! same digests and therefore the same cache keys.
//!
//! [BLAKE3]: https://github.com/BLAKE3-team/BLAKE3

use std::fmt;
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{ForgeError, Result};

/// A 256-bit content digest rendered as 64 lowercase hex characters.
///
/// `Digest` serializes transparently as a JSON string, so action cache entries
/// and RPC messages carry it with no extra structure.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(String);

impl Digest {
    /// Wraps an already-computed hex string. The caller is responsible for the
    /// string actually being a digest.
    pub fn from_hex(hex: impl Into<String>) -> Self {
        Digest(hex.into())
    }

    /// Returns the digest as a lowercase hex string.
    pub fn as_hex(&self) -> &str {
        &self.0
    }

    /// Returns the raw bytes if the digest is a valid 64-character hex string.
    pub fn to_bytes(&self) -> Option<[u8; 32]> {
        let raw = hex::decode(&self.0).ok()?;
        let mut out = [0u8; 32];
        if raw.len() != 32 {
            return None;
        }
        out.copy_from_slice(&raw);
        Some(out)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.0)
    }
}

/// Hashes a byte slice and returns its digest.
pub fn bytes_digest(bytes: &[u8]) -> Digest {
    Digest(blake3::hash(bytes).to_hex().to_string())
}

/// Hashes the full contents of `path` and returns the digest.
///
/// The file is streamed through BLAKE3 so arbitrarily large inputs do not need
/// to fit in memory.
pub fn file_digest(path: impl AsRef<Path>) -> Result<Digest> {
    let path = path.as_ref();
    let file = std::fs::File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ForgeError::MissingFile(path.to_path_buf())
        } else {
            ForgeError::Io(e)
        }
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut reader, &mut hasher)?;
    Ok(Digest(hasher.finalize().to_hex().to_string()))
}

/// Writes `bytes` into an existing hasher with an unambiguous length prefix.
pub(crate) fn update_len_prefixed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Renders a hex string using the standard lowercase alphabet.
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn file_digest_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        std::fs::write(&path, b"hello world").unwrap();
        let d1 = file_digest(&path).unwrap();
        let d2 = file_digest(&path).unwrap();
        assert_eq!(d1, d2);
        assert_eq!(d1.as_hex().len(), 64);
        assert_eq!(d1.as_hex(), bytes_digest(b"hello world").as_hex());
    }

    #[test]
    fn file_digest_changes_when_a_byte_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        std::fs::write(&path, b"hello world").unwrap();
        let d1 = file_digest(&path).unwrap();
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.write_all(b"!").unwrap();
        f.flush().unwrap();
        drop(f);
        let d2 = file_digest(&path).unwrap();
        assert_ne!(d1, d2);
    }

    #[test]
    fn missing_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let err = file_digest(dir.path().join("nope")).unwrap_err();
        assert!(matches!(err, ForgeError::MissingFile(_)));
    }

    #[test]
    fn to_bytes_round_trips() {
        let d = bytes_digest(b"abc");
        assert!(d.to_bytes().is_some());
        assert!(Digest::from_hex("zz").to_bytes().is_none());
    }
}
