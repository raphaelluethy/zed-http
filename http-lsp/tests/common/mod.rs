//! An in-process axum server and temporary workspaces for the integration tests.

use std::{
    env, fs,
    net::SocketAddr,
    path::PathBuf,
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Bytes,
    extract::Multipart,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect},
    routing::{any, get},
    Json, Router,
};
use serde_json::{json, Value};
use tower_http::compression::CompressionLayer;
use zed_http_lsp::{
    report::Report,
    runner::Runner,
    script::{Limits, ScriptEngine},
    session::Session,
};

/// Script workers are the real binary; the test executable cannot act as one.
pub fn script_engine(limits: Limits) -> ScriptEngine {
    ScriptEngine::new(limits, PathBuf::from(env!("CARGO_BIN_EXE_zed-http-lsp")))
}

pub async fn start_server() -> SocketAddr {
    let compressed = Router::new()
        .route("/gzip", get(|| async { "compressed ".repeat(200) }))
        .layer(CompressionLayer::new());
    let app = Router::new()
        .route(
            "/json",
            get(|| async { Json(json!({ "hello": "world", "items": [1, 2] })) }),
        )
        .route("/echo", any(echo))
        .route("/multipart", any(multipart))
        .route(
            "/graphql",
            any(|Json(request): Json<Value>| async move { Json(json!({ "data": request })) }),
        )
        .route(
            "/login",
            get(|| async {
                (
                    [(header::SET_COOKIE, "login=yes; Path=/")],
                    Redirect::to("/cookie"),
                )
            }),
        )
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                "slow"
            }),
        )
        .route("/redirect", get(|| async { Redirect::to("/json") }))
        .route(
            "/set-cookie",
            get(|| async { ([(header::SET_COOKIE, "session=abc; Path=/")], "set") }),
        )
        .route(
            "/cookie",
            get(|headers: HeaderMap| async move {
                headers
                    .get(header::COOKIE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("none")
                    .to_owned()
            }),
        )
        .route(
            "/latin1",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/plain; charset=iso-8859-1")],
                    b"caf\xe9".to_vec(),
                )
            }),
        )
        .route(
            "/binary",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/octet-stream")],
                    vec![0u8, 159, 146, 150],
                )
            }),
        )
        .route(
            "/status/{code}",
            get(
                |axum::extract::Path(code): axum::extract::Path<u16>| async move {
                    StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST)
                },
            ),
        )
        .merge(compressed);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

async fn echo(method: axum::http::Method, headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map_or(Value::Null, |value| Value::String(value.to_owned()))
    };
    Json(json!({
        "method": method.as_str(),
        "token": header("x-token"),
        "contentType": header("content-type"),
        "body": String::from_utf8_lossy(&body),
    }))
}

async fn multipart(mut multipart: Multipart) -> impl IntoResponse {
    let mut fields = Vec::new();
    while let Some(field) = multipart.next_field().await.unwrap() {
        fields.push(json!({
            "name": field.name(),
            "fileName": field.file_name(),
            "contentType": field.content_type(),
            "text": field.text().await.unwrap(),
        }));
    }
    Json(Value::Array(fields))
}

/// A temporary directory holding a `.http` file and an env file pointing at `address`.
pub struct Workspace {
    pub root: PathBuf,
}

impl Workspace {
    pub fn new(address: SocketAddr) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = env::temp_dir().join(format!(
            "zed-http-it-{}-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("http-client.env.json"),
            json!({
                "test": { "baseUrl": format!("http://{address}"), "token": "env-token" }
            })
            .to_string(),
        )
        .unwrap();
        Self { root }
    }

    pub fn path(&self) -> PathBuf {
        self.root.join("requests.http")
    }

    pub fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    pub fn runner(&self) -> Runner {
        let runner = Runner::with_scripts(Session::new(), script_engine(Limits::default()));
        runner.set_workspace_roots(vec![self.root.clone()]);
        runner
    }

    pub async fn run(&self, runner: &Runner, text: &str, line: Option<u32>) -> Report {
        let path = self.path();
        fs::write(&path, text).unwrap();
        runner.run(&path, text, line, Some("test")).await.unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
