#[allow(dead_code)]
mod circuit_breaker;
mod config;
mod metrics;
mod proxy;
mod routing;
mod server;
#[allow(dead_code)]
mod tree;
mod worker;

use crate::circuit_breaker::CircuitBreakerConfig;
use crate::config::{AppConfig, CliArgs};
use crate::routing::CacheRouter;
use crate::server::AppState;
use crate::worker::Worker;
use arc_swap::ArcSwap;
use clap::Parser;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};

#[tokio::main]
async fn main() {
    let cli = CliArgs::parse();
    let config = AppConfig::load(&cli.config).unwrap_or_else(|e| {
        eprintln!("Configuration error: {}", e);
        std::process::exit(1);
    });

    if cli.config_check {
        println!("Configuration is valid.");
        std::process::exit(0);
    }

    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.logging.level)),
        )
        .init();

    info!("Starting cache-aware router v{}", env!("CARGO_PKG_VERSION"));
    info!("Current config:\n{}", config.effective_dump());

    // Create workers
    let health_config = config.health_config();
    let cb_config = CircuitBreakerConfig {
        failure_threshold: config.circuit_breaker.failure_threshold,
        success_threshold: config.circuit_breaker.success_threshold,
        timeout_duration: Duration::from_secs(config.circuit_breaker.timeout_secs),
        ..Default::default()
    };

    let workers: Arc<[Arc<Worker>]> = config
        .workers
        .iter()
        .map(|entry| {
            Arc::new(Worker::new(
                entry.url.clone(),
                entry.max_load,
                entry.load_penalty,
                health_config.clone(),
                cb_config.clone(),
            ))
        })
        .collect();

    info!("Created {} workers", workers.len());

    // Spawn health checker with stop flag
    let health_stop = Arc::new(AtomicBool::new(false));
    let health_workers = Arc::clone(&workers);
    let health_stop_clone = Arc::clone(&health_stop);
    tokio::spawn(async move {
        worker::run_health_checker(health_workers, health_stop_clone).await;
    });

    // Create cache-aware router (swappable via ArcSwap)
    let cache_router = Arc::new(ArcSwap::from_pointee(CacheRouter::new(
        Arc::clone(&workers),
        config.cache_config(),
    )));

    // Build HTTP client for proxying
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(32)
        .pool_idle_timeout(Duration::from_secs(90))
        .connect_timeout(Duration::from_secs(config.proxy.connect_timeout_secs))
        .build()
        .expect("Failed to create HTTP client");

    // Build app state
    let state = AppState {
        client,
        router: Arc::clone(&cache_router),
        proxy_config: config.proxy_config(),
    };

    // Spawn SIGHUP reload handler
    let config_path = cli.config.clone();
    let current_config = Arc::new(parking_lot::Mutex::new(config));
    spawn_reload_handler(
        config_path,
        Arc::clone(&cache_router),
        Arc::clone(&current_config),
        health_stop,
    );

    let app = server::build_app(state);

    // Bind and serve
    let addr = {
        let cfg = current_config.lock();
        format!("{}:{}", cfg.server.host, cfg.server.port)
    };
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind address");

    info!("Listening on {}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("Server error");

    info!("Server shut down gracefully");
}

fn spawn_reload_handler(
    config_path: Vec<PathBuf>,
    router: Arc<ArcSwap<CacheRouter>>,
    current_config: Arc<parking_lot::Mutex<AppConfig>>,
    health_stop: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        let mut sig = signal(SignalKind::hangup()).expect("Failed to install SIGHUP handler");
        let mut health_stop = health_stop;

        loop {
            sig.recv().await;
            info!("Received SIGHUP, reloading config from {:?}", config_path);

            let new_config = match AppConfig::load(&config_path) {
                Ok(c) => c,
                Err(e) => {
                    warn!("Config reload failed (parse error): {}", e);
                    continue;
                }
            };

            // Validate only workers changed
            {
                let current = current_config.lock();
                if let Err(e) = current.validate_reload_compatibility(&new_config) {
                    warn!("{}", e);
                    continue;
                }
            }

            let old_workers = current_config.lock().workers.clone();
            info!(
                "Config reload: workers changing from {:?} to {:?}",
                old_workers, new_config.workers
            );

            // Build new workers
            let health_config = new_config.health_config();
            let cb_config = CircuitBreakerConfig {
                failure_threshold: new_config.circuit_breaker.failure_threshold,
                success_threshold: new_config.circuit_breaker.success_threshold,
                timeout_duration: Duration::from_secs(new_config.circuit_breaker.timeout_secs),
                ..Default::default()
            };

            let new_workers: Arc<[Arc<Worker>]> = new_config
                .workers
                .iter()
                .map(|entry| {
                    Arc::new(Worker::new(
                        entry.url.clone(),
                        entry.max_load,
                        entry.load_penalty,
                        health_config.clone(),
                        cb_config.clone(),
                    ))
                })
                .collect();

            // Stop old health checker
            health_stop.store(true, Ordering::Relaxed);

            // Start new health checker
            let new_stop = Arc::new(AtomicBool::new(false));
            let hc_workers = Arc::clone(&new_workers);
            let hc_stop = Arc::clone(&new_stop);
            tokio::spawn(async move {
                worker::run_health_checker(hc_workers, hc_stop).await;
            });
            health_stop = new_stop;

            // Build new router and swap atomically
            let new_router = CacheRouter::new(new_workers, new_config.cache_config());
            router.store(Arc::new(new_router));

            // Update stored config
            *current_config.lock() = new_config;

            info!("Config reloaded successfully");
            info!(
                "Current config:\n{}",
                current_config.lock().effective_dump()
            );
        }
    });
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received Ctrl+C, shutting down..."),
        _ = terminate => info!("Received SIGTERM, shutting down..."),
    }
}
