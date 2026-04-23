use std::sync::Arc;

use ff_server::admin::{load_probe_inputs, PartitionCollisionsReport};
use ff_server::api;
use ff_server::config::ServerConfig;
use ff_server::server::Server;

#[tokio::main]
async fn main() {
    // Admin subcommands are parsed before tracing init because
    // `partition-collisions` prints a plain-text table to stdout and
    // should not be interleaved with structured tracing output. Other
    // subcommands can opt into tracing individually.
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && args[1] == "admin" {
        run_admin_subcommand(&args[2..]);
        return;
    }

    // Initialize Sentry before tracing so the client is live when the
    // `sentry-tracing` bridge starts forwarding events. No-op when
    // `FF_SENTRY_DSN` is unset; compiles out when the `sentry` feature
    // is off. Guard must live until the process exits to flush buffered
    // events on drop.
    #[cfg(feature = "sentry")]
    let _sentry_guard = ff_observability::init_sentry();

    // `audit=info` is non-negotiable: rotation, security-kid changes,
    // and other compliance events use `tracing::*!(target: "audit",
    // ...)`. Without this directive the default subscriber silently
    // drops every audit event (module-path directives like
    // `ff_server=info` do not match custom targets).
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new(
                "ff_server=info,ff_engine=info,ff_script=info,tower_http=debug,audit=info",
            )
        });

    // Registry-based composition so the Sentry layer can attach
    // alongside `fmt` when the `sentry` feature is on. Without the
    // feature, only `fmt` + `env_filter` are installed — identical
    // to the previous `fmt().with_env_filter().init()` shape.
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer());
    #[cfg(feature = "sentry")]
    let subscriber = subscriber.with(ff_observability::sentry_tracing_layer());
    subscriber.init();

    let config = match ServerConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to load config");
            std::process::exit(1);
        }
    };

    let listen_addr = config.listen_addr.clone();
    let cors_origins = config.cors_origins.clone();
    let api_token = config.api_token.clone();

    // PR-94: build a metrics registry once at startup. The shim is
    // zero-cost when the `observability` feature is off; when on, the
    // same handle is shared between the server (scanners + scheduler
    // call sites), the HTTP middleware, and the `/metrics` route so a
    // scrape reflects everything the process produces.
    let metrics = Arc::new(ff_server::Metrics::new());

    let server = match Server::start_with_metrics(config, metrics.clone()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to start server");
            std::process::exit(1);
        }
    };

    let server = Arc::new(server);

    let app = match api::router_with_metrics(
        server.clone(),
        &cors_origins,
        api_token,
        Some(metrics),
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "failed to build HTTP router");
            std::process::exit(1);
        }
    };

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

/// Dispatch an `admin` subcommand. Reads only the minimal env subset each
/// probe needs (via `ff_server::admin::load_probe_inputs` and similar);
/// does NOT call `ServerConfig::from_env()` and does NOT connect to Valkey
/// or start any long-lived task — probes are pure computations over the
/// configured state.
///
/// Exit codes:
/// - `0` — probe succeeded
/// - `2` — unknown subcommand or invalid config
fn run_admin_subcommand(args: &[String]) {
    let subcommand = args.first().map(String::as_str).unwrap_or("");
    match subcommand {
        "partition-collisions" => {
            // Use the probe-specific loader — the collisions probe is a
            // pure computation over lanes + partition_config and does not
            // need the prod-boot requirements (HMAC secret, CORS, etc.).
            let (lanes, partition_config) = match load_probe_inputs() {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("ff-server admin partition-collisions: {e}");
                    std::process::exit(2);
                }
            };
            let report =
                PartitionCollisionsReport::compute(&lanes, &partition_config);
            print!("{}", report.format_plain());
        }
        "" => {
            eprintln!(
                "ff-server admin: no subcommand given\n\
                 \n\
                 USAGE:\n    \
                 ff-server admin <subcommand>\n\
                 \n\
                 SUBCOMMANDS:\n    \
                 partition-collisions    Report RFC-011 §5.6 partition collisions across configured lanes\n"
            );
            std::process::exit(2);
        }
        other => {
            eprintln!(
                "ff-server admin: unknown subcommand '{other}'\n\
                 \n\
                 Available subcommands: partition-collisions\n"
            );
            std::process::exit(2);
        }
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
