//! Error types shared across the ForgeCache core.

use std::path::PathBuf;

/// The single error type used by `forge-core`.
///
/// Every fallible operation in the core returns [`Result<T>`], which is an alias
/// for `std::result::Result<T, ForgeError>`.
#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    /// An underlying filesystem or IO operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON document could not be parsed or serialized.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A path that was expected to exist did not.
    #[error("missing file: {0}")]
    MissingFile(PathBuf),

    /// An action declared an input that does not exist on disk.
    #[error("action '{action}' references missing input '{path}'")]
    MissingInput {
        /// Id of the action with the missing input.
        action: String,
        /// The input path that was not found.
        path: PathBuf,
    },

    /// A dependency edge pointed at an action id that is not in the graph.
    #[error("action '{action}' depends on unknown action '{dep}'")]
    UnknownDep {
        /// Id of the action that referenced the unknown dependency.
        action: String,
        /// The unknown dependency id.
        dep: String,
    },

    /// Two actions in the manifest shared the same id.
    #[error("duplicate action id '{0}'")]
    DuplicateAction(String),

    /// The dependency graph contains a cycle. The message names the cycle.
    #[error("dependency cycle detected: {0}")]
    Cycle(String),

    /// A digest was requested from the CAS but is not present.
    #[error("digest not present in CAS: {0}")]
    DigestNotFound(String),

    /// A scratch/working directory could not be prepared.
    #[error("sandbox error: {0}")]
    Sandbox(String),

    /// The worker returned an error for an action.
    #[error("remote action failed: {0}")]
    Remote(String),

    /// The manifest JSON did not match the expected schema.
    #[error("invalid manifest: {0}")]
    Manifest(String),
}

/// Convenience alias for results produced by the core.
pub type Result<T> = std::result::Result<T, ForgeError>;
