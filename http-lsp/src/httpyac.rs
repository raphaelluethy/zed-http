use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    process::{self, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::Deserialize;
use tokio::{io::AsyncWriteExt, process::Command};

static NEXT_REQUEST_INPUT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Deserialize)]
pub struct Exchange {
    #[serde(default)]
    pub requests: Vec<RequestResult>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RequestResult {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub response: Option<Response>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Response {
    #[serde(rename = "statusCode")]
    pub status_code: u16,
    #[serde(rename = "statusMessage", default)]
    pub status_message: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub headers: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub timings: Option<Timings>,
    #[serde(default)]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Timings {
    #[serde(default)]
    pub total: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Meta {
    #[serde(default)]
    pub size: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpYacCommand {
    executable: String,
    prefix_args: Vec<String>,
}

impl HttpYacCommand {
    pub fn new(executable: impl Into<String>, prefix_args: Vec<String>) -> Self {
        Self {
            executable: executable.into(),
            prefix_args,
        }
    }
}

pub struct RequestInput {
    path: PathBuf,
}

impl RequestInput {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RequestInput {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

pub async fn request_input(source_file: &Path, current_text: &str) -> Result<RequestInput, String> {
    let parent = source_file
        .parent()
        .ok_or_else(|| format!("request file has no parent: {}", source_file.display()))?;

    for _ in 0..100 {
        let id = NEXT_REQUEST_INPUT.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".zed-http-{}-{id}.http", process::id()));
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);

        let mut file = match options.open(&path).await {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "failed to create a request snapshot beside {}: {error}",
                    source_file.display()
                ));
            }
        };
        let input = RequestInput { path };

        let write_result = async {
            file.write_all(current_text.as_bytes()).await?;
            file.flush().await
        }
        .await;
        drop(file);
        if let Err(error) = write_result {
            drop(input);
            return Err(format!(
                "failed to write the current request buffer: {error}"
            ));
        }

        return Ok(input);
    }

    Err(format!(
        "failed to allocate a unique request snapshot beside {}",
        source_file.display()
    ))
}

/// Sends one request through httpyac and returns its parsed exchange output.
///
/// `line` is the zero-indexed source line; converted to httpyac's
/// one-indexed `--line`.
pub async fn send_exchange(
    command: &HttpYacCommand,
    file: &Path,
    line: u32,
) -> Result<Exchange, String> {
    let parent = file
        .parent()
        .ok_or_else(|| format!("request file has no parent: {}", file.display()))?;
    let file_name = file
        .file_name()
        .ok_or_else(|| format!("request path has no file name: {}", file.display()))?;

    let mut process = Command::new(&command.executable);
    process
        .args(&command.prefix_args)
        .current_dir(parent)
        .arg("send")
        .arg(file_name)
        .arg("--line")
        .arg((line + 1).to_string())
        .arg("--json")
        .arg("--output")
        .arg("exchange")
        .arg("--no-color")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = process.output().await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("httpyac command not found at `{}`", command.executable)
        } else {
            format!("failed to run httpyac: {error}")
        }
    })?;

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !output.status.success() {
        return Err(format!(
            "httpyac exited with status {}: {stderr}",
            output.status.code().unwrap_or(-1)
        ));
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("httpyac returned invalid exchange JSON: {error}; {stderr}"))
}

#[cfg(test)]
mod tests {
    use std::{
        env, process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{request_input, Exchange};

    static NEXT_TEST_FILE: AtomicU64 = AtomicU64::new(0);

    #[tokio::test]
    async fn sends_current_buffer_instead_of_stale_file() {
        let id = NEXT_TEST_FILE.fetch_add(1, Ordering::Relaxed);
        let source =
            env::temp_dir().join(format!("zed-http-stale-buffer-{}-{id}.http", process::id()));
        std::fs::write(&source, "GET https://stale.example\n")
            .expect("stale request fixture should be written");

        let input = request_input(&source, "GET https://current.example\n")
            .await
            .expect("request input should be prepared");
        let sent = tokio::fs::read_to_string(input.path())
            .await
            .expect("prepared request should be readable");
        std::fs::remove_file(source).expect("stale request fixture should be removed");

        assert_eq!(sent, "GET https://current.example\n");
    }

    #[test]
    fn deserializes_httpyac_exchange_output() {
        let exchange: Exchange = serde_json::from_str(
            r#"{
                "_meta": {"version": "1.1.0"},
                "requests": [{
                    "name": "GET https://example.com (line: 1)",
                    "response": {
                        "protocol": "HTTP/1.1",
                        "statusCode": 200,
                        "statusMessage": "OK",
                        "headers": {"content-type": "application/json"},
                        "body": "{\"ok\":true}",
                        "timings": {"total": 12},
                        "meta": {"size": "11 B"}
                    }
                }],
                "summary": {"totalRequests": 1}
            }"#,
        )
        .expect("exchange payload should deserialize");

        let response = exchange.requests[0]
            .response
            .as_ref()
            .expect("response should be present");
        assert_eq!(response.status_code, 200);
        assert_eq!(response.status_message.as_deref(), Some("OK"));
        assert_eq!(response.body, "{\"ok\":true}");
        assert_eq!(
            response.timings.as_ref().and_then(|timings| timings.total),
            Some(12.0)
        );
    }
}
