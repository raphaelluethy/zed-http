//! Protocol dispatch. Each protocol module exposes `send(&PreparedRequest, &Context)` and
//! returns a [`Response`]; the runner handles variables, scripts and the report around it.

pub mod grpc;
pub mod http;
mod net;
pub mod websocket;

use std::{
    path::{Path, PathBuf},
    sync::Once,
    time::Duration,
};

use crate::{
    session::Session,
    syntax::{Directives, Header},
};

/// Overall deadline for a request unless `@timeout` asks for longer.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Response bodies (after decompression) larger than this are truncated and reported as errors.
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Http,
    GraphQl,
    WebSocket,
    Grpc,
}

impl Protocol {
    pub fn of(method: &str) -> Self {
        match method {
            "GRAPHQL" => Self::GraphQl,
            "WEBSOCKET" => Self::WebSocket,
            "GRPC" => Self::Grpc,
            _ => Self::Http,
        }
    }
}

/// A request with every variable substituted and every body part resolved.
#[derive(Clone, Debug)]
pub struct PreparedRequest {
    pub method: String,
    pub url: String,
    pub http_version: Option<String>,
    pub headers: Vec<Header>,
    pub body: PreparedBody,
    pub directives: Directives,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PreparedBody {
    #[default]
    Empty,
    Bytes(Vec<u8>),
}

/// What a protocol hands back to the runner.
#[derive(Clone, Debug, Default)]
pub struct Response {
    pub success: bool,
    /// The HTTP status, when the protocol has one.
    pub status: Option<u16>,
    /// `HTTP/1.1` and friends; protocols without a status use it as the whole status line.
    pub status_line: String,
    pub url: String,
    /// Lower-cased names in received order.
    pub headers: Vec<(String, String)>,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    /// A protocol-specific rendering (such as a message transcript) that replaces the default
    /// content-type based formatting of `body`.
    pub formatted: Option<String>,
    pub elapsed: Duration,
    /// Set when the exchange failed part way; whatever was received is still rendered.
    pub error: Option<String>,
}

/// Shared resources a protocol may use.
pub struct Context<'a> {
    pub session: &'a Session,
    pub http: &'a http::Clients,
    /// The `.http` file's directory, for resolving relative paths.
    pub base_dir: &'a Path,
    pub workspace_roots: &'a [PathBuf],
}

pub async fn send(request: &PreparedRequest, context: &Context<'_>) -> Result<Response, String> {
    match Protocol::of(&request.method) {
        Protocol::Http => http::send(request, context).await,
        Protocol::GraphQl => Err(unsupported("GraphQL")),
        Protocol::WebSocket => websocket::send(request, context).await,
        Protocol::Grpc => grpc::send(request, context).await,
    }
}

fn unsupported(protocol: &str) -> String {
    format!("{protocol} requests are not yet supported")
}

/// Every TLS client uses rustls with the ring provider, installed once per process.
pub fn install_crypto_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Formats an error with its source chain, which is where reqwest and friends put the cause.
pub fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}
