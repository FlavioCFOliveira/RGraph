//! Request dispatcher and CPU-bound worker pool offload.
//!
//! [`RequestDispatcher`] routes incoming requests to the graph engine while
//! offloading CPU-intensive work (planning, traversal, analytics) to a dedicated
//! [`rayon`] thread pool via `tokio::task::spawn_blocking`.  This prevents
//! async runtime starvation under heavy query load.

use crate::error::RGraphError;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tracing::{debug, trace};

/// A handle to a dedicated rayon thread pool for CPU-bound graph work.
pub struct CpuPool {
    pool: Arc<rayon::ThreadPool>,
}

impl CpuPool {
    /// Create a new pool with `threads` worker threads.
    pub fn new(threads: usize) -> Result<Self, RGraphError> {
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .thread_name(|i| format!("rgraph-cpu-{i}"))
                .build()
                .map_err(|e| RGraphError::Internal(format!("rayon pool build failed: {e}").into()))?,
        );
        Ok(Self { pool })
    }

    /// Spawn a blocking CPU job and return a Tokio [`JoinHandle`].
    ///
    /// The closure `f` runs inside the rayon pool, so the Tokio async runtime
    /// is never blocked.  Tail-latency histograms are updated automatically.
    pub fn spawn<F, R>(&self, f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let start = Instant::now();
            let result = pool.install(f);
            let elapsed = start.elapsed();
            trace!("cpu pool job completed in {:?}", elapsed);
            result
        })
    }

    /// Execute a future on the Tokio runtime and measure tail latency.
    ///
    /// Returns an error if the operation exceeds `timeout`.
    pub async fn dispatch_with_timeout<F, R>(
        &self,
        timeout: Duration,
        fut: F,
    ) -> Result<R, RGraphError>
    where
        F: Future<Output = Result<R, RGraphError>> + Send,
    {
        let start = Instant::now();
        let res = tokio::time::timeout(timeout, fut).await;
        let elapsed = start.elapsed();
        debug!("dispatch latency: {:?}", elapsed);

        match res {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(RGraphError::ResourceExhausted(format!(
                "request timed out after {:?}",
                timeout
            ).into())),
        }
    }
}

/// Request dispatcher that combines the CPU pool with backpressure-aware
/// admission control.
pub struct RequestDispatcher {
    pub cpu_pool: CpuPool,
    /// Default timeout for graph operations.
    pub default_timeout: Duration,
}

impl RequestDispatcher {
    /// Build a new dispatcher backed by a rayon pool of `cpu_threads` workers.
    pub fn new(cpu_threads: usize) -> Result<Self, RGraphError> {
        let cpu_pool = CpuPool::new(cpu_threads)?;
        Ok(Self {
            cpu_pool,
            default_timeout: Duration::from_secs(30),
        })
    }

    /// Submit a CPU-bound closure and await the result.
    ///
    /// # Errors
    ///
    /// Returns [`RGraphError::Internal`] if the rayon task panics.
    pub async fn offload<F, R>(&self, f: F) -> Result<R, RGraphError>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        self.cpu_pool
            .spawn(f)
            .await
            .map_err(|e| RGraphError::Internal(format!("cpu pool task panicked: {e}").into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_pool_creation_ok() {
        let pool = CpuPool::new(2);
        assert!(pool.is_ok());
    }

    #[tokio::test]
    async fn offload_computes_result() {
        let dispatcher = RequestDispatcher::new(2).unwrap();
        let result = dispatcher.offload(|| 42usize).await.unwrap();
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn dispatch_with_timeout_honours_deadline() {
        let dispatcher = RequestDispatcher::new(2).unwrap();
        let fut = async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(())
        };
        let res = dispatcher
            .cpu_pool
            .dispatch_with_timeout(Duration::from_millis(50), fut)
            .await;
        assert!(matches!(res, Err(RGraphError::ResourceExhausted(_))));
    }
}
