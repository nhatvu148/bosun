//! Bosun — an engine-agnostic, agent-ergonomic Docker MCP server.
//!
//! Bosun exposes container lifecycle, logs, stats and Compose as MCP tools whose
//! defining property is *token-bounded I/O*: every read caps and summarizes by
//! default, every destructive write is gated, and the whole thing talks the plain
//! Docker Engine socket so it drives Docker, OrbStack, Colima or Podman alike.

mod bound;
mod engine;
mod safety;
mod server;
mod tools;

use clap::Parser;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

use crate::engine::client::EngineClient;
use crate::server::BosunServer;

#[derive(Parser, Debug)]
#[command(
    name = "bosun",
    version,
    about = "Engine-agnostic Docker MCP server with token-bounded I/O",
    long_about = "Bosun is an MCP server for local container operations. It speaks the plain \
                  Docker Engine API, so it drives Docker, OrbStack, Colima or Podman \
                  interchangeably. Reads are bounded and summarizing by default; destructive \
                  writes require dry_run or an explicit confirm token.\n\n\
                  Run with no arguments to serve MCP over stdio."
)]
struct Cli {
    /// Docker socket path or URL. Overrides auto-discovery.
    #[arg(long, value_name = "PATH")]
    socket: Option<String>,

    /// Log verbosity: error, warn, info, debug, trace. Logs go to stderr.
    /// Defaults to 'info' when serving, 'warn' with --check.
    #[arg(long, value_name = "LEVEL")]
    log_level: Option<String>,

    /// Resolve and print the engine Bosun would bind to, then exit.
    #[arg(long)]
    check: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // `--check` is a human-facing one-shot, so the startup chatter that is useful
    // when serving is just noise around five lines of answer. An explicit
    // --log-level or BOSUN_LOG still wins — asking for logs should always get
    // you logs, whichever subcommand you asked on.
    let level = cli
        .log_level
        .clone()
        .unwrap_or_else(|| if cli.check { "warn" } else { "info" }.to_string());

    // stdout is the MCP JSON-RPC channel. Every byte of logging goes to stderr,
    // or the protocol is corrupted. This is the single most important line here.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("BOSUN_LOG").unwrap_or_else(|_| EnvFilter::new(&level)),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let engine = EngineClient::connect(cli.socket.as_deref()).await?;

    if cli.check {
        // Diagnostic path for humans: print to stdout and exit without serving,
        // so this can't collide with the MCP channel.
        println!("engine:         {}", engine.engine());
        println!("socket:         {}", engine.endpoint().address);
        println!("resolved from:  {}", engine.endpoint().source.as_str());
        println!("server version: {}", engine.server_version());
        println!("api version:    {}", engine.api_version());
        return Ok(());
    }

    tracing::info!("bosun {} starting on stdio", env!("CARGO_PKG_VERSION"));

    let service = BosunServer::new(engine).serve(stdio()).await?;
    service.waiting().await?;

    tracing::info!("bosun shutting down");
    Ok(())
}
