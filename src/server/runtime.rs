//! Tokio runtime bootstrap and graceful shutdown.
//!
//! [`ServerRuntime`] owns the [`tokio::runtime::Runtime`] and a [`tokio::sync::broadcast`]
//! channel used to signal all worker tasks when a graceful shutdown is requested.

use crate::error::RGraphError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::{Builder, Runtime};
use tokio::signal;
use tokio::sync::broadcast;
use tokio::time::{interval, Instant};
use tracing::{info, warn};

/// Configuration for the Tokio runtime and server lifecycle.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Number of worker threads for the multi-threaded runtime.
    pub worker_threads: usize,
    /// Max time to wait for in-flight work during shutdown.
    pub shutdown_timeout_secs: u64,
    /// Host to bind the server listener to.
    pub host: String,
    /// Port to bind the server listener to.
    pub port: u16,
    /// Whether to enable TLS.
    pub tls_enabled: bool,
    /// Path to TLS certificate (PEM).
    pub tls_cert_path: Option<std::path::PathBuf>,
    /// Path to TLS private key (PEM).
    pub tls_key_path: Option<std::path::PathBuf>,
    /// Maximum concurrent connections.
    pub max_connections: usize,
    /// Size of the CPU-bound rayon pool.
    pub cpu_pool_threads: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            worker_threads: num_cpus::get().max(2),
            shutdown_timeout_secs: 30,
            host: "0.0.0.0".into(),
            port: 7687,
            tls_enabled: false,
            tls_cert_path: None,
            tls_key_path: None,
            max_connections: 1024,
            cpu_pool_threads: num_cpus::get().max(2),
        }
    }
}

/// Owner of the Tokio runtime and shutdown signalling infrastructure.
pub struct ServerRuntime {
    pub config: ServerConfig,
    pub runtime: Runtime,
    /// Broadcast channel used to notify all tasks of an impending shutdown.
    pub shutdown_tx: broadcast::Sender<()>,
    /// Flag that becomes `true` once the runtime has entered shutdown.
    pub shutting_down: Arc<AtomicBool>,
}

impl ServerRuntime {
    /// Build a multi-threaded Tokio runtime and return a [`ServerRuntime`] handle.
    ///
    /// # Errors
    ///
    /// Returns [`RGraphError::Internal`] if the runtime cannot be constructed.
    pub fn new(config: ServerConfig) -> Result<Self, RGraphError> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(config.worker_threads)
            .thread_name("rgraph-server")
            .enable_all()
            .build()
            .map_err(|e| RGraphError::Internal(format!("tokio runtime build failed: {e}")))?;

        let (shutdown_tx, _rx) = broadcast::channel(1);
        let shutting_down = Arc::new(AtomicBool::new(false));

        Ok(Self {
            config,
            runtime,
            shutdown_tx,
            shutting_down,
        })
    }

    /// Block the current thread and run the async `main_future` to completion.
    ///
    /// When `SIGTERM` or `SIGINT` is received, the shutdown sequence begins:
    /// 1. `shutting_down` flag is set.
    /// 2. The broadcast channel fires so every listener can stop accepting new work.
    /// 3. A bounded timeout waits for in-flight transactions to finish.
    /// 4. The runtime exits.
    pub fn block_on<F>(&self,
        main_future: F,
        in_flight_count: Arc<std::sync::atomic::AtomicUsize>,
    ) where
        F: std::future::Future<Output = Result<(), RGraphError>> + Send + 'static,
    {
        let shutdown_tx = self.shutdown_tx.clone();
        let shutting_down = self.shutting_down.clone();
        let timeout = Duration::from_secs(self.config.shutdown_timeout_secs);

        let _ = self.runtime.block_on(async {
            tokio::select! {
                res = main_future => {
                    if let Err(ref e) = res {
                        warn!("server main future returned error: {}", e);
                    }
                    res
                }
                _ = Self::wait_for_signal() => {
                    info!("shutdown signal received; initiating graceful shutdown");
                    shutting_down.store(true, Ordering::SeqCst);
                    let _ = shutdown_tx.send(());

                    // Drain in-flight work with a deadline.
                    let deadline = Instant::now() + timeout;
                    let mut ticker = interval(Duration::from_millis(100));
                    loop {
                        ticker.tick().await;
                        let remaining = in_flight_count.load(Ordering::Relaxed);
                        if remaining == 0 {
                            info!("all in-flight work drained; exiting cleanly");
                            break;
                        }
                        if Instant::now() >= deadline {
                            warn!(
                                "shutdown timeout reached with {} in-flight items; forcing exit",
                                remaining
                            );
                            break;
                        }
                    }
                    Ok(())
                }
            }
        });
    }

    /// Register a Tokio signal handler that resolves on SIGTERM or SIGINT.
    async fn wait_for_signal() {
        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())
            .expect("install SIGINT handler");

        tokio::select! {
            _ = sigterm.recv() => {},
            _ = sigint.recv() => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_builder_succeeds() {
        let mut config = ServerConfig::default();
        config.worker_threads = 2;
        let rt = ServerRuntime::new(config);
        assert!(rt.is_ok());
    }

    #[test]
    fn shutdown_flag_defaults_to_false() {
        let config = ServerConfig::default();
        let rt = ServerRuntime::new(config).unwrap();
        assert!(!rt.shutting_down.load(Ordering::Relaxed));
        // Explicitly drop the runtime outside an async context.
        drop(rt);
    }
}
