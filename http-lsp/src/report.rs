use std::{collections::BTreeMap, fmt::Write as _};

use http::StatusCode;
use serde_json::Value;

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

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub executions: Vec<Execution>,
}

impl Report {
    pub fn summary(&self) -> RunSummary {
        RunSummary {
            executed: self.executions.len(),
            failed: self
                .executions
                .iter()
                .filter(|execution| execution.failed())
                .count(),
        }
    }

    pub fn render(&self, view: OutputView) -> String {
        let mut output = String::new();
        for (index, execution) in self.executions.iter().enumerate() {
            if index > 0 {
                output.push('\n');
            }
            execution.render_into(view, &mut output);
        }
        output
    }
}

/// One executed block. `success` is explicit so protocols without an HTTP status (WebSocket,
/// gRPC) can report failures; an HTTP status of 400 or more also counts as a failure.
#[derive(Clone, Debug, Default)]
pub struct Execution {
    pub success: bool,
    pub status: Option<u16>,
    /// The status line protocol, such as `HTTP/1.1`. Protocols without an HTTP status use it as
    /// the whole status line.
    pub http_version: String,
    pub headers: BTreeMap<String, Value>,
    pub url: String,
    pub request: Option<ExecutedRequest>,
    pub timings: Option<Timings>,
    pub body: Option<Body>,
    pub raw_body: String,
    pub error: Option<String>,
    pub warnings: Vec<String>,
    pub script_console: Vec<ScriptConsoleEntry>,
    pub block_name: String,
}

impl Execution {
    pub fn failed(&self) -> bool {
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
        for warning in &self.warnings {
            let _ = writeln!(output, "# warning: {warning}");
        }
        for entry in &self.script_console {
            entry.render_into(output);
        }

        // Partial results (such as messages received before a failure) still render below.
        if let Some(error) = &self.error {
            let _ = writeln!(output, "\nerror: {error}");
            self.render_body(view, output);
            return;
        }
        let Some(status) = self.status else {
            if self.success {
                if !self.http_version.is_empty() {
                    let _ = writeln!(output, "\n{}", self.http_version);
                }
                self.render_details(view, output);
            } else {
                output.push_str("\nerror: request failed without an error message\n");
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
        self.render_details(view, output);
    }

    fn render_details(&self, view: OutputView, output: &mut String) {
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
            .and_then(Body::text)
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

#[derive(Clone, Debug)]
pub struct ExecutedRequest {
    pub method: String,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct Timings {
    pub total: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct Body {
    pub formatted: Option<String>,
    pub content: Option<Value>,
}

impl Body {
    fn text(&self) -> Option<&str> {
        self.formatted
            .as_deref()
            .or_else(|| self.content.as_ref().and_then(Value::as_str))
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScriptConsoleEntry {
    pub level: String,
    pub message: String,
    pub kind: Option<String>,
    pub test_name: String,
    pub status: Option<String>,
}

impl ScriptConsoleEntry {
    pub fn log(level: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: level.into(),
            message: message.into(),
            ..Self::default()
        }
    }

    pub fn test(name: impl Into<String>, passed: bool) -> Self {
        Self {
            kind: Some("test".to_owned()),
            test_name: name.into(),
            status: Some(if passed { "pass" } else { "fail" }.to_owned()),
            ..Self::default()
        }
    }

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

pub fn value_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        value => serde_json::to_string(value).unwrap_or_else(|_| "<unprintable value>".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn report() -> Report {
        Report {
            executions: vec![
                Execution {
                    success: true,
                    status: Some(200),
                    http_version: "HTTP/1.1".to_owned(),
                    headers: BTreeMap::from([(
                        "content-type".to_owned(),
                        json!("application/json"),
                    )]),
                    url: "https://example.test/users".to_owned(),
                    request: Some(ExecutedRequest {
                        method: "GET".to_owned(),
                        url: "https://example.test/users".to_owned(),
                    }),
                    timings: Some(Timings { total: Some(12.4) }),
                    body: Some(Body {
                        formatted: Some("{\n  \"ok\": true\n}".to_owned()),
                        content: Some(json!({ "ok": true })),
                    }),
                    raw_body: "{\"ok\":true}".to_owned(),
                    block_name: "List users".to_owned(),
                    ..Execution::default()
                },
                Execution {
                    success: false,
                    error: Some("connection refused".to_owned()),
                    block_name: "Unavailable".to_owned(),
                    ..Execution::default()
                },
            ],
        }
    }

    #[test]
    fn renders_responses_and_failure_summary() {
        let report = report();
        assert_eq!(
            report.summary(),
            RunSummary {
                executed: 2,
                failed: 1
            }
        );
        let output = report.render(OutputView::Full);
        assert!(output.contains("HTTP/1.1 200 OK"));
        assert!(output.contains("# 12 ms · https://example.test/users"));
        assert!(output.contains("content-type: application/json"));
        assert!(output.contains("\"ok\": true"));
        assert!(output.contains("error: connection refused"));
        assert!(!report
            .render(OutputView::HeadersOnly)
            .contains("\"ok\": true"));
    }

    #[test]
    fn renders_script_console_and_counts_failed_tests() {
        let execution = Execution {
            success: true,
            status: Some(200),
            script_console: vec![
                ScriptConsoleEntry::log("info", "hello"),
                ScriptConsoleEntry::test("status is 200", true),
                ScriptConsoleEntry::test("has token", false),
            ],
            warnings: vec!["unresolved variable {{missing}}".to_owned()],
            ..Execution::default()
        };
        let report = Report {
            executions: vec![execution],
        };
        let output = report.render(OutputView::Full);
        assert!(output.contains("# [script:info] hello"));
        assert!(output.contains("# ✓ status is 200"));
        assert!(output.contains("# ✗ has token"));
        assert!(output.contains("# warning: unresolved variable {{missing}}"));
        assert_eq!(report.summary().failed, 1);
    }

    #[test]
    fn renders_non_http_results_and_partial_bodies() {
        let succeeded = Execution {
            success: true,
            http_version: "gRPC OK".to_owned(),
            raw_body: "{}".to_owned(),
            ..Execution::default()
        };
        let failed = Execution {
            success: false,
            error: Some("connection reset".to_owned()),
            raw_body: "← partial".to_owned(),
            ..Execution::default()
        };
        let report = Report {
            executions: vec![succeeded, failed],
        };
        let output = report.render(OutputView::Full);
        assert!(output.contains("\ngRPC OK\n"));
        assert!(output.contains("error: connection reset\n\n← partial"));
        assert!(!output.contains("Kulala"));
        assert_eq!(report.summary().failed, 1);
    }
}
