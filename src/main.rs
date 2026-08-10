use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use clap::Parser;
use quotamux::{AppState, Config, build_app};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Path to the private TOML configuration file.
    #[arg(long, default_value = "quotamux.toml", env = "QUOTAMUX_CONFIG")]
    config: PathBuf,

    /// Validate configuration and provider model identities, then exit.
    #[arg(long)]
    check: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "quotamux=info".into()),
        )
        .json()
        .with_current_span(false)
        .init();

    let args = Args::parse();
    let config = Config::load(&args.config)?;
    if args.check {
        println!("configuration valid");
        return Ok(());
    }

    let listen: SocketAddr = config.server.listen.parse()?;
    let state = Arc::new(AppState::new(config).await?);
    state.start_background();
    let app = build_app(state);
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
