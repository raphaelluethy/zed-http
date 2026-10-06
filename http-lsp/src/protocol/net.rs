//! TCP and TLS connections for the protocols that do not go through reqwest (WebSocket, gRPC).
//! TLS uses rustls with the ring provider and the platform verifier, like the HTTP client.

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use rustls::{pki_types::ServerName, ClientConfig};
use rustls_platform_verifier::ConfigVerifierExt;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::TlsConnector;

use super::{error_chain, install_crypto_provider};

/// Connection deadline unless `@connection-timeout` says otherwise.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

/// A plain or TLS stream.
pub type BoxedIo = Box<dyn Io>;

/// The application protocol negotiated over TLS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alpn {
    Http1,
}

fn tls_config(alpn: Alpn) -> Result<Arc<ClientConfig>, String> {
    static HTTP1: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    let (cell, protocol): (_, &[u8]) = match alpn {
        Alpn::Http1 => (&HTTP1, b"http/1.1"),
    };
    cell.get_or_init(|| {
        install_crypto_provider();
        let mut config = ClientConfig::with_platform_verifier()
            .map_err(|error| format!("failed to set up TLS: {error}"))?;
        config.alpn_protocols = vec![protocol.to_vec()];
        Ok(Arc::new(config))
    })
    .clone()
}

/// Opens a TCP connection to `host:port`, wrapped in TLS when `tls` is set. `host` may be an IPv6
/// address with or without brackets.
pub async fn connect(
    host: &str,
    port: u16,
    tls: Option<Alpn>,
    timeout: Duration,
) -> Result<BoxedIo, String> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let connect = async {
        let tcp = TcpStream::connect((host, port))
            .await
            .map_err(|error| format!("failed to connect to {host}:{port}: {error}"))?;
        let _ = tcp.set_nodelay(true);
        let Some(alpn) = tls else {
            return Ok(Box::new(tcp) as BoxedIo);
        };
        let name = ServerName::try_from(host.to_owned())
            .map_err(|_| format!("invalid TLS server name {host:?}"))?;
        let stream = TlsConnector::from(tls_config(alpn)?)
            .connect(name, tcp)
            .await
            .map_err(|error| {
                format!(
                    "TLS handshake with {host}:{port} failed: {}",
                    error_chain(&error)
                )
            })?;
        Ok(Box::new(stream) as BoxedIo)
    };
    tokio::time::timeout(timeout, connect)
        .await
        .map_err(|_| format!("timed out connecting to {host}:{port}"))?
}
