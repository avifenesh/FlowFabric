use std::sync::Arc;

use ff_server::api;
use ff_server::config::ServerConfig;
use ff_server::server::Server;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| {
                    "ff_server=info,ff_engine=info,ff_script=info,tower_http=debug".into()
                }),
        )
        .init();

    let config = match ServerConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to load config");
            std::process::exit(1);
        }
    };

    let listen_addr = config.listen_addr.clone();

    let server = match Server::start(config).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to start server");
            std::process::exit(1);
        }
    };

    let server = Arc::new(server);
    let app = api::router(server.clone());

    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(addr = %listen_addr, error = %e, "failed to bind listener");
            std::process::exit(1);
        });

    tracing::info!(addr = %listen_addr, "HTTP API listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "HTTP server error");
        });

    // Graceful engine shutdown after HTTP server stops
    match Arc::try_unwrap(server) {
        Ok(s) => s.shutdown().await,
        Err(_) => tracing::warn!("could not take exclusive server ownership for shutdown"),
    }
}

async fn shutdown_signal() {
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
