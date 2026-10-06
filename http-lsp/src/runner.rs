//! Orchestrates a run: select blocks → pre-request scripts → substitute → send → response
//! handlers → report.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use serde_json::Value;

use crate::{
    protocol::{self, http::Clients, Context, PreparedBody, PreparedRequest, Response},
    report::{Body, ExecutedRequest, Execution, Report, ScriptConsoleEntry, Timings},
    script::{
        ScriptEffects, ScriptEngine, ScriptInput, ScriptRequest, ScriptResponse, MAX_SOURCE_BYTES,
    },
    session::{NamedResponse, Session},
    syntax::{self, BodyPart, Header, RequestBlock, Script},
    variables::{self, Environment, Variables},
};

/// Bodies beyond this total are left out of a Send All report to keep response files sane.
const MAX_REPORT_BODY_BYTES: usize = 64 * 1024 * 1024;

pub struct Runner {
    session: Arc<Session>,
    http: Clients,
    scripts: ScriptEngine,
    workspace_roots: RwLock<Vec<PathBuf>>,
}

/// Per-run state shared by every block.
struct RunContext<'a> {
    base_dir: &'a Path,
    workspace_roots: &'a [PathBuf],
    environment: &'a Environment,
    file_vars: &'a BTreeMap<String, String>,
}

impl Runner {
    pub fn new(session: Arc<Session>) -> Self {
        let http = Clients::new(session.cookies());
        Self {
            session,
            http,
            scripts: ScriptEngine::default(),
            workspace_roots: RwLock::new(Vec::new()),
        }
    }

    pub fn set_workspace_roots(&self, roots: Vec<PathBuf>) {
        *self
            .workspace_roots
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = roots;
    }

    fn workspace_roots(&self) -> Vec<PathBuf> {
        self.workspace_roots
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Runs the block containing the 0-based `line`, or every block when `line` is `None`.
    pub async fn run(
        &self,
        path: &Path,
        text: &str,
        line: Option<u32>,
        configured_environment: Option<&str>,
    ) -> Result<Report, String> {
        let document = syntax::parse(text);
        let blocks: Vec<&RequestBlock> = match line {
            Some(line) => vec![document
                .block_at(line)
                .ok_or_else(|| format!("no request found at line {}", line + 1))?],
            None => document.blocks.iter().collect(),
        };
        if blocks.is_empty() {
            return Err("no requests found in this file".to_owned());
        }

        let workspace_roots = self.workspace_roots();
        let environment =
            variables::load_environment(path, &workspace_roots, configured_environment)?;
        let file_vars = document
            .file_vars
            .iter()
            .map(|variable| (variable.name.clone(), variable.value.clone()))
            .collect();
        let context = RunContext {
            base_dir: path.parent().unwrap_or_else(|| Path::new(".")),
            workspace_roots: &workspace_roots,
            environment: &environment,
            file_vars: &file_vars,
        };

        let mut report = Report::default();
        let mut body_bytes = 0;
        for block in blocks {
            let mut execution = self.execute(block, &context).await;
            if let Some(body) = execution.body.as_mut() {
                body_bytes += body.formatted.as_ref().map_or(0, String::len);
                if body_bytes > MAX_REPORT_BODY_BYTES {
                    body.formatted = Some(format!(
                        "<body omitted: this run exceeded {} MiB of response bodies>",
                        MAX_REPORT_BODY_BYTES / 1024 / 1024
                    ));
                    body.content = None;
                }
            }
            report.executions.push(execution);
        }
        Ok(report)
    }

    async fn execute(&self, block: &RequestBlock, context: &RunContext<'_>) -> Execution {
        let mut execution = Execution {
            block_name: block.name.clone().unwrap_or_default(),
            ..Execution::default()
        };
        let mut variables = Variables {
            request: BTreeMap::new(),
            globals: self.session.globals(),
            file: context.file_vars.clone(),
            environment: context.environment.variables.clone(),
        };

        for script in &block.pre_scripts {
            let input = ScriptInput {
                globals: variables.globals.clone(),
                request_variables: variables.request.clone(),
                environment: context.environment.variables.clone(),
                request: script_request(block, &variables),
                response: None,
            };
            let effects = self.run_script(script, &input, context).await;
            variables.request.extend(effects.request_variables.clone());
            if let Some(error) = self.apply_effects(effects, &mut execution) {
                execution.error = Some(format!("pre-request script failed: {error}"));
                return execution;
            }
            variables.globals = self.session.globals();
        }

        let mut unresolved = Vec::new();
        let request = prepare(block, &variables, &mut unresolved);
        execution.warnings = unresolved
            .iter()
            .map(|name| format!("unresolved variable {{{{{name}}}}}"))
            .collect();
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                execution.error = Some(error);
                return execution;
            }
        };
        execution.request = Some(ExecutedRequest {
            method: request.method.clone(),
            url: request.url.clone(),
        });

        let protocol_context = Context {
            session: &self.session,
            http: &self.http,
            base_dir: context.base_dir,
            workspace_roots: context.workspace_roots,
        };
        let response = match protocol::send(&request, &protocol_context).await {
            Ok(response) => response,
            Err(error) => {
                execution.error = Some(error);
                return execution;
            }
        };
        let body_value = fill_response(&mut execution, &response);

        if !block.handlers.is_empty() {
            let input = ScriptInput {
                globals: self.session.globals(),
                request_variables: variables.request.clone(),
                environment: context.environment.variables.clone(),
                request: ScriptRequest {
                    method: request.method.clone(),
                    url: request.url.clone(),
                    headers: request
                        .headers
                        .iter()
                        .map(|header| (header.name.clone(), header.value.clone()))
                        .collect(),
                },
                response: Some(ScriptResponse {
                    status: response.status,
                    headers: response.headers.clone(),
                    body: body_value.clone(),
                    content_type: response.content_type.clone(),
                }),
            };
            for script in &block.handlers {
                let mut input = input.clone();
                input.globals = self.session.globals();
                let effects = self.run_script(script, &input, context).await;
                if self.apply_effects(effects, &mut execution).is_some() {
                    execution.success = false;
                }
            }
        }

        if let Some(name) = &block.name {
            self.session.store_response(
                name,
                NamedResponse {
                    status: response.status,
                    headers: response.headers.clone(),
                    body: body_value,
                },
            );
        }
        execution
    }

    async fn run_script(
        &self,
        script: &Script,
        input: &ScriptInput,
        context: &RunContext<'_>,
    ) -> ScriptEffects {
        let source = match script {
            Script::Inline { source, .. } => source.clone(),
            Script::File { path, .. } => match read_script(&context.base_dir.join(path)).await {
                Ok(source) => source,
                Err(error) => {
                    return ScriptEffects {
                        error: Some(error),
                        ..ScriptEffects::default()
                    }
                }
            },
        };
        self.scripts.run(source, input).await
    }

    /// Applies globals and moves output into the console. Returns the script error, if any;
    /// effects made before the error are still applied.
    fn apply_effects(&self, effects: ScriptEffects, execution: &mut Execution) -> Option<String> {
        self.session.apply_globals(&effects.globals);
        let console = &mut execution.script_console;
        for log in effects.logs {
            console.push(ScriptConsoleEntry::log(log.level, log.message));
        }
        for test in effects.tests {
            let name = match test.message {
                Some(message) if !test.passed => format!("{}: {message}", test.name),
                _ => test.name,
            };
            console.push(ScriptConsoleEntry::test(name, test.passed));
        }
        if let Some(error) = &effects.error {
            console.push(ScriptConsoleEntry::log("error", error.clone()));
        }
        effects.error
    }
}

/// The request as a pre-request script sees it, substituted with the variables known so far.
fn script_request(block: &RequestBlock, variables: &Variables) -> ScriptRequest {
    ScriptRequest {
        method: block.method.clone(),
        url: variables.substitute(&block.url).text,
        headers: block
            .headers
            .iter()
            .map(|header| {
                (
                    variables.substitute(&header.name).text,
                    variables.substitute(&header.value).text,
                )
            })
            .collect(),
    }
}

async fn read_script(path: &Path) -> Result<String, String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| format!("failed to read script {}: {error}", path.display()))?;
    if metadata.len() > MAX_SOURCE_BYTES as u64 {
        return Err(format!(
            "script {} is larger than {} KiB",
            path.display(),
            MAX_SOURCE_BYTES / 1024
        ));
    }
    tokio::fs::read_to_string(path)
        .await
        .map_err(|error| format!("failed to read script {}: {error}", path.display()))
}

/// Substitutes variables and resolves the body. Unresolved names are collected, not fatal.
fn prepare(
    block: &RequestBlock,
    variables: &Variables,
    unresolved: &mut Vec<String>,
) -> Result<PreparedRequest, String> {
    let mut substitute = |text: &str| {
        let result = variables.substitute(text);
        for name in result.unresolved {
            if !unresolved.contains(&name) {
                unresolved.push(name);
            }
        }
        result.text
    };
    let url = substitute(&block.url);
    let headers = block
        .headers
        .iter()
        .map(|header| Header {
            name: substitute(&header.name),
            value: substitute(&header.value),
        })
        .collect();
    let body = match &block.body {
        syntax::Body::Parts(parts) if parts.is_empty() => PreparedBody::Empty,
        syntax::Body::Parts(parts) => {
            let mut text = Vec::with_capacity(parts.len());
            for part in parts {
                match part {
                    BodyPart::Text(part) => text.push(substitute(part)),
                    BodyPart::File { .. } => {
                        return Err("file includes in bodies are not yet supported".to_owned())
                    }
                }
            }
            PreparedBody::Bytes(text.join("\n").into_bytes())
        }
        syntax::Body::Multipart { .. } => {
            return Err("multipart bodies are not yet supported".to_owned())
        }
    };
    Ok(PreparedRequest {
        method: block.method.clone(),
        url,
        http_version: block.http_version.clone(),
        headers,
        body,
        directives: block.directives.clone(),
    })
}

/// Copies a protocol response into the report and returns the body as scripts and named
/// responses see it: parsed JSON for JSON responses, otherwise text.
fn fill_response(execution: &mut Execution, response: &Response) -> Value {
    execution.success = response.success;
    execution.error = response.error.clone();
    execution.status = response.status;
    execution.http_version = response.status_line.clone();
    execution.url = response.url.clone();
    execution.timings = Some(Timings {
        total: Some(response.elapsed.as_secs_f64() * 1000.0),
    });
    for (name, value) in &response.headers {
        let value = Value::String(value.clone());
        match execution.headers.get_mut(name) {
            Some(Value::Array(values)) => values.push(value),
            Some(existing) => *existing = Value::Array(vec![existing.take(), value]),
            None => {
                execution.headers.insert(name.clone(), value);
            }
        }
    }

    if let Some(formatted) = &response.formatted {
        execution.body = Some(Body {
            formatted: Some(formatted.clone()),
            content: None,
        });
        return Value::String(String::from_utf8_lossy(&response.body).into_owned());
    }
    let (formatted, content) = format_body(&response.body, response.content_type.as_deref());
    let value = content
        .clone()
        .unwrap_or_else(|| Value::String(String::from_utf8_lossy(&response.body).into_owned()));
    execution.body = Some(Body { formatted, content });
    value
}

/// Pretty-prints JSON, passes other text through and summarises binary bodies.
pub fn format_body(body: &[u8], content_type: Option<&str>) -> (Option<String>, Option<Value>) {
    if body.is_empty() {
        return (None, None);
    }
    if content_type.is_some_and(is_json) {
        if let Ok(value) = serde_json::from_slice::<Value>(body) {
            let formatted = serde_json::to_string_pretty(&value).ok();
            return (formatted, Some(value));
        }
    }
    match std::str::from_utf8(body) {
        Ok(text) => (Some(text.to_owned()), None),
        Err(_) => (
            Some(format!(
                "<binary {} bytes, {}>",
                body.len(),
                content_type.unwrap_or("unknown type")
            )),
            None,
        ),
    }
}

pub fn is_json(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    mime == "application/json" || mime.ends_with("+json")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn formats_json_text_and_binary_bodies() {
        let (formatted, content) =
            format_body(br#"{"b":1,"a":2}"#, Some("application/json; charset=utf-8"));
        assert_eq!(formatted.unwrap(), "{\n  \"b\": 1,\n  \"a\": 2\n}");
        assert_eq!(content, Some(json!({ "b": 1, "a": 2 })));
        assert_eq!(
            format_body(b"plain", Some("text/plain")).0.as_deref(),
            Some("plain")
        );
        assert_eq!(
            format_body(&[0xff, 0xfe, 0x00], Some("image/png"))
                .0
                .as_deref(),
            Some("<binary 3 bytes, image/png>")
        );
        assert!(is_json("application/problem+json"));
        assert!(!is_json("text/json-ish"));
    }

    #[test]
    fn prepares_requests_with_substitution() {
        let document =
            syntax::parse("POST {{host}}/items\nX-Token: {{token}}\n\n{\"id\": \"{{id}}\"}\n");
        let variables = Variables {
            file: BTreeMap::from([
                ("host".to_owned(), "http://example.test".to_owned()),
                ("id".to_owned(), "7".to_owned()),
            ]),
            ..Variables::default()
        };
        let mut unresolved = Vec::new();
        let request = prepare(&document.blocks[0], &variables, &mut unresolved).unwrap();
        assert_eq!(request.url, "http://example.test/items");
        assert_eq!(request.headers[0].value, "{{token}}");
        assert_eq!(
            request.body,
            PreparedBody::Bytes(b"{\"id\": \"7\"}".to_vec())
        );
        assert_eq!(unresolved, vec!["token"]);
    }
}
