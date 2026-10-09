//! # ForgeCache coordinator
//!
//! Library entry points for the coordinator: manifest planning
//! ([`planner`]), the cache-aware scheduler ([`build`]), the build [`report`],
//! persistent [`state`] for `explain`, and the [`explain`] implementation.

pub mod build;
pub mod explain;
pub mod planner;
pub mod report;
pub mod state;

/// Convenience result type for the coordinator.
pub type Result<T> = anyhow::Result<T>;

pub use build::{run_build, BuildOptions};
pub use explain::{explain, Explanation};
pub use report::{ActionReport, ActionStatus, Report};
pub use state::{BuildState, TargetState};
