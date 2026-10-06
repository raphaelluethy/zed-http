use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fmt::Write as _,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use http::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

const CORE_PATH_ENV: &str = "KULALA_CORE_PATH";
const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_STDOUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 1024 * 1024;
const PARSE_TIMEOUT: Duration = Duration::from_secs(15);
const RUN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug)]
pub struct CoreClient {
    executable: PathBuf,
}

impl CoreClient {
    pub fn discover() -> Result<Self, String> {
        let current_exe = env::current_exe()
            .map_err(|error| format!("failed to locate the zed-http execution adapter: {error}"))?;
        let executable = resolve_executable(env::var_os(CORE_PATH_ENV), &current_exe)?;
        Ok(Self { executable })
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub async fn parse(&self, filepath: &Path, content: &str) -> Result<CoreDocument, String> {
        let value = self
            .invoke(
                json!({
                    "action": "parse",
                    "filepath": filepath,
                    "content": content,
                }),
                PARSE_TIMEOUT,
            )
            .await?;
        serde_json::from_value(value)
            .map_err(|error| format!("Kulala Core returned an invalid parse result: {error}"))
    }

    pub async fn run(
        &self,
        filepath: &Path,
        content: &str,
        one_based_line: Option<u32>,
        configured_environment: Option<&str>,
    ) -> Result<CoreReport, String> {
        let environment = match configured_environment {
            Some(environment) => Some(environment.to_owned()),
            None => self.default_environment(filepath).await?,
        };
        let mut request = json!({
            "action": "run",
            "filepath": filepath,
            "content": content,
            "haltOnError": false,
        });
        if let Some(environment) = environment {
            request["env"] = Value::String(environment);
        }
        if let Some(line) = one_based_line {
            request["limit"] = json!([{
                "filter": "cursorPosition",
                "line": line,
                "column": 1,
            }]);
        }

        let value = self.invoke(request, RUN_TIMEOUT).await?;
        let report: CoreReport = serde_json::from_value(value).map_err(|error| {
            format!("Kulala Core returned an invalid execution result: {error}")
        })?;
        if report.response_type != "responses" {
            return Err(format!(
                "Kulala Core returned an unexpected result type {:?}",
                report.response_type
            ));
        }
        Ok(report)
    }

    async fn default_environment(&self, filepath: &Path) -> Result<Option<String>, String> {
        let cwd = filepath.parent().unwrap_or_else(|| Path::new("."));
        let value = self
            .invoke(
                json!({
                    "action": "environments",
                    "filepath": filepath,
                    "cwd": cwd,
                }),
                PARSE_TIMEOUT,
            )
            .await?;
        let result: CoreEnvironments = serde_json::from_value(value).map_err(|error| {
            format!("Kulala Core returned an invalid environments result: {error}")
        })?;
        Ok(select_environment(result.environments.into_keys()))
    }

    async fn invoke(&self, payload: Value, deadline: Duration) -> Result<Value, String> {
        let input = serde_json::to_vec(&payload)
            .map_err(|error| format!("failed to encode a Kulala Core request: {error}"))?;
        if input.len() > MAX_INPUT_BYTES {
            return Err(format!(
                "HTTP document exceeds the {} MiB Core input limit",
                MAX_INPUT_BYTES / 1024 / 1024
            ));
        }

        let mut child = Command::new(&self.executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                format!(
                    "failed to start Kulala Core at {}: {error}",
                    self.executable.display()
                )
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to capture Kulala Core stdout".to_owned())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "failed to capture Kulala Core stderr".to_owned())?;
        let stdout_task = tokio::spawn(read_bounded(stdout, MAX_STDOUT_BYTES));
        let stderr_task = tokio::spawn(read_bounded(stderr, MAX_STDERR_BYTES));

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open Kulala Core stdin".to_owned())?;
        // A broken pipe means Core exited early; its exit status and stderr explain why.
        let written = async {
            stdin.write_all(&input).await?;
            stdin.shutdown().await
        }
        .await;
        drop(stdin);
        if let Err(error) = written.as_ref() {
            if error.kind() != io::ErrorKind::BrokenPipe {
                return Err(format!("failed to send a request to Kulala Core: {error}"));
            }
        }

        let status = match timeout(deadline, child.wait()).await {
            Ok(result) => {
                result.map_err(|error| format!("failed to wait for Kulala Core: {error}"))?
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(format!(
                    "Kulala Core exceeded its {} second execution limit",
                    deadline.as_secs()
                ));
            }
        };
        let (stdout, stdout_overflow) = stdout_task
            .await
            .map_err(|error| format!("failed to collect Kulala Core output: {error}"))??;
        let (stderr, stderr_overflow) = stderr_task
            .await
            .map_err(|error| format!("failed to collect Kulala Core diagnostics: {error}"))??;

        if stdout_overflow {
            return Err(format!(
                "Kulala Core output exceeded the {} MiB response limit",
                MAX_STDOUT_BYTES / 1024 / 1024
            ));
        }
        if !status.success() {
            let diagnostics = diagnostic(&stderr, stderr_overflow);
            return Err(if diagnostics.is_empty() {
                format!("Kulala Core exited with {status}")
            } else {
                format!("Kulala Core exited with {status}: {diagnostics}")
            });
        }
        if written.is_err() {
            return Err("Kulala Core exited before reading the request".to_owned());
        }
        if stdout.is_empty() {
            return Err("Kulala Core returned no output".to_owned());
        }

        let value: Value = serde_json::from_slice(&stdout)
            .map_err(|error| format!("Kulala Core returned invalid JSON: {error}"))?;
        if value.get("type").and_then(Value::as_str) == Some("error") {
            let message = value
                .get("error")
                .or_else(|| value.get("message"))
                .map(value_text)
                .unwrap_or_else(|| "unknown Core error".to_owned());
            return Err(format!("Kulala Core: {message}"));
        }
        Ok(value)
    }
}

async fn read_bounded<R>(reader: R, limit: usize) -> Result<(Vec<u8>, bool), String>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("failed to read a Kulala Core pipe: {error}"))?;
    let overflow = bytes.len() > limit;
    if overflow {
        bytes.truncate(limit);
    }
    Ok((bytes, overflow))
}

/// Prefers `default`, otherwise the alphabetically first environment. Names starting with `$`
/// (such as `$shared`) hold variables shared by all environments and are never selectable.
fn select_environment(names: impl IntoIterator<Item = String>) -> Option<String> {
    let mut selectable = names.into_iter().filter(|name| !name.starts_with('$'));
    let first = selectable.next()?;
    if first == "default" {
        return Some(first);
    }
    Some(selectable.find(|name| name == "default").unwrap_or(first))
}

fn diagnostic(stderr: &[u8], overflow: bool) -> String {
    let mut message = String::from_utf8_lossy(stderr).trim().to_owned();
    if overflow {
        message.push_str(" [diagnostics truncated]");
    }
    message
}

fn resolve_executable(configured: Option<OsString>, current_exe: &Path) -> Result<PathBuf, String> {
    if let Some(configured) = configured.filter(|path| !path.is_empty()) {
        let path = PathBuf::from(configured);
        return require_executable(path, "configured KULALA_CORE_PATH");
    }
    let parent = current_exe
        .parent()
        .ok_or_else(|| "zed-http execution adapter has no parent directory".to_owned())?;
    require_executable(parent.join(core_binary_name()), "bundled Kulala Core")
}

fn require_executable(path: PathBuf, source: &str) -> Result<PathBuf, String> {
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "{source} was not found at {}. Install the release bundle or set {CORE_PATH_ENV} explicitly",
            path.display()
        ))
    }
}

fn core_binary_name() -> &'static str {
    if cfg!(windows) {
        "kulala-core.exe"
    } else {
        "kulala-core"
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct CoreDocument {
    #[serde(default)]
    blocks: Vec<RequestBlock>,
    #[serde(default, rename = "hasErrors")]
    has_errors: bool,
    #[serde(default)]
    errors: Vec<CoreParseError>,
}

impl CoreDocument {
    pub fn request_blocks(&self) -> Vec<RequestBlock> {
        self.blocks
            .iter()
            .filter(|block| block.has_request && block.request.is_some())
            .cloned()
            .collect()
    }

    pub fn error_summary(&self) -> Option<String> {
        if !self.has_errors {
            return None;
        }
        let messages: Vec<_> = self
            .errors
            .iter()
            .take(3)
            .map(|error| match error.line_number {
                Some(line) => format!("line {line}: {}", error.error_message),
                None => error.error_message.clone(),
            })
            .collect();
        Some(if messages.is_empty() {
            "Kulala Core reported parse errors".to_owned()
        } else {
            messages.join("; ")
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct RequestBlock {
    position: CorePosition,
    #[serde(default, rename = "hasRequest")]
    has_request: bool,
    request: Option<Value>,
}

impl RequestBlock {
    pub fn lsp_line(&self) -> u32 {
        self.position.start.saturating_sub(1)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct CorePosition {
    start: u32,
}

#[derive(Clone, Debug, Deserialize)]
struct CoreParseError {
    #[serde(default, rename = "errorMessage")]
    error_message: String,
    #[serde(default, rename = "lineNumber")]
    line_number: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct CoreEnvironments {
    #[serde(default)]
    environments: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputView {
    Full,
    HeadersOnly,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunSummary {
    pub executed: usize,
    pub failed: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CoreReport {
    #[serde(rename = "type")]
    response_type: String,
    #[serde(default)]
    data: Vec<CoreExecution>,
}

impl CoreReport {
    pub fn summary(&self) -> RunSummary {
        RunSummary {
            executed: self.data.len(),
            failed: self
                .data
                .iter()
                .filter(|execution| execution.failed())
                .count(),
        }
    }

    pub fn render(&self, view: OutputView) -> String {
        let mut output = String::new();
        for (index, execution) in self.data.iter().enumerate() {
            if index > 0 {
                output.push('\n');
            }
            execution.render_into(view, &mut output);
        }
        output
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct CoreExecution {
    #[serde(default)]
    success: bool,
    status: Option<u16>,
    #[serde(default, rename = "httpVersion")]
    http_version: String,
    #[serde(default)]
    headers: BTreeMap<String, Value>,
    #[serde(default)]
    url: String,
    request: Option<ExecutedRequest>,
    timings: Option<CoreTimings>,
    body: Option<CoreBody>,
    #[serde(default, rename = "rawBody")]
    raw_body: String,
    error: Option<Value>,
    #[serde(default, rename = "scriptConsole")]
    script_console: Vec<ScriptConsoleEntry>,
    #[serde(default, rename = "blockName")]
    block_name: String,
}

impl CoreExecution {
    fn failed(&self) -> bool {
        !self.success
            || self.status.is_some_and(|status| status >= 400)
            || self
                .script_console
                .iter()
                .any(|entry| entry.status.as_deref() == Some("fail"))
    }

    fn render_into(&self, view: OutputView, output: &mut String) {
        if !self.block_name.is_empty() {
            let _ = writeln!(output, "# {}", self.block_name);
        }
        if let Some(request) = &self.request {
            let _ = writeln!(output, "# {} {}", request.method, request.url);
        }
        for entry in &self.script_console {
            entry.render_into(output);
        }

        if let Some(error) = &self.error {
            let _ = writeln!(output, "\nkulala-core: {}", value_text(error));
            return;
        }
        let Some(status) = self.status else {
            if self.success {
                output.push_str("\n# Kulala Core completed without an HTTP status\n");
                self.render_body(view, output);
            } else {
                output.push_str("\nkulala-core: request failed without an error message\n");
            }
            return;
        };

        let protocol = if self.http_version.is_empty() {
            "HTTP"
        } else {
            &self.http_version
        };
        let reason = StatusCode::from_u16(status)
            .ok()
            .and_then(|status| status.canonical_reason())
            .unwrap_or_default();
        let _ = writeln!(output, "\n{protocol} {status} {reason}");
        if let Some(total) = self.timings.as_ref().and_then(|timings| timings.total) {
            let _ = writeln!(output, "# {total:.0} ms · {}", self.url);
        } else if !self.url.is_empty() {
            let _ = writeln!(output, "# {}", self.url);
        }
        for (name, value) in &self.headers {
            match value {
                Value::Array(values) => {
                    for value in values {
                        let _ = writeln!(output, "{name}: {}", value_text(value));
                    }
                }
                value => {
                    let _ = writeln!(output, "{name}: {}", value_text(value));
                }
            }
        }
        self.render_body(view, output);
    }

    fn render_body(&self, view: OutputView, output: &mut String) {
        if view == OutputView::HeadersOnly {
            return;
        }
        let body = self
            .body
            .as_ref()
            .and_then(CoreBody::text)
            .filter(|body| !body.is_empty())
            .or_else(|| (!self.raw_body.is_empty()).then_some(self.raw_body.as_str()));
        if let Some(body) = body {
            output.push('\n');
            output.push_str(body);
            if !body.ends_with('\n') {
                output.push('\n');
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ExecutedRequest {
    #[serde(default)]
    method: String,
    #[serde(default)]
    url: String,
}

#[derive(Clone, Debug, Deserialize)]
struct CoreTimings {
    total: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
struct CoreBody {
    formatted: Option<String>,
    content: Option<Value>,
}

impl CoreBody {
    fn text(&self) -> Option<&str> {
        self.formatted
            .as_deref()
            .or_else(|| self.content.as_ref().and_then(Value::as_str))
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ScriptConsoleEntry {
    #[serde(default)]
    level: String,
    #[serde(default)]
    message: String,
    kind: Option<String>,
    #[serde(default, rename = "testName")]
    test_name: String,
    status: Option<String>,
}

impl ScriptConsoleEntry {
    fn render_into(&self, output: &mut String) {
        match self.kind.as_deref() {
            Some("test") => {
                let marker = if self.status.as_deref() == Some("pass") {
                    "✓"
                } else {
                    "✗"
                };
                let _ = writeln!(output, "# {marker} {}", self.test_name);
            }
            Some("assert") => {}
            _ => {
                let level = if self.level.is_empty() {
                    "log"
                } else {
                    &self.level
                };
                let _ = writeln!(output, "# [script:{level}] {}", self.message);
            }
        }
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        value => serde_json::to_string(value).unwrap_or_else(|_| "<unprintable value>".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs, process,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn parses_core_blocks_and_converts_lines_to_lsp_positions() {
        let document: CoreDocument = serde_json::from_value(serde_json::json!({
            "blocks": [{
                "name": "List users",
                "position": { "start": 3, "end": 5 },
                "hasRequest": true,
                "request": { "method": "GET", "url": "{{baseUrl}}/users" }
            }],
            "hasErrors": false,
            "directiveLinesRemoved": 0,
            "nativeBlockCount": 1
        }))
        .unwrap();

        let requests = document.request_blocks();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].lsp_line(), 2);
    }

    #[test]
    fn renders_core_responses_and_failure_summary() {
        let report: CoreReport = serde_json::from_value(serde_json::json!({
            "type": "responses",
            "data": [{
                "success": true,
                "status": 200,
                "httpVersion": "HTTP/1.1",
                "headers": { "content-type": "application/json" },
                "url": "https://example.test/users",
                "request": { "method": "GET", "url": "https://example.test/users" },
                "timings": { "total": 12.4 },
                "body": { "type": "json", "content": { "ok": true }, "formatted": "{\n  \"ok\": true\n}" },
                "rawBody": "{\"ok\":true}",
                "blockName": "List users"
            }, {
                "success": false,
                "error": "connection refused",
                "blockName": "Unavailable"
            }]
        }))
        .unwrap();

        assert_eq!(
            report.summary(),
            RunSummary {
                executed: 2,
                failed: 1
            }
        );
        let output = report.render(OutputView::Full);
        assert!(output.contains("HTTP/1.1 200 OK"));
        assert!(output.contains("content-type: application/json"));
        assert!(output.contains("\"ok\": true"));
        assert!(output.contains("connection refused"));
        assert!(!report
            .render(OutputView::HeadersOnly)
            .contains("\"ok\": true"));
    }

    #[test]
    fn selects_default_or_first_named_environment() {
        let names = |names: &[&str]| {
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            select_environment(names(&["$shared", "dev", "default"])).as_deref(),
            Some("default")
        );
        assert_eq!(
            select_environment(names(&["$shared", "dev", "prod"])).as_deref(),
            Some("dev")
        );
        assert_eq!(select_environment(names(&["$shared"])), None);
    }

    #[test]
    fn resolves_only_explicit_or_bundled_core_executables() {
        let directory = env::temp_dir().join(format!(
            "zed-http-core-resolution-{}-{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let current_exe = directory.join(if cfg!(windows) {
            "zed-http-lsp.exe"
        } else {
            "zed-http-lsp"
        });
        let bundled = directory.join(core_binary_name());
        fs::write(&bundled, b"core").unwrap();

        assert_eq!(resolve_executable(None, &current_exe).unwrap(), bundled);
        let explicit = directory.join("explicit-core");
        fs::write(&explicit, b"core").unwrap();
        assert_eq!(
            resolve_executable(Some(explicit.clone().into_os_string()), &current_exe).unwrap(),
            explicit
        );

        fs::remove_dir_all(directory).unwrap();
    }
}
