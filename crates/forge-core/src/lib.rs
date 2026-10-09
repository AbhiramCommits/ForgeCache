//! # ForgeCache core
//!
//! The `forge-core` crate contains the parts of ForgeCache that both the
//! coordinator and the workers agree on:
//!
//! * [`Action`] — the hermetic action model.
//! * [`Digest`](digest::Digest) and [`action`] — stable, content-addressed
//!   digests computed with BLAKE3.
//! * [`Cas`] — a filesystem content-addressed store.
//! * [`ActionCache`] — a persistent map from action digest to [`ActionResult`].
//! * [`BuildGraph`] — manifest loading, validation, cycle detection, and
//!   topological layering.
//!
//! Everything in this crate is designed so that identical inputs produce
//! identical digests on any machine, which is what makes the action cache
//! correct rather than merely fast.

pub mod action;
pub mod action_cache;
pub mod cas;
pub mod digest;
pub mod error;
pub mod graph;

pub use action::{action_digest, action_digest_at, resolve, Action};
pub use action_cache::{ActionCache, ActionResult};
pub use cas::{Cas, PutOutcome};
pub use digest::{bytes_digest, file_digest, Digest};
pub use error::{ForgeError, Result};
pub use graph::BuildGraph;

/// Default relative path (under the working directory) of the ForgeCache state.
pub const FORGE_DIR: &str = ".forge";

/// Returns the CAS root for a given working directory.
pub fn cas_dir(workdir: impl AsRef<std::path::Path>) -> std::path::PathBuf {
    workdir.as_ref().join(FORGE_DIR).join("cas")
}

/// Returns the action-cache root for a given working directory.
pub fn ac_dir(workdir: impl AsRef<std::path::Path>) -> std::path::PathBuf {
    workdir.as_ref().join(FORGE_DIR).join("ac")
}
