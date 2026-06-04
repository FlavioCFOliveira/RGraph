//! Connection acceptor and protocol multiplexer.
//!
//! [`ConnectionAcceptor`] listens on a TCP socket (optionally with TLS) and
//! yields [`Protocol`] variants so that a future Bolt compatibility layer can
//! be added without architectural changes.

use crate::error::RGraphError;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig as RustlsServerConfig;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

/// Protocol detected on an incoming connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// gRPC (HTTP/2 with TLS or plaintext).
    Grpc,
    /// Bolt (Neo4j wire protocol) — placeholder for future sprint.
    Bolt,
    /// Unrecognised or unsupported protocol.
    Unknown,
}

/// Multiplexes incoming TCP connections by inspecting the first few bytes.
///
/// The design is intentionally extensible: adding Bolt support later only
/// requires adding a new detection branch in [`ProtocolMultiplexer::detect`].
pub struct ProtocolMultiplexer;

impl ProtocolMultiplexer {
    /// Peek at the first 6 bytes of `stream` and infer the protocol.
    ///
    /// * HTTP/2 preface (`PRI *`) → [`Protocol::Grpc`]
    /// * Bolt handshake (`0x60 0x60 0xB0 0x17`) → [`Protocol::Bolt`]
    /// * Anything else → [`Protocol::Unknown`]
    pub async fn detect(stream: &mut TcpStream) -> Result<Protocol, RGraphError> {
        let mut buf = [0u8; 6];
        match stream.peek(&mut buf).await {
            Ok(0) => return Ok(Protocol::Unknown), // closed immediately
            Ok(n) if n >= 4 => {
                // HTTP/2 connection preface starts with "PRI *".
                if &buf[..4] == b"PRI " {
                    return Ok(Protocol::Grpc);
                }
                // Bolt v4+ handshake magic bytes.
                if n >= 4 && buf[..4] == [0x60, 0x60, 0xB0, 0x17] {
                    return Ok(Protocol::Bolt);
                }
                // HTTP/1.x or plaintext gRPC might start with "POST ", "GET ", etc.
                if buf[0].is_ascii_uppercase() {
                    return Ok(Protocol::Grpc);
                }
                Ok(Protocol::Unknown)
            }
            Ok(_) => Ok(Protocol::Unknown),
            Err(e) => Err(RGraphError::Io(format!("peek failed: {e}").into())),
        }
    }
}

/// A connection stream that is either plaintext TCP or TLS-wrapped TCP.
///
/// Holds an [`OwnedSemaphorePermit`] so that the connection slot is released
/// only when the stream is dropped.
pub struct ServerStream {
    inner: ServerStreamInner,
    local_addr: Option<std::net::SocketAddr>,
    peer_addr: Option<std::net::SocketAddr>,
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

enum ServerStreamInner {
    Plain(TcpStream),
    Tls(TlsStream<TcpStream>),
}

impl ServerStream {
    fn new(
        inner: ServerStreamInner,
        local_addr: Option<std::net::SocketAddr>,
        peer_addr: Option<std::net::SocketAddr>,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Self {
        Self {
            inner,
            local_addr,
            peer_addr,
            _permit: Some(permit),
        }
    }

    /// The local address this connection was accepted on, if known.
    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.local_addr
    }

    /// The remote peer address of this connection, if known.
    pub fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.peer_addr
    }
}

// tonic requires the incoming IO type to implement `Connected` so it can
// surface connection metadata (peer/local address) via request extensions.
impl tonic::transport::server::Connected for ServerStream {
    type ConnectInfo = tonic::transport::server::TcpConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        tonic::transport::server::TcpConnectInfo {
            local_addr: self.local_addr,
            remote_addr: self.peer_addr,
        }
    }
}

impl AsyncRead for ServerStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut self.get_mut().inner {
            ServerStreamInner::Plain(s) => Pin::new(s).poll_read(cx, buf),
            ServerStreamInner::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ServerStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match &mut self.get_mut().inner {
            ServerStreamInner::Plain(s) => Pin::new(s).poll_write(cx, buf),
            ServerStreamInner::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match &mut self.get_mut().inner {
            ServerStreamInner::Plain(s) => Pin::new(s).poll_flush(cx),
            ServerStreamInner::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        match &mut self.get_mut().inner {
            ServerStreamInner::Plain(s) => Pin::new(s).poll_shutdown(cx),
            ServerStreamInner::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// A [`ServerStream`] wrapper that records connection-lifecycle metrics.
///
/// The active-connection gauge is incremented when the underlying stream is
/// accepted (see [`ConnectionAcceptor::into_incoming`]) and decremented when
/// this wrapper is dropped, i.e. when tonic finishes serving the connection.
pub struct MeteredStream {
    inner: ServerStream,
    metrics: Arc<crate::server::metrics::MetricsCollector>,
}

impl MeteredStream {
    fn new(inner: ServerStream, metrics: Arc<crate::server::metrics::MetricsCollector>) -> Self {
        Self { inner, metrics }
    }
}

impl Drop for MeteredStream {
    fn drop(&mut self) {
        self.metrics.observe_connection_closed();
    }
}

impl AsyncRead for MeteredStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for MeteredStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl tonic::transport::server::Connected for MeteredStream {
    type ConnectInfo = tonic::transport::server::TcpConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.inner.connect_info()
    }
}

/// TCP/TLS listener that accepts connections and enforces a global connection
/// limit via a [`Semaphore`].
pub struct ConnectionAcceptor {
    listener: TcpListener,
    tls_acceptor: Option<TlsAcceptor>,
    /// Semaphore controlling how many concurrent connections are allowed.
    connection_limit: Arc<Semaphore>,
    /// The port we are actually bound to (useful when binding to :0).
    pub local_port: u16,
}

impl ConnectionAcceptor {
    /// Bind to `addr` (e.g. "0.0.0.0:7687") and prepare for accepting.
    ///
    /// If `tls_cert_path` and `tls_key_path` are both provided, TLS is enabled.
    pub async fn bind(
        addr: &str,
        tls_cert_path: Option<PathBuf>,
        tls_key_path: Option<PathBuf>,
        max_connections: usize,
    ) -> Result<Self, RGraphError> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| RGraphError::Io(format!("bind failed: {e}").into()))?;
        let local_port = listener
            .local_addr()
            .map_err(|e| RGraphError::Io(format!("local_addr failed: {e}").into()))?
            .port();

        let tls_acceptor = match (tls_cert_path, tls_key_path) {
            (Some(cert), Some(key)) => {
                info!("loading TLS certificate from {:?} and key from {:?}", cert, key);
                let config = Self::load_tls_config(&cert, &key)?;
                Some(TlsAcceptor::from(Arc::new(config)))
            }
            (None, None) => {
                info!("TLS disabled; accepting plaintext connections");
                None
            }
            _ => {
                return Err(RGraphError::Argument(
                    "both tls-cert and tls-key must be provided, or neither".into(),
                ));
            }
        };

        Ok(Self {
            listener,
            tls_acceptor,
            connection_limit: Arc::new(Semaphore::new(max_connections)),
            local_port,
        })
    }

    /// Accept the next connection, respecting the global connection limit.
    ///
    /// Returns `None` when the semaphore has been closed (shutdown signal).
    pub async fn accept(
        &self,
    ) -> Result<Option<(Protocol, ServerStream)>, RGraphError> {
        // Acquire a connection slot; if the semaphore is closed, shutdown is in progress.
        let permit = match self.connection_limit.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };

        let (mut stream, peer) = match self.listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                warn!("accept error: {}", e);
                return Ok(None);
            }
        };

        let local_addr = stream.local_addr().ok();
        let peer_addr = Some(peer);

        // Detect protocol before TLS handshake so we can route correctly.
        let protocol = ProtocolMultiplexer::detect(&mut stream).await?;

        // Perform TLS handshake if configured.
        let inner = if let Some(ref acceptor) = self.tls_acceptor {
            match acceptor.accept(stream).await {
                Ok(tls_stream) => ServerStreamInner::Tls(tls_stream),
                Err(e) => {
                    warn!("TLS handshake failed for {}: {}", peer, e);
                    return Ok(None);
                }
            }
        } else {
            ServerStreamInner::Plain(stream)
        };

        // The permit is moved into the stream so the connection slot is held
        // for the lifetime of the stream and released on drop.
        let stream = ServerStream::new(inner, local_addr, peer_addr, permit);
        Ok(Some((protocol, stream)))
    }

    /// Close the acceptor so that no new connections are accepted.
    pub fn close(&self) {
        self.connection_limit.close();
    }

    /// Number of connection slots currently available.
    ///
    /// Exposed primarily for tests asserting backpressure behaviour.
    pub fn available_permits(&self) -> usize {
        self.connection_limit.available_permits()
    }

    /// Convert the acceptor into a [`Stream`](futures::Stream) of accepted,
    /// gRPC-protocol connections suitable for `tonic::Server::serve_with_incoming`.
    ///
    /// The stream:
    /// * enforces the global connection limit (via the acceptor's semaphore);
    /// * performs the TLS handshake when configured;
    /// * filters out non-gRPC (e.g. Bolt) and failed connections — these are
    ///   skipped, not surfaced as stream errors, so a single bad client cannot
    ///   tear down the listener;
    /// * records connection-open/close metrics against `metrics`.
    ///
    /// The stream ends (`None`) once [`close`](ConnectionAcceptor::close) has
    /// been called and the semaphore is drained, which is how graceful
    /// shutdown stops the acceptance loop.
    pub fn into_incoming(
        self,
        metrics: Arc<crate::server::metrics::MetricsCollector>,
    ) -> impl futures::Stream<Item = Result<MeteredStream, io::Error>> {
        futures::stream::unfold((self, metrics), |(acceptor, metrics)| async move {
            loop {
                match acceptor.accept().await {
                    // Only gRPC connections are routed to tonic; other protocols
                    // are dropped (the permit is released on stream drop).
                    Ok(Some((Protocol::Grpc, stream))) => {
                        metrics.observe_connection_opened();
                        let metered = MeteredStream::new(stream, metrics.clone());
                        return Some((Ok(metered), (acceptor, metrics)));
                    }
                    // Non-gRPC or skipped connection: keep looping.
                    Ok(Some(_)) => continue,
                    // Acceptor closed (shutdown) — terminate the stream.
                    Ok(None) => return None,
                    // Transient error (e.g. TLS detect): surface it but keep the
                    // listener alive; tonic logs and continues.
                    Err(e) => {
                        warn!("connection acceptance error: {e}");
                        continue;
                    }
                }
            }
        })
    }

    fn load_tls_config(
        cert_path: &PathBuf,
        key_path: &PathBuf,
    ) -> Result<RustlsServerConfig, RGraphError> {
        let cert_file = std::fs::read(cert_path)
            .map_err(|e| RGraphError::Io(format!("cannot read cert: {e}").into()))?;
        let key_file = std::fs::read(key_path)
            .map_err(|e| RGraphError::Io(format!("cannot read key: {e}").into()))?;

        let certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut cert_file.as_slice())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| RGraphError::Io(format!("invalid cert PEM: {e}").into()))?;

        let keys: Vec<PrivateKeyDer<'static>> =
            rustls_pemfile::private_key(&mut key_file.as_slice())
                .map_err(|e| RGraphError::Io(format!("invalid key PEM: {e}").into()))?
                .into_iter()
                .collect();

        if certs.is_empty() {
            return Err(RGraphError::Argument("no certificates found in PEM".into()));
        }
        let key = keys
            .into_iter()
            .next()
            .ok_or_else(|| RGraphError::Argument("no private key found in PEM".into()))?;

        let mut config = RustlsServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| RGraphError::Io(format!("TLS config error: {e}").into()))?;

        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn acceptor_binds_to_random_port() {
        let acceptor = ConnectionAcceptor::bind("127.0.0.1:0", None, None, 10)
            .await
            .unwrap();
        assert!(acceptor.local_port > 0);
    }

    #[tokio::test]
    async fn protocol_detects_http2_preface() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 6];
            let _ = stream.read(&mut buf).await;
            let _ = stream.write_all(&buf).await;
        });

        let mut client = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
        client.write_all(b"PRI * HTTP/2.0").await.unwrap();

        let mut buf = [0u8; 6];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"PRI * ");
    }

    #[tokio::test]
    async fn connection_limit_enforced() {
        let acceptor = ConnectionAcceptor::bind("127.0.0.1:0", None, None, 1)
            .await
            .unwrap();
        let port = acceptor.local_port;

        // Spawn a client so the first accept can complete.
        tokio::spawn(async move {
            let _ = TcpStream::connect(format!("127.0.0.1:{port}")).await;
        });

        // Accept the first connection and hold the only slot.
        let _first = acceptor.accept().await.unwrap();

        // The semaphore should now have zero available permits.
        assert_eq!(acceptor.connection_limit.available_permits(), 0);

        // Spawn a second client; the acceptor should not be able to acquire.
        tokio::spawn(async move {
            let _ = TcpStream::connect(format!("127.0.0.1:{port}")).await;
        });

        // Second accept should time out because the semaphore is exhausted.
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            acceptor.accept(),
        )
        .await;
        assert!(
            result.is_err(),
            "second accept should block on exhausted semaphore"
        );
    }
}
