//! Terminal presentation for gutter tasks. Response files keep their plain HTTP format.

use std::fmt::Write as _;

use http::StatusCode;
use serde_json::Value;

use crate::report::Report;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[35m";

pub fn colors_enabled(is_terminal: bool) -> bool {
    if std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        return false;
    }
    if let Ok(value) = std::env::var("FORCE_COLOR") {
        return !value.is_empty() && value != "0";
    }
    is_terminal && std::env::var("TERM").as_deref() != Ok("dumb")
}

pub fn render(report: &Report, color: bool) -> String {
    let mut output = String::new();
    for (index, execution) in report.executions.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        let failed = execution.failed();
        let status_color = if failed { RED } else { GREEN };
        let marker = if failed { "✗" } else { "✓" };
        let name = if execution.block_name.is_empty() {
            "Request"
        } else {
            &execution.block_name
        };
        let _ = writeln!(
            output,
            "{} {}",
            paint(marker, status_color, color),
            paint(&clean(name), BOLD, color)
        );
        if let Some(request) = &execution.request {
            let _ = writeln!(
                output,
                "  {} {}",
                paint(&clean(&request.method), CYAN, color),
                clean(&request.url)
            );
        } else if !execution.url.is_empty() {
            let _ = writeln!(output, "  {}", clean(&execution.url));
        }
        if let Some(status) = execution.status {
            let protocol = if execution.http_version.is_empty() {
                "HTTP"
            } else {
                &execution.http_version
            };
            let reason = StatusCode::from_u16(status)
                .ok()
                .and_then(|status| status.canonical_reason())
                .unwrap_or_default();
            let _ = write!(
                output,
                "  {}",
                paint(
                    &format!("{} {status} {reason}", clean(protocol)),
                    status_color,
                    color
                )
            );
        } else if !execution.http_version.is_empty() {
            let _ = write!(
                output,
                "  {}",
                paint(&clean(&execution.http_version), status_color, color)
            );
        }
        if let Some(total) = execution.timings.as_ref().and_then(|timings| timings.total) {
            let _ = write!(
                output,
                " · {}",
                paint(&format!("{total:.0} ms"), DIM, color)
            );
        }
        output.push('\n');

        for warning in &execution.warnings {
            let _ = writeln!(
                output,
                "  {} {}",
                paint("Warning:", YELLOW, color),
                clean(warning)
            );
        }
        for note in &execution.notes {
            let _ = writeln!(output, "  {}", paint(&clean(note), DIM, color));
        }
        for entry in &execution.script_console {
            match entry.kind.as_deref() {
                Some("assert") => {}
                Some("test") => {
                    let passed = entry.status.as_deref() == Some("pass");
                    let _ = writeln!(
                        output,
                        "  {} {}",
                        paint(
                            if passed { "✓" } else { "✗" },
                            if passed { GREEN } else { RED },
                            color
                        ),
                        clean(&entry.test_name)
                    );
                }
                _ => {
                    let level = if entry.level.is_empty() {
                        "log"
                    } else {
                        &entry.level
                    };
                    let _ = writeln!(
                        output,
                        "  {} {}",
                        paint(&format!("[{}]", clean(level)), DIM, color),
                        clean(&entry.message)
                    );
                }
            }
        }
        if let Some(error) = &execution.error {
            let _ = writeln!(
                output,
                "\n  {} {}",
                paint("Error:", RED, color),
                clean(error)
            );
        } else if !execution.success && execution.status.is_none() {
            let _ = writeln!(output, "\n  {} Request failed", paint("Error:", RED, color));
        }
        if !execution.headers.is_empty() {
            let _ = writeln!(output, "\n  {}", paint("Headers", DIM, color));
            for (name, value) in &execution.headers {
                let _ = writeln!(
                    output,
                    "  {}: {}",
                    paint(&clean(name), DIM, color),
                    clean(value)
                );
            }
        }
        if let Some(body) = &execution.body {
            let text = body.formatted.clone().or_else(|| {
                body.content
                    .as_ref()
                    .and_then(|value| serde_json::to_string_pretty(value).ok())
            });
            if let Some(text) = text.filter(|text| !text.is_empty()) {
                let _ = writeln!(output, "\n  {}", paint("Body", DIM, color));
                let text = match serde_json::from_str::<Value>(&text) {
                    Ok(value) => {
                        let pretty = serde_json::to_string_pretty(&value).unwrap_or(text);
                        if color {
                            highlight_json(&pretty)
                        } else {
                            pretty
                        }
                    }
                    Err(_) => clean(&text),
                };
                output.push_str(&text);
                if !text.ends_with('\n') {
                    output.push('\n');
                }
            }
        }
    }
    if report.executions.len() > 1 {
        let summary = report.summary();
        let _ = writeln!(
            output,
            "\n{} passed · {} failed",
            summary.executed - summary.failed,
            summary.failed
        );
    }
    output
}

fn paint(text: &str, style: &str, color: bool) -> String {
    if color {
        format!("{style}{text}{RESET}")
    } else {
        text.to_owned()
    }
}

/// Prevent response text and script logs from injecting terminal control sequences.
fn clean(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() && character != '\n' && character != '\t' {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

/// Tokenize already validated JSON, keeping its indentation and escaped strings intact.
fn highlight_json(json: &str) -> String {
    let bytes = json.as_bytes();
    let mut output = String::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        let style = match bytes[index] {
            b'"' => {
                index += 1;
                while index < bytes.len() {
                    match bytes[index] {
                        b'\\' => index += 2,
                        b'"' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
                if bytes[index..]
                    .iter()
                    .find(|byte| !byte.is_ascii_whitespace())
                    == Some(&b':')
                {
                    CYAN
                } else {
                    GREEN
                }
            }
            b'-' | b'0'..=b'9' => {
                index += 1;
                while index < bytes.len()
                    && matches!(bytes[index], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    index += 1;
                }
                MAGENTA
            }
            b't' | b'f' | b'n' => {
                index += 1;
                while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
                    index += 1;
                }
                YELLOW
            }
            b'{' | b'}' | b'[' | b']' | b':' | b',' => {
                index += 1;
                DIM
            }
            _ => {
                output.push(bytes[index] as char);
                index += 1;
                continue;
            }
        };
        output.push_str(&paint(&json[start..index], style, true));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Body, Execution};

    #[test]
    fn pretty_json_and_colors_preserve_escaped_strings_and_unicode() {
        let value = serde_json::json!({"quote": "a\\b\"c", "unicode": "Grüezi 🦀", "values": [1.5, true, null]});
        let report = Report {
            executions: vec![Execution {
                success: true,
                status: Some(200),
                body: Some(Body {
                    formatted: Some(value.to_string()),
                    content: Some(value.clone()),
                }),
                ..Default::default()
            }],
        };
        let plain = render(&report, false);
        assert!(plain.contains("\n  \"quote\": "));
        assert!(!plain.contains('\x1b'));
        let colored = render(&report, true);
        assert!(colored.contains(CYAN));
        assert!(colored.contains(GREEN));
        assert!(colored.contains(MAGENTA));
        // Removing presentation codes restores the exact plain output.
        let mut stripped = colored;
        for code in [RESET, BOLD, DIM, RED, GREEN, YELLOW, CYAN, MAGENTA] {
            stripped = stripped.replace(code, "");
        }
        assert_eq!(stripped, plain);
    }

    #[test]
    fn failures_keep_partial_bodies_and_escape_terminal_controls() {
        let report = Report {
            executions: vec![Execution {
                error: Some("connection refused".into()),
                body: Some(Body {
                    formatted: Some("partial\x1b[2J".into()),
                    content: None,
                }),
                ..Default::default()
            }],
        };
        let output = render(&report, false);
        assert!(output.contains("✗ Request"));
        assert!(output.contains("Error: connection refused"));
        assert!(output.contains("partial\\u{1b}[2J"));
        assert!(!output.contains('\x1b'));
    }
}
