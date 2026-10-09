//! The worker daemon: a tokio TCP server that enforces a global slot limit and
//! executes actions on behalf of the coordinator.

use std::net::SocketAddr;
use std::sync::Arc;

use forge_core::protocol::{read_message, write_message, ExecOutcome, Message};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::exec::{execute, WorkerConfig};

/// A running worker. Dropping it without calling [`RunningWorker::shutdown`]
/// detaches the accept loop.
pub struct RunningWorker {
    addr: SocketAddr,
    worker_id: String,
    slots: u32,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl RunningWorker {
    /// The address the worker actually bound (useful when `port == 0`).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The advertised worker id.
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    /// The advertised slot count.
    pub fn slots(&self) -> u32 {
        self.slots
    }

    /// Signals the accept loop to stop and waits for it to finish.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

impl Drop for RunningWorker {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Binds `addr` and starts serving. Returns immediately; the accept loop runs in
/// a background task.
pub async fn start(addr: SocketAddr, config: WorkerConfig) -> std::io::Result<RunningWorker> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let worker_id = config.worker_id.clone();
    let slots = config.slots;
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let semaphore = Arc::new(Semaphore::new(config.slots as usize));

    info!(%local, %worker_id, slots, "worker listening");

    let join = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    debug!("worker accept loop shutting down");
                    break;
                }
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, peer)) => {
                            let config = config.clone();
                            let semaphore = Arc::clone(&semaphore);
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, peer, config, semaphore).await {
                                    debug!(%peer, error = %e, "connection ended with error");
                                }
                            });
                        }
                        Err(e) => warn!(error = %e, "accept failed"),
                    }
                }
            }
        }
    });

    Ok(RunningWorker {
        addr: local,
        worker_id,
        slots,
        shutdown: Some(shutdown_tx),
        join: Some(join),
    })
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    config: WorkerConfig,
    semaphore: Arc<Semaphore>,
) -> std::io::Result<()> {
    let (mut read_half, mut write_half) = stream.into_split();
    write_message(
        &mut write_half,
        &Message::Hello {
            worker_id: config.worker_id.clone(),
            slots: config.slots,
        },
    )
    .await?;

    let (tx, mut rx) = mpsc::channel::<Message>(256);
    let writer = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if write_message(&mut write_half, &message).await.is_err() {
                break;
            }
        }
    });

    loop {
        match read_message(&mut read_half).await {
            Ok(Message::ExecRequest {
                request_id,
                action,
                action_digest,
                input_blobs,
            }) => {
                debug!(%peer, id = %action.id, digest = %action_digest, "exec request");
                let tx = tx.clone();
                let semaphore = Arc::clone(&semaphore);
                let config = config.clone();
                tokio::spawn(async move {
                    let permit = match semaphore.acquire().await {
                        Ok(permit) => permit,
                        Err(_) => return,
                    };
                    let outcome = match execute(&config, &action, &input_blobs).await {
                        Ok(success) => ExecOutcome::Success(success),
                        Err(error) => ExecOutcome::Error(error),
                    };
                    drop(permit);
                    let _ = tx
                        .send(Message::ExecResponse {
                            request_id,
                            result: outcome,
                        })
                        .await;
                });
            }
            Ok(Message::Ping { nonce }) => {
                let _ = tx.send(Message::Pong { nonce }).await;
            }
            Ok(_) => {}
            Err(e) => {
                debug!(%peer, error = %e, "connection read ended");
                break;
            }
        }
    }

    drop(tx);
    let _ = writer.await;
    Ok(())
}
