//! Orchestrates a run: select blocks → pre-request scripts → substitute → send → response
//! handlers → report.

use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, RwLock},
};

use serde_json::Value;

use crate::{
    body,
    protocol::{self, http::Clients, Context, PreparedRequest, Response},
    report::{Body, ExecutedRequest, Execution, Report, ScriptConsoleEntry, Timings},
    script::{
        ScriptEffects, ScriptEngine, ScriptInput, ScriptRequest, ScriptResponse, MAX_SOURCE_BYTES,
    },
    session::{NamedResponse, Session},
    syntax::{self, Document, Header, RequestBlock, RunCommand, RunTarget, Script},
    variables::{self, Environment, Variables},
};

/// Bodies beyond this total are left out of a Send All report to keep response files sane.
const MAX_REPORT_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Bounds `run ./file.http` nesting and the files searched through `import`.
const MAX_RUN_DEPTH: usize = 8;
const MAX_IMPORTED_FILES: usize = 32;
const MAX_HTTP_FILE_BYTES: u64 = 16 * 1024 * 1024;

type BoxFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

pub struct Runner {
    session: Arc<Session>,
    http: Clients,
    scripts: ScriptEngine,
    workspace_roots: RwLock<Vec<PathBuf>>,
}

/// One `.http` file taking part in a run: the edited file, a `run ./file.http` target or a file
/// reached through `import`.
struct FileFrame {
    path: PathBuf,
    document: Document,
    environment: Environment,
    file_vars: BTreeMap<String, String>,
}

impl FileFrame {
    fn base_dir(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }
}

/// What a run executes, in document order.
#[derive(Clone, Copy)]
enum Step<'a> {
    Block(&'a RequestBlock),
    Run(&'a RunCommand),
}

/// Mutable state threaded through nested runs.
struct RunState {
    workspace_roots: Vec<PathBuf>,
    environment: Option<String>,
    /// Files currently executing through `run ./file.http`, to stop cycles.
    stack: Vec<PathBuf>,
    report: Report,
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
        Self::with_scripts(session, ScriptEngine::default())
    }

    pub fn with_scripts(session: Arc<Session>, scripts: ScriptEngine) -> Self {
        let http = Clients::new(session.cookies());
        Self {
            session,
            http,
            scripts,
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

    /// Runs the block or `run` command at the 0-based `line`, or everything when `line` is
    /// `None`.
    pub async fn run(
        &self,
        path: &Path,
        text: &str,
        line: Option<u32>,
        configured_environment: Option<&str>,
    ) -> Result<Report, String> {
        let mut state = RunState {
            workspace_roots: self.workspace_roots(),
            environment: configured_environment.map(str::to_owned),
            stack: vec![normalize(path)],
            report: Report::default(),
        };
        let frame = load_frame(path.to_path_buf(), syntax::parse(text), &state)?;
        let steps = select_steps(&frame.document, line)?;
        self.run_steps(&frame, steps, &BTreeMap::new(), &mut state)
            .await;

        let mut report = state.report;
        let mut body_bytes = 0;
        for execution in &mut report.executions {
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
        }
        Ok(report)
    }

    fn run_steps<'a>(
        &'a self,
        frame: &'a FileFrame,
        steps: Vec<Step<'a>>,
        overrides: &'a BTreeMap<String, String>,
        state: &'a mut RunState,
    ) -> BoxFuture<'a> {
        Box::pin(async move {
            for step in steps {
                match step {
                    Step::Block(block) => {
                        let execution = self.execute_in(frame, block, overrides, state).await;
                        state.report.executions.push(execution);
                    }
                    Step::Run(command) => self.run_command(frame, command, overrides, state).await,
                }
            }
        })
    }

    async fn execute_in(
        &self,
        frame: &FileFrame,
        block: &RequestBlock,
        overrides: &BTreeMap<String, String>,
        state: &RunState,
    ) -> Execution {
        let mut file_vars = frame.file_vars.clone();
        file_vars.extend(overrides.clone());
        let context = RunContext {
            base_dir: frame.base_dir(),
            workspace_roots: &state.workspace_roots,
            environment: &frame.environment,
            file_vars: &file_vars,
        };
        self.execute(block, &context).await
    }

    /// `run #Name` executes a named request from this file or its imports; `run ./file.http`
    /// executes every request in another file. `(@name=value)` overrides file variables.
    async fn run_command(
        &self,
        frame: &FileFrame,
        command: &RunCommand,
        inherited: &BTreeMap<String, String>,
        state: &mut RunState,
    ) {
        // Overrides from enclosing `run` commands apply too; this command's own win. Each
        // command gets its own copy, so nothing leaks into sibling commands.
        let mut overrides = inherited.clone();
        overrides.extend(command.overrides.iter().cloned());
        let failure = match &command.target {
            RunTarget::Request(name) => {
                if let Some(block) = find_named(&frame.document, name) {
                    let execution = self.execute_in(frame, block, &overrides, state).await;
                    state.report.executions.push(execution);
                    return;
                }
                match find_imported(frame, name, state) {
                    Ok(Some((imported, index))) => {
                        let block = &imported.document.blocks[index];
                        let execution = self.execute_in(&imported, block, &overrides, state).await;
                        state.report.executions.push(execution);
                        return;
                    }
                    Ok(None) => format!("no request named {name:?} in this file or its imports"),
                    Err(error) => error,
                }
            }
            RunTarget::File(path) => {
                let target = normalize(&frame.base_dir().join(path));
                if state.stack.contains(&target) {
                    format!(
                        "{} is already running; run cycles are not allowed",
                        target.display()
                    )
                } else if state.stack.len() >= MAX_RUN_DEPTH {
                    format!("run commands are nested more than {MAX_RUN_DEPTH} levels deep")
                } else {
                    match read_document(&target)
                        .and_then(|document| load_frame(target.clone(), document, state))
                    {
                        Ok(nested) => {
                            let steps = select_steps(&nested.document, None).unwrap_or_default();
                            state.stack.push(target);
                            self.run_steps(&nested, steps, &overrides, state).await;
                            state.stack.pop();
                            return;
                        }
                        Err(error) => error,
                    }
                }
            }
        };
        let target = match &command.target {
            RunTarget::Request(name) => format!("#{name}"),
            RunTarget::File(path) => path.clone(),
        };
        state.report.executions.push(Execution {
            block_name: format!("run {target}"),
            error: Some(failure),
            ..Execution::default()
        });
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
            responses: self.session.responses(),
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
        let request = prepare(block, &variables, context.base_dir, &mut unresolved).await;
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
            method: block.method.clone(),
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
        if let Some(redirect) = &block.redirect {
            let path = variables.substitute(&redirect.path).text;
            match body::write_redirect(context.base_dir, &path, redirect.overwrite, &response.body)
            {
                Ok(target) => execution
                    .notes
                    .push(format!("saved response body to {}", target.display())),
                Err(error) => {
                    execution.success = false;
                    execution
                        .warnings
                        .push(format!("response redirect failed: {error}"));
                }
            }
        }
        if block.directives.no_log {
            execution.body = Some(Body {
                formatted: Some("<response body not logged (@no-log)>".to_owned()),
                content: None,
            });
        }

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

/// Blocks and `run` commands in document order, or the one at `line`.
fn select_steps(document: &Document, line: Option<u32>) -> Result<Vec<Step<'_>>, String> {
    let steps = match line {
        Some(line) => {
            let run = document.runs.iter().find(|run| run.line == line);
            match run {
                Some(run) => vec![Step::Run(run)],
                None => {
                    vec![Step::Block(document.block_at(line).ok_or_else(|| {
                        format!("no request found at line {}", line + 1)
                    })?)]
                }
            }
        }
        None => {
            let mut steps: Vec<(u32, Step<'_>)> = document
                .blocks
                .iter()
                .map(|block| (block.start_line, Step::Block(block)))
                .chain(document.runs.iter().map(|run| (run.line, Step::Run(run))))
                .collect();
            steps.sort_by_key(|(line, _)| *line);
            steps.into_iter().map(|(_, step)| step).collect()
        }
    };
    if steps.is_empty() {
        return Err("no requests found in this file".to_owned());
    }
    Ok(steps)
}

fn find_named<'a>(document: &'a Document, name: &str) -> Option<&'a RequestBlock> {
    document
        .blocks
        .iter()
        .find(|block| block.name.as_deref() == Some(name))
}

/// Searches `import`ed files breadth first, following their imports too. Each file is read at
/// most once, which also stops import cycles.
fn find_imported(
    frame: &FileFrame,
    name: &str,
    state: &RunState,
) -> Result<Option<(FileFrame, usize)>, String> {
    let mut visited = vec![normalize(&frame.path)];
    let mut queue: VecDeque<PathBuf> = frame
        .document
        .imports
        .iter()
        .map(|import| frame.base_dir().join(&import.path))
        .collect();
    while let Some(path) = queue.pop_front() {
        let path = normalize(&path);
        if visited.contains(&path) {
            continue;
        }
        if visited.len() > MAX_IMPORTED_FILES {
            return Err(format!(
                "imports reach more than {MAX_IMPORTED_FILES} files"
            ));
        }
        visited.push(path.clone());
        let document = read_document(&path)?;
        if let Some(index) = document
            .blocks
            .iter()
            .position(|block| block.name.as_deref() == Some(name))
        {
            return load_frame(path, document, state).map(|frame| Some((frame, index)));
        }
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        queue.extend(
            document
                .imports
                .iter()
                .map(|import| base_dir.join(&import.path)),
        );
    }
    Ok(None)
}

fn read_document(path: &Path) -> Result<Document, String> {
    let size = std::fs::metadata(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?
        .len();
    if size > MAX_HTTP_FILE_BYTES {
        return Err(format!("{} is larger than 16 MiB", path.display()));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    Ok(syntax::parse(&text))
}

fn load_frame(path: PathBuf, document: Document, state: &RunState) -> Result<FileFrame, String> {
    let environment =
        variables::load_environment(&path, &state.workspace_roots, state.environment.as_deref())?;
    let file_vars = document
        .file_vars
        .iter()
        .map(|variable| (variable.name.clone(), variable.value.clone()))
        .collect();
    Ok(FileFrame {
        path,
        document,
        environment,
        file_vars,
    })
}

fn normalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Substitutes variables and resolves the body. Unresolved names are collected, not fatal.
async fn prepare(
    block: &RequestBlock,
    variables: &Variables,
    base_dir: &Path,
    unresolved: &mut Vec<String>,
) -> Result<PreparedRequest, String> {
    let mut failure = None;
    let mut substitute = |text: &str| {
        let result = variables.substitute(text);
        if let Some(error) = result.error {
            failure.get_or_insert(error);
        }
        for name in result.unresolved {
            if !unresolved.contains(&name) {
                unresolved.push(name);
            }
        }
        result.text
    };
    let url = substitute(&block.url);
    // A multipart boundary is resolved once so the header and the delimiters agree.
    let boundary = match &block.body {
        syntax::Body::Multipart { boundary, .. } => Some((boundary.clone(), substitute(boundary))),
        syntax::Body::Parts(_) => None,
    };
    let headers = block
        .headers
        .iter()
        .map(|header| {
            let value = match &boundary {
                Some((raw, resolved)) if header.name.eq_ignore_ascii_case("content-type") => {
                    substitute(&header.value.replacen(raw.as_str(), resolved, 1))
                }
                _ => substitute(&header.value),
            };
            Header {
                name: substitute(&header.name),
                value,
            }
        })
        .collect();
    let resolved_boundary = boundary.as_ref().map(|(_, resolved)| resolved.as_str());
    let body = body::prepare(&block.body, resolved_boundary, base_dir, &mut substitute).await?;
    if let Some(error) = failure {
        return Err(error);
    }
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
    execution.headers = response.headers.clone();

    if let Some(formatted) = &response.formatted {
        execution.body = Some(Body {
            formatted: Some(formatted.clone()),
            content: None,
        });
        return Value::String(String::from_utf8_lossy(&response.body).into_owned());
    }
    let content_type = response.content_type.as_deref();
    let (formatted, content) = format_body(&response.body, content_type);
    let value = content.clone().unwrap_or_else(|| {
        Value::String(
            decode_text(&response.body, content_type)
                .unwrap_or_else(|| String::from_utf8_lossy(&response.body).into_owned()),
        )
    });
    execution.body = Some(Body { formatted, content });
    value
}

/// Pretty-prints JSON, passes other text through and summarises binary bodies.
pub fn format_body(body: &[u8], content_type: Option<&str>) -> (Option<String>, Option<Value>) {
    if body.is_empty() {
        return (None, None);
    }
    let text = decode_text(body, content_type);
    if content_type.is_some_and(is_json) {
        if let Some(Ok(value)) = text.as_deref().map(serde_json::from_str::<Value>) {
            let formatted = serde_json::to_string_pretty(&value).ok();
            return (formatted, Some(value));
        }
    }
    match text {
        Some(text) => (Some(text), None),
        None => (
            Some(format!(
                "<binary {} bytes, {}>",
                body.len(),
                content_type.unwrap_or("unknown type")
            )),
            None,
        ),
    }
}

/// Decodes text in the declared charset, or as UTF-8 when none is declared. `None` means the
/// body is not text.
pub fn decode_text(body: &[u8], content_type: Option<&str>) -> Option<String> {
    let charset = content_type.and_then(|content_type| {
        content_type.split(';').skip(1).find_map(|parameter| {
            let (name, value) = parameter.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| value.trim().trim_matches('"'))
        })
    });
    match charset.and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes())) {
        Some(encoding) => Some(encoding.decode_without_bom_handling(body).0.into_owned()),
        None => std::str::from_utf8(body).ok().map(str::to_owned),
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
    use crate::protocol::PreparedBody;

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
        assert_eq!(
            format_body(b"caf\xe9", Some("text/plain; charset=ISO-8859-1"))
                .0
                .as_deref(),
            Some("café")
        );
        assert_eq!(
            format_body(
                b"{\"a\":\"\xe9\"}",
                Some("application/json; charset=\"latin1\"")
            )
            .1,
            Some(json!({ "a": "é" }))
        );
        assert!(is_json("application/problem+json"));
        assert!(!is_json("text/json-ish"));
    }

    #[tokio::test]
    async fn resolves_a_dynamic_multipart_boundary_once() {
        let document = syntax::parse(
            "POST http://x\nContent-Type: multipart/form-data; boundary={{$uuid}}\n\n\
             --{{$uuid}}\nContent-Disposition: form-data; name=\"a\"\n\n1\n--{{$uuid}}--\n",
        );
        let request = prepare(
            &document.blocks[0],
            &Variables::default(),
            Path::new("."),
            &mut Vec::new(),
        )
        .await
        .unwrap();
        let boundary = request.headers[0]
            .value
            .split_once("boundary=")
            .unwrap()
            .1
            .to_owned();
        assert_eq!(boundary.len(), 36);
        let PreparedBody::Bytes(body) = request.body else {
            panic!("expected a body");
        };
        let body = String::from_utf8(body).unwrap();
        assert!(body.starts_with(&format!("--{boundary}\r\n")), "{body}");
        assert!(body.ends_with(&format!("--{boundary}--\r\n")), "{body}");
    }

    #[tokio::test]
    async fn prepares_requests_with_substitution() {
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
        let request = prepare(
            &document.blocks[0],
            &variables,
            Path::new("."),
            &mut unresolved,
        )
        .await
        .unwrap();
        assert_eq!(request.url, "http://example.test/items");
        assert_eq!(request.headers[0].value, "{{token}}");
        assert_eq!(
            request.body,
            PreparedBody::Bytes(b"{\"id\": \"7\"}".to_vec())
        );
        assert_eq!(unresolved, vec!["token"]);
    }
}
