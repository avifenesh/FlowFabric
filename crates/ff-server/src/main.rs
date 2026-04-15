use ff_server::config::ServerConfig;
use ff_server::server::Server;

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ff_server=info,ff_engine=info,ff_script=info".into()),
        )
        .init();

    // Load config
    let config = match ServerConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to load config");
            std::process::exit(1);
        }
    };

    // Start server
    let server = match Server::start(config).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to start server");
            std::process::exit(1);
        }
    };

    // Wait for shutdown signal
    wait_for_shutdown_signal().await;

    // Graceful shutdown
    server.shutdown().await;
}

/// Wait for SIGTERM or SIGINT (Ctrl+C).
async fn wait_for_shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");

        tokio::select! {
            _ = ctrl_c => {
                tracing::info!("received SIGINT (Ctrl+C)");
            }
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM");
            }
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.expect("failed to listen for Ctrl+C");
        tracing::info!("received Ctrl+C");
    }
}
