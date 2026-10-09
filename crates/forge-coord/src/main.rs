//! `forge-coord` — the ForgeCache coordinator CLI.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use forge_coord::{explain, run_build, BuildOptions, BuildState};
use forge_core::{ac_dir, ActionCache, BuildGraph};
use tracing_subscriber::EnvFilter;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

#[derive(Parser, Debug)]
#[command(name = "forge-coord", version, about = "ForgeCache coordinator")]
struct Cli {
    /// Log format.
    #[arg(long, value_enum, default_value_t = LogFormat::Text, global = true)]
    log_format: LogFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Plan and execute a manifest.
    Build(BuildArgs),
    /// Explain why a target would rebuild.
    Explain(ExplainArgs),
}

#[derive(Args, Debug)]
struct BuildArgs {
    /// Path to the JSON manifest.
    #[arg(long)]
    manifest: PathBuf,
    /// Comma-separated worker addresses (`host:port,...`).
    #[arg(long)]
    workers: String,
    /// Global in-flight cap.
    #[arg(long, default_value_t = 4)]
    jobs: usize,
    /// Do not read or write the action cache.
    #[arg(long, default_value_t = false)]
    no_cache: bool,
    /// Remove `.forge/` before building.
    #[arg(long, default_value_t = false)]
    clean_cache: bool,
    /// Write the JSON report to this path.
    #[arg(long)]
    report: Option<PathBuf>,
    /// Execute in-process if every worker is unavailable.
    #[arg(long, default_value_t = false)]
    local_fallback: bool,
    /// Consecutive worker faults before ejecting it.
    #[arg(long, default_value_t = 3)]
    eject_after: u32,
}

#[derive(Args, Debug)]
struct ExplainArgs {
    /// Path to the JSON manifest.
    #[arg(long)]
    manifest: PathBuf,
    /// Target action id to explain.
    #[arg(long)]
    target: String,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.log_format);

    match cli.command {
        Command::Build(args) => run_build_command(args),
        Command::Explain(args) => run_explain_command(args),
    }
}

fn run_build_command(args: BuildArgs) -> anyhow::Result<()> {
    let workers = parse_workers(&args.workers)?;
    let mut options = BuildOptions::new(&args.manifest, workers);
    options.jobs = args.jobs;
    options.no_cache = args.no_cache;
    options.clean_cache = args.clean_cache;
    options.local_fallback = args.local_fallback;
    options.eject_after = args.eject_after;
    options.report = args.report;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let report = runtime.block_on(run_build(&options))?;
    println!("{}", report.summary());
    Ok(())
}

fn run_explain_command(args: ExplainArgs) -> anyhow::Result<()> {
    let graph = BuildGraph::from_manifest(&args.manifest)?;
    let base_dir = graph.base_dir().to_path_buf();
    let action_cache = ActionCache::new(ac_dir(&base_dir))?;
    let state = BuildState::load(&forge_coord::state::state_path(&base_dir))?;
    let explanation = explain(&graph, &action_cache, &state, &args.target)?;

    println!("target          : {}", explanation.id);
    println!("digest          : {}", explanation.digest);
    println!(
        "previous digest : {}",
        explanation.previous_digest.as_deref().unwrap_or("<none>")
    );
    println!("cached          : {}", explanation.cached);
    println!("up to date      : {}", explanation.up_to_date);
    if explanation.up_to_date {
        println!("would rebuild   : no");
    } else {
        println!("would rebuild   : yes");
        println!("reasons:");
        for change in &explanation.changes {
            println!("  - {change}");
        }
    }
    Ok(())
}

fn parse_workers(spec: &str) -> anyhow::Result<Vec<SocketAddr>> {
    use std::net::ToSocketAddrs;
    let mut out = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        // Accept both literal IPs and DNS names (e.g. Kubernetes Service DNS or
        // docker-compose service names). Each name may resolve to many addresses.
        let resolved: Vec<SocketAddr> = part
            .to_socket_addrs()
            .map_err(|e| anyhow::anyhow!("cannot resolve worker address '{part}': {e}"))?
            .collect();
        if resolved.is_empty() {
            anyhow::bail!("worker address '{part}' resolved to nothing");
        }
        out.extend(resolved);
    }
    if out.is_empty() {
        anyhow::bail!("--workers must list at least one address");
    }
    Ok(out)
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
