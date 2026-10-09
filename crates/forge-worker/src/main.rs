//! `forge-worker` — the ForgeCache execution daemon.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use forge_worker::{start, DockerConfig, Executor, WorkerConfig};
use tracing_subscriber::EnvFilter;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ExecutorArg {
    Local,
    Docker,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

#[derive(Parser, Debug)]
#[command(name = "forge-worker", version, about = "ForgeCache worker daemon")]
struct Cli {
    /// Address to listen on. Port `0` picks an ephemeral port.
    #[arg(long, default_value = "0.0.0.0:7701")]
    listen: String,

    /// Maximum number of concurrent actions.
    #[arg(long, default_value_t = 4)]
    slots: u32,

    /// Directory used for scratch sandboxes.
    #[arg(long, default_value = "/tmp/forge-worker")]
    workdir: PathBuf,

    /// Execution backend.
    #[arg(long, value_enum, default_value_t = ExecutorArg::Local)]
    executor: ExecutorArg,

    /// Docker image (docker executor only).
    #[arg(long, default_value = "forgecache-worker:latest")]
    image: String,

    /// CPU limit for the docker executor.
    #[arg(long, default_value_t = 1.0)]
    cpus: f32,

    /// Memory limit in MiB for the docker executor.
    #[arg(long, default_value_t = 1024)]
    memory: u64,

    /// Worker id advertised in `Hello`.
    #[arg(long, default_value = "worker")]
    id: String,

    /// Log format.
    #[arg(long, value_enum, default_value_t = LogFormat::Text)]
    log_format: LogFormat,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.log_format);

    let addr: SocketAddr = cli
        .listen
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --listen '{}': {e}", cli.listen))?;

    let executor = match cli.executor {
        ExecutorArg::Local => Executor::Local,
        ExecutorArg::Docker => Executor::Docker(DockerConfig {
            image: cli.image.clone(),
            cpus: cli.cpus,
            memory_mb: cli.memory,
        }),
    };

    std::fs::create_dir_all(&cli.workdir)?;
    let config = WorkerConfig {
        worker_id: cli.id.clone(),
        slots: cli.slots,
        workdir: cli.workdir.clone(),
        executor,
    };

    let worker = start(addr, config).await?;
    println!(
        "forge-worker listening on {} (id={}, slots={})",
        worker.addr(),
        worker.worker_id(),
        worker.slots()
    );

    tokio::signal::ctrl_c().await?;
    println!("forge-worker shutting down");
    worker.shutdown().await;
    Ok(())
}

fn init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match format {
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
        LogFormat::Text => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }
}
