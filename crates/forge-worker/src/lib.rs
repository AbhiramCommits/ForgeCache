//! # ForgeCache worker
//!
//! The worker crate implements three things:
//!
//! * [`exec`] — hermetic action execution with sandboxed scratch directories,
//!   process-group isolation, timeouts, and output collection.
//! * [`server`] — a tokio TCP daemon that speaks the [`forge_core::protocol`]
//!   wire format and enforces a global slot limit.
//! * a small `forge-worker` binary (see `main.rs`) that wires the two together.
//!
//! Both the local subprocess backend and an optional Docker backend are
//! supported.

pub mod exec;
pub mod server;

pub use exec::{execute, DockerConfig, Executor, WorkerConfig};
pub use server::{start, RunningWorker};
