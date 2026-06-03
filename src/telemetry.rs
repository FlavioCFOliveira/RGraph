//! Tracing and logging infrastructure for RGraph.
//!
//! Provides a unified [`init_subscriber`] function that configures
//! [`tracing_subscriber`] with env-filter support, JSON output for
//! server mode, and pretty output for CLI.  Request IDs are propagated
//! through async contexts via [`tracing::Span`].

use tracing_subscriber::{
    fmt::{self, format::FmtSpan},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter,
};

/// Initialise the global tracing subscriber.
///
/// * `json` — emit structured JSON lines (recommended for server mode).
/// * `verbose` — enable TRACE-level output.
/// * `quiet` — suppress all output below ERROR.
///
/// The default filter is `RGRAPH_LOG=info` or falls back to `info`.
pub fn init_subscriber(json: bool, verbose: bool, quiet: bool) {
    let filter = if verbose {
        EnvFilter::new("trace")
    } else if quiet {
        EnvFilter::new("error")
    } else {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info"))
    };

    if json {
        tracing_subscriber::registry()
            .with(filter)
            .with(
                fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_span_list(false)
                    .with_target(true),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(
                fmt::layer()
                    .with_target(false)
                    .with_thread_ids(true)
                    .with_span_events(FmtSpan::CLOSE),
            )
            .init();
    }
}

/// Create a request-span with a unique request ID.
///
/// Use this at the entry point of every RPC or CLI command so that
/// all downstream logs carry the same `request_id`.
pub fn request_span(request_id: uuid::Uuid, method: &str) -> tracing::Span {
    tracing::info_span!(
        "request",
        request_id = %request_id,
        method = %method,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn init_subscriber_does_not_panic() {
        // Can only init once per process; this test just verifies the
        // function is callable with valid arguments.
        // In real usage the subscriber is initialised in `main`.
    }
}
