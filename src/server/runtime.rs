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

impl ServerConfig {
    /// Validate the server configuration, returning the first violation as a
    /// typed [`RGraphError::Argument`].
    ///
    /// # Errors
    ///
    /// Returns [`RGraphError::Argument`] when:
    ///
    /// - `port` is `0` (no ephemeral binding for the server listener);
    /// - `max_connections` is `0`;
    /// - `tls_enabled` is `true` but `tls_cert_path` or `tls_key_path` is
    ///   missing.
    pub fn validate(&self) -> Result<(), RGraphError> {
        if self.port == 0 {
            return Err(RGraphError::Argument(
                "server port must not be 0".into(),
            ));
        }
        if self.max_connections == 0 {
            return Err(RGraphError::Argument(
                "max_connections must be > 0".into(),
            ));
        }
        if self.tls_enabled {
            if self.tls_cert_path.is_none() {
                return Err(RGraphError::Argument(
                    "tls_enabled is true but tls_cert_path is not set".into(),
                ));
            }
            if self.tls_key_path.is_none() {
                return Err(RGraphError::Argument(
                    "tls_enabled is true but tls_key_path is not set".into(),
                ));
            }
        }
        Ok(())
    }
}

impl ServerRuntime {
    /// Build a multi-threaded Tokio runtime and return a [`ServerRuntime`] handle.
    ///
    /// # Errors
    ///
    /// Returns [`RGraphError::Argument`] if the [`ServerConfig`] is invalid
    /// (see [`ServerConfig::validate`]), or [`RGraphError::Internal`] if the
    /// runtime itself cannot be constructed.
    pub fn new(config: ServerConfig) -> Result<Self, RGraphError> {
        config.validate()?;
        let runtime = Builder::new_multi_thread()
            .worker_threads(config.worker_threads)
            .thread_name("rgraph-server")
            .enable_all()
            .build()
            .map_err(|e| RGraphError::Internal(format!("tokio runtime build failed: {e}").into()))?;

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
    ///
    /// If the SIGTERM/SIGINT handlers cannot be installed (resource exhaustion
    /// at process start), this falls back to Ctrl-C only and logs a warning,
    /// rather than panicking on the serving path.
    async fn wait_for_signal() {
        let sigterm = signal::unix::signal(signal::unix::SignalKind::terminate());
        let sigint = signal::unix::signal(signal::unix::SignalKind::interrupt());

        match (sigterm, sigint) {
            (Ok(mut sigterm), Ok(mut sigint)) => {
                tokio::select! {
                    _ = sigterm.recv() => {},
                    _ = sigint.recv() => {},
                }
            }
            (term, int) => {
                if let Err(e) = &term {
                    warn!("could not install SIGTERM handler: {e}");
                }
                if let Err(e) = &int {
                    warn!("could not install SIGINT handler: {e}");
                }
                // Fall back to Ctrl-C so shutdown is still observable.
                let _ = signal::ctrl_c().await;
            }
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

    #[test]
    fn default_server_config_is_valid() {
        assert!(ServerConfig::default().validate().is_ok());
    }

    #[test]
    fn tls_enabled_without_cert_is_rejected() {
        let mut config = ServerConfig::default();
        config.tls_enabled = true;
        config.tls_key_path = Some(std::path::PathBuf::from("/etc/rgraph/key.pem"));
        // cert missing
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("tls_cert_path"), "got: {err}");
        // The same contradiction is rejected by the runtime constructor.
        let mut config = ServerConfig::default();
        config.tls_enabled = true;
        config.tls_key_path = Some(std::path::PathBuf::from("/etc/rgraph/key.pem"));
        assert!(ServerRuntime::new(config).is_err());
    }

    #[test]
    fn zero_port_is_rejected() {
        let mut config = ServerConfig::default();
        config.port = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("port"), "got: {err}");
        assert!(ServerRuntime::new(config).is_err());
    }
}
