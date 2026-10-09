//! The ForgeCache worker wire protocol.
//!
//! Coordinator and worker speak length-prefixed JSON over TCP. Each frame is a
//! 4-byte big-endian length followed by a UTF-8 JSON [`Message`]. The protocol
//! is intentionally simple (rather than gRPC/REAPI) so the whole system stays
//! inspectable and dependency-light; see the README's limitations section.
//!
//! Messages:
//!
//! * [`Message::Hello`] — sent by the worker immediately on accept, advertising
//!   its id and execution slots.
//! * [`Message::ExecRequest`] — sent by the coordinator to run an action. Input
//!   bytes travel inline as [`Blob`]s so workers need no shared filesystem.
//! * [`Message::ExecResponse`] — the worker's reply, either [`ExecOutcome::Success`]
//!   or [`ExecOutcome::Error`].
//! * [`Message::Ping`] / [`Message::Pong`] — liveness probe used by health checks.
//!
//! Every request carries a `request_id`, which lets one TCP connection carry
//! many in-flight requests (the coordinator multiplexes responses by id).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::action::Action;
use crate::action_cache::ActionResult;
use crate::digest::Digest;

/// Maximum accepted frame size (256 MiB).
pub const MAX_FRAME: u32 = 256 * 1024 * 1024;

/// A named chunk of bytes addressed by its content digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blob {
    /// Digest of `data`.
    pub digest: Digest,
    /// Raw bytes.
    pub data: Vec<u8>,
}

impl Blob {
    /// Wraps bytes, computing the digest.
    pub fn new(data: Vec<u8>) -> Self {
        let digest = crate::digest::bytes_digest(&data);
        Blob { digest, data }
    }
}

/// The result of a *successful* round trip. A non-zero `exit_code` is still a
/// success at the protocol level: it is a real compiler result, not a transport
/// failure, and must not be retried.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecSuccess {
    /// The recorded action result (exit code, stream/output digests, wall time).
    pub result: ActionResult,
    /// Captured stdout bytes.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes.
    pub stderr: Vec<u8>,
    /// The declared output files, by relative path.
    pub outputs: BTreeMap<PathBuf, Blob>,
}

/// A worker-side failure: the action could not be run to completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecError {
    /// A stable machine-readable kind, e.g. `"timeout"` or `"missing_output"`.
    pub kind: String,
    /// A human-readable message.
    pub msg: String,
}

impl ExecError {
    /// Constructs an error.
    pub fn new(kind: impl Into<String>, msg: impl Into<String>) -> Self {
        ExecError {
            kind: kind.into(),
            msg: msg.into(),
        }
    }

    /// Returns `true` when a coordinator should retry this error.
    ///
    /// Timeouts, kills, and worker-side internal failures are transient;
    /// missing outputs and spawn errors are treated as deterministic failures.
    pub fn is_retryable(&self) -> bool {
        matches!(self.kind.as_str(), "timeout" | "killed" | "internal" | "io")
    }
}

/// The payload of an [`Message::ExecResponse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecOutcome {
    /// The action ran and produced a result.
    Success(ExecSuccess),
    /// The action could not complete.
    Error(ExecError),
}

/// A protocol message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// Worker greeting sent on accept.
    Hello {
        /// Stable worker identifier.
        worker_id: String,
        /// How many actions the worker may run concurrently.
        slots: u32,
    },
    /// Coordinator request to execute an action.
    ExecRequest {
        /// Correlates the response on a multiplexed connection.
        request_id: u64,
        /// The action to run.
        action: Action,
        /// The action's content-addressed digest (for logging/telemetry).
        action_digest: Digest,
        /// Input files keyed by their relative path.
        input_blobs: BTreeMap<PathBuf, Blob>,
    },
    /// Worker reply to an [`Message::ExecRequest`].
    ExecResponse {
        /// The `request_id` being answered.
        request_id: u64,
        /// Success or error.
        result: ExecOutcome,
    },
    /// Liveness probe.
    Ping {
        /// Echoed back in the matching [`Message::Pong`].
        nonce: u64,
    },
    /// Liveness reply.
    Pong {
        /// The nonce from the matching [`Message::Ping`].
        nonce: u64,
    },
}

/// Errors surfaced by the client side of the protocol.
#[derive(Debug, Error)]
pub enum ClientError {
    /// The connection failed or closed; retryable.
    #[error("transport error: {0}")]
    Transport(String),
    /// The worker reported an execution error; retryable only for some kinds.
    #[error("remote error [{kind}]: {msg}")]
    Remote {
        /// Machine-readable kind.
        kind: String,
        /// Human-readable message.
        msg: String,
    },
    /// An operation exceeded its deadline on the client side.
    #[error("client timeout after {0:?}")]
    Timeout(std::time::Duration),
}

impl ClientError {
    /// Returns `true` when a coordinator should retry this error.
    pub fn is_retryable(&self) -> bool {
        match self {
            ClientError::Transport(_) | ClientError::Timeout(_) => true,
            ClientError::Remote { kind, .. } => ExecError {
                kind: kind.clone(),
                msg: String::new(),
            }
            .is_retryable(),
        }
    }
}

/// Writes one length-prefixed JSON frame.
pub async fn write_message<W>(writer: &mut W, message: &Message) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(message)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if payload.len() as u64 > MAX_FRAME as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one length-prefixed JSON frame.
pub async fn read_message<R>(reader: &mut R) -> std::io::Result<Message>
where
    R: AsyncRead + Unpin,
{
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes).await?;
    let len = u32::from_be_bytes(len_bytes);
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let msg = Message::Hello {
            worker_id: "w1".into(),
            slots: 4,
        };
        write_message(&mut a, &msg).await.unwrap();
        let got = read_message(&mut b).await.unwrap();
        match got {
            Message::Hello { worker_id, slots } => {
                assert_eq!(worker_id, "w1");
                assert_eq!(slots, 4);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn exec_error_retryability() {
        assert!(ExecError::new("timeout", "").is_retryable());
        assert!(!ExecError::new("missing_output", "").is_retryable());
    }

    #[test]
    fn blob_digest_matches_data() {
        let blob = Blob::new(b"payload".to_vec());
        assert_eq!(blob.digest, crate::digest::bytes_digest(b"payload"));
    }
}
