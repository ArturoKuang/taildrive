mod update_cli;

use std::{env, io};

use taildrive_core::update::current_build_info;
use taildrive_server::{App, config::Config};
use tokio::{net::TcpListener, signal};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<_> = env::args_os().collect();
    if matches!(
        update_cli::maybe_run_cli(&args).await?,
        update_cli::CliAction::Handled
    ) {
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let build = current_build_info();
    let config = Config::from_env()?;
    let bind = config.bind;
    let app = App::new(config).await?;
    let listener = TcpListener::bind(bind).await?;
    let shutdown = shutdown_signal()?;
    tracing::info!(
        address = %listener.local_addr()?,
        version = %build.version,
        build_number = build.build_number,
        commit = %build.short_sha,
        "TailDrive is listening"
    );
    app.serve(listener, shutdown).await?;
    Ok(())
}

fn shutdown_signal() -> io::Result<impl Future<Output = ()>> {
    #[cfg(unix)]
    let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())?;
    Ok(async move {
        #[cfg(unix)]
        {
            tokio::select! {
                result = signal::ctrl_c() => if let Err(error) = result {
                    tracing::error!(%error, "could not listen for SIGINT");
                },
                _ = terminate.recv() => {},
            }
        }
        #[cfg(not(unix))]
        if let Err(error) = signal::ctrl_c().await {
            tracing::error!(%error, "could not listen for SIGINT");
        }
    })
}
