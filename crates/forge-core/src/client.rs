//! A multiplexed async client for the ForgeCache worker protocol.
//!
//! A [`WorkerClient`] opens one TCP connection to a worker and keeps it for the
//! lifetime of the build. Requests carry a monotonically increasing
//! `request_id`; a reader task routes replies back to the waiting caller, so
//! many actions can be in flight over a single connection. This is what lets
//! the coordinator saturate a worker's advertised slots.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, trace};

use crate::action::Action;
use crate::digest::Digest;
use crate::protocol::{
    read_message, write_message, Blob, ClientError, ExecOutcome, ExecSuccess, Message,
};

enum Pending {
    Exec(oneshot::Sender<Result<ExecOutcome, ClientError>>),
    Pong(oneshot::Sender<Result<(), ClientError>>),
}

/// A live connection to a single worker.
pub struct WorkerClient {
    worker_id: String,
    slots: u32,
    addr: SocketAddr,
    outbound: mpsc::Sender<Message>,
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    next_id: AtomicU64,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl WorkerClient {
    /// Connects to `addr` and completes the `Hello` handshake.
    pub async fn connect(addr: SocketAddr) -> Result<Self, ClientError> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| ClientError::Transport(format!("connect {addr}: {e}")))?;
        let _ = stream.set_nodelay(true);
        let (mut read_half, mut write_half) = stream.into_split();

        let hello = read_message(&mut read_half)
            .await
            .map_err(|e| ClientError::Transport(format!("read hello: {e}")))?;
        let (worker_id, slots) = match hello {
            Message::Hello { worker_id, slots } => (worker_id, slots),
            other => {
                return Err(ClientError::Transport(format!(
                    "expected Hello, got {other:?}"
                )))
            }
        };
        debug!(%addr, %worker_id, slots, "connected to worker");

        let (outbound, mut outbound_rx) = mpsc::channel::<Message>(1024);
        let pending: Arc<Mutex<HashMap<u64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));

        let writer = tokio::spawn(async move {
            while let Some(message) = outbound_rx.recv().await {
                if write_message(&mut write_half, &message).await.is_err() {
                    break;
                }
            }
        });

        let reader_pending = Arc::clone(&pending);
        let reader = tokio::spawn(async move {
            loop {
                match read_message(&mut read_half).await {
                    Ok(Message::ExecResponse { request_id, result }) => {
                        if let Some(Pending::Exec(tx)) =
                            reader_pending.lock().await.remove(&request_id)
                        {
                            let _ = tx.send(Ok(result));
                        }
                    }
                    Ok(Message::Pong { nonce }) => {
                        if let Some(Pending::Pong(tx)) = reader_pending.lock().await.remove(&nonce)
                        {
                            let _ = tx.send(Ok(()));
                        }
                    }
                    Ok(other) => trace!(?other, "ignoring unexpected message"),
                    Err(e) => {
                        let reason = e.to_string();
                        let mut pending = reader_pending.lock().await;
                        for (_, slot) in pending.drain() {
                            match slot {
                                Pending::Exec(tx) => {
                                    let _ = tx.send(Err(ClientError::Transport(reason.clone())));
                                }
                                Pending::Pong(tx) => {
                                    let _ = tx.send(Err(ClientError::Transport(reason.clone())));
                                }
                            }
                        }
                        break;
                    }
                }
            }
        });

        Ok(WorkerClient {
            worker_id,
            slots,
            addr,
            outbound,
            pending,
            next_id: AtomicU64::new(1),
            reader,
            writer,
        })
    }

    /// The worker's advertised id.
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    /// The worker's advertised concurrency slots.
    pub fn slots(&self) -> u32 {
        self.slots
    }

    /// The worker's address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Returns `true` if both background tasks are still running.
    pub fn is_alive(&self) -> bool {
        !self.reader.is_finished() && !self.writer.is_finished()
    }

    /// Executes an action, waiting at most `client_timeout` for a reply.
    ///
    /// A worker-level execution failure is returned as
    /// [`ClientError::Remote`]; a genuine non-zero process exit is an
    /// [`ExecSuccess`] with a non-zero `result.exit_code`.
    pub async fn exec(
        &self,
        action: &Action,
        action_digest: &Digest,
        input_blobs: BTreeMap<PathBuf, Blob>,
        client_timeout: Duration,
    ) -> Result<ExecSuccess, ClientError> {
        let request_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(request_id, Pending::Exec(tx));

        let message = Message::ExecRequest {
            request_id,
            action: action.clone(),
            action_digest: action_digest.clone(),
            input_blobs,
        };
        if self.outbound.send(message).await.is_err() {
            self.pending.lock().await.remove(&request_id);
            return Err(ClientError::Transport("outbound channel closed".into()));
        }

        match tokio::time::timeout(client_timeout, rx).await {
            Err(_) => {
                self.pending.lock().await.remove(&request_id);
                Err(ClientError::Timeout(client_timeout))
            }
            Ok(Err(_)) => Err(ClientError::Transport("response channel dropped".into())),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Ok(Ok(ExecOutcome::Success(success)))) => Ok(success),
            Ok(Ok(Ok(ExecOutcome::Error(err)))) => Err(ClientError::Remote {
                kind: err.kind,
                msg: err.msg,
            }),
        }
    }

    /// Sends a ping and waits for the matching pong.
    pub async fn ping(&self, timeout: Duration) -> Result<(), ClientError> {
        let nonce = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(nonce, Pending::Pong(tx));
        if self.outbound.send(Message::Ping { nonce }).await.is_err() {
            self.pending.lock().await.remove(&nonce);
            return Err(ClientError::Transport("outbound channel closed".into()));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => Err(e),
            _ => Err(ClientError::Timeout(timeout)),
        }
    }

    /// Aborts the background tasks, forcing the connection closed.
    pub fn abort(&self) {
        self.reader.abort();
        self.writer.abort();
    }
}

impl Drop for WorkerClient {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}
