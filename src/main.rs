use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;
use quotamux::{Config, workspace::Workspace};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Path to the private TOML configuration file.
    #[arg(long, default_value = "quotamux.toml", env = "QUOTAMUX_CONFIG")]
    config: PathBuf,

    /// Validate configuration, backend identities, and adapter model contracts, then exit.
    #[arg(long, conflicts_with = "diagnose")]
    check: bool,

    /// 执行独立诊断并写入脱敏报告，不启动网关。
    #[arg(long)]
    diagnose: bool,

    /// 诊断报告 JSON 路径（不覆盖现有文件）。
    #[arg(long, requires = "diagnose")]
    report: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error) => {
            let code = if error.use_stderr() { 1 } else { 0 };
            let _ = error.print();
            std::process::exit(code);
        }
    };
    if args.diagnose {
        std::process::exit(quotamux::diagnostics::run(&args.config, args.report.as_deref()).await);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "quotamux=info".into()),
        )
        .json()
        .with_current_span(false)
        .init();

    let config = Config::load(&args.config)?;
    if args.check {
        println!("configuration valid");
        return Ok(());
    }

    let listen: SocketAddr = config.server.listen.parse()?;
    let workspace = Workspace::open(&args.config).await?;
    let app = workspace.router();
    let listener = TcpListener::bind(listen).await?;
    info!(%listen, "QuotaMux listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
