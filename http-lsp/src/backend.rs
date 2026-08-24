use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
};

use dashmap::DashMap;
use serde::Deserialize;
use serde_json::Value;
use tower_lsp::{jsonrpc::Result as LspResult, lsp_types::*, Client, LanguageServer};

use crate::{
    cache::{CachedResponse, ResponseCache},
    httpyac::{self, HttpYacCommand},
    request_index::{self, Request},
    response_format::{self, View},
};

const CMD_SEND: &str = "zed-http.send";
const CMD_SHOW: &str = "zed-http.show";
const CMD_SAVE: &str = "zed-http.save";
const CMD_HEADERS: &str = "zed-http.headers";
const ALL_COMMANDS: &[&str] = &[CMD_SEND, CMD_SHOW, CMD_SAVE, CMD_HEADERS];

// These names form a contract with the extension launcher in src/http.rs,
// which sets them when starting this server; they must stay in sync.
const HTTPYAC_NODE_ENV: &str = "ZED_HTTPYAC_NODE";
const HTTPYAC_SCRIPT_ENV: &str = "ZED_HTTPYAC_SCRIPT";
const HTTPYAC_PATH_ENV: &str = "ZED_HTTPYAC_PATH";

#[derive(Clone)]
struct OpenDocument {
    text: String,
    requests: Vec<Request>,
}

/// Entry points offered per request as `(command, lens title, action title)`.
/// The Show/Headers entries are only offered once a response is cached.
fn entry_points(has_cached: bool) -> Vec<(&'static str, &'static str, &'static str)> {
    let mut entries = vec![(CMD_SEND, "▶ Send", "▶ Send Request")];
    if has_cached {
        entries.push((CMD_SHOW, "👁 Show", "👁 Show Response"));
        entries.push((CMD_HEADERS, "◉ Headers", "◉ Show Response Headers"));
    }
    entries.push((CMD_SAVE, "💾 Save", "💾 Save Last Response"));
    entries
}

#[derive(Debug, Default)]
struct Config {
    httpyac_path: Option<String>,
}

impl Config {
    fn httpyac_command(&self) -> Result<HttpYacCommand, String> {
        let launched_path = env::var(HTTPYAC_PATH_ENV).ok();
        let node = env::var(HTTPYAC_NODE_ENV).ok();
        let script = env::var(HTTPYAC_SCRIPT_ENV).ok();
        resolve_httpyac_command(
            self.httpyac_path.as_deref(),
            launched_path.as_deref(),
            node.as_deref(),
            script.as_deref(),
        )
    }
}

fn resolve_httpyac_command(
    configured_path: Option<&str>,
    launched_path: Option<&str>,
    node: Option<&str>,
    script: Option<&str>,
) -> Result<HttpYacCommand, String> {
    if let Some(path) = configured_path.filter(|path| !path.is_empty()) {
        return Ok(HttpYacCommand::new(path, Vec::new()));
    }
    if let Some(path) = launched_path.filter(|path| !path.is_empty()) {
        return Ok(HttpYacCommand::new(path, Vec::new()));
    }

    let node = node
        .filter(|path| !path.is_empty())
        .ok_or_else(|| format!("missing {HTTPYAC_NODE_ENV} from the extension launcher"))?;
    let script = script
        .filter(|path| !path.is_empty())
        .ok_or_else(|| format!("missing {HTTPYAC_SCRIPT_ENV} from the extension launcher"))?;
    Ok(HttpYacCommand::new(node, vec![script.to_owned()]))
}

#[derive(Debug, Default, Deserialize)]
struct UserSettings {
    #[serde(default)]
    httpyac: HttpYacSettings,
}

#[derive(Debug, Default, Deserialize)]
struct HttpYacSettings {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Clone)]
pub struct Backend {
    client: Client,
    documents: Arc<DashMap<Url, OpenDocument>>,
    cache: Arc<ResponseCache>,
    config: Arc<tokio::sync::RwLock<Config>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(DashMap::new()),
            cache: Arc::new(ResponseCache::default()),
            config: Arc::new(tokio::sync::RwLock::new(Config::default())),
        }
    }

    fn reindex(&self, uri: &Url, text: &str) {
        self.documents.insert(
            uri.clone(),
            OpenDocument {
                text: text.to_owned(),
                requests: request_index::scan(text),
            },
        );
    }

    fn enclosing_request(&self, uri: &Url, line: u32) -> Option<Request> {
        let document = self.documents.get(uri)?;
        request_index::request_at_line(&document.requests, line).cloned()
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _params: InitializeParams) -> LspResult<InitializeResult> {
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "zed-http-lsp".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![CodeActionKind::new("zed-http")]),
                        resolve_provider: Some(false),
                        work_done_progress_options: Default::default(),
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: ALL_COMMANDS
                        .iter()
                        .map(|command| (*command).to_owned())
                        .collect(),
                    work_done_progress_options: Default::default(),
                }),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "zed-http-lsp ready")
            .await;
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        match serde_json::from_value::<UserSettings>(params.settings) {
            Ok(settings) => {
                self.config.write().await.httpyac_path = settings.httpyac.path;
            }
            Err(error) => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("zed-http: ignored invalid settings: {error}"),
                    )
                    .await;
            }
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        self.cache.remove_document(&uri);
        self.reindex(&uri, &params.text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Some(text) = full_document_text(&params.content_changes) else {
            return;
        };
        let uri = params.text_document.uri;
        self.cache.remove_document(&uri);
        self.reindex(&uri, text);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.remove(&params.text_document.uri);
        self.cache.remove_document(&params.text_document.uri);
    }

    async fn code_lens(&self, params: CodeLensParams) -> LspResult<Option<Vec<CodeLens>>> {
        let uri = params.text_document.uri;
        let Some(document) = self.documents.get(&uri) else {
            return Ok(None);
        };

        let mut lenses = Vec::with_capacity(document.requests.len() * 4);
        for request in &document.requests {
            let range = Range {
                start: Position {
                    line: request.line,
                    character: 0,
                },
                end: Position {
                    line: request.line,
                    character: 0,
                },
            };
            let has_cached = self.cache.get(&uri, request.line).is_some();
            for (command, title, _) in entry_points(has_cached) {
                lenses.push(lens(range, title, command, &uri, request.line));
            }
        }

        Ok(Some(lenses))
    }

    async fn code_action(&self, params: CodeActionParams) -> LspResult<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let Some(request) = self.enclosing_request(&uri, params.range.start.line) else {
            return Ok(None);
        };

        let has_cached = self.cache.get(&uri, request.line).is_some();
        let actions = entry_points(has_cached)
            .into_iter()
            .map(|(command, _, title)| action(title, command, &uri, request.line))
            .collect();

        Ok(Some(actions))
    }

    async fn hover(&self, params: HoverParams) -> LspResult<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let line = params.text_document_position_params.position.line;
        let Some(request) = self.enclosing_request(&uri, line) else {
            return Ok(None);
        };

        let mut markdown = format!("**{}** `{}`\n", request.method, request.url);
        if let Some(cached) = self.cache.get(&uri, request.line) {
            if let Some(response) = cached
                .exchange
                .requests
                .first()
                .and_then(|request| request.response.as_ref())
            {
                let duration = response
                    .timings
                    .as_ref()
                    .and_then(|timings| timings.total)
                    .map(|duration| format!(" · {duration:.0} ms"))
                    .unwrap_or_default();
                markdown.push_str(&format!(
                    "\nLast response: **{} {}**{} at {}\n",
                    response.status_code,
                    response.status_message.as_deref().unwrap_or_default(),
                    duration,
                    cached.received_at.format("%H:%M:%S"),
                ));
            }
        } else {
            markdown.push_str("\n_No response cached. Send the request to inspect it._\n");
        }

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: markdown,
            }),
            range: None,
        }))
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> LspResult<Option<Value>> {
        let Some((uri, line)) = parse_args(&params.arguments) else {
            self.show_error("command is missing its URI or line argument")
                .await;
            return Ok(None);
        };

        let backend = self.clone();
        tokio::spawn(async move {
            match params.command.as_str() {
                CMD_SEND => backend.run_send(uri, line).await,
                CMD_SHOW => backend.run_show(uri, line, View::Full).await,
                CMD_HEADERS => backend.run_show(uri, line, View::HeadersOnly).await,
                CMD_SAVE => backend.run_save(uri, line).await,
                command => {
                    backend
                        .show_error(format!("unknown command {command}"))
                        .await
                }
            }
        });

        Ok(None)
    }

    async fn shutdown(&self) -> LspResult<()> {
        Ok(())
    }
}

impl Backend {
    async fn show_error(&self, message: impl Into<String>) {
        self.client
            .show_message(MessageType::ERROR, format!("zed-http: {}", message.into()))
            .await;
    }

    /// Returns the cached response for a request, warning the user on a miss.
    async fn cached_response(&self, uri: &Url, line: u32) -> Option<CachedResponse> {
        let cached = self.cache.get(uri, line);
        if cached.is_none() {
            self.client
                .show_message(
                    MessageType::WARNING,
                    "zed-http: no response is cached for this request; send it first",
                )
                .await;
        }
        cached
    }

    async fn run_send(&self, uri: Url, line: u32) {
        let Some(path) = uri.to_file_path().ok() else {
            self.show_error(format!("cannot execute non-file URI {uri}"))
                .await;
            return;
        };
        let Some(text) = self
            .documents
            .get(&uri)
            .map(|document| document.text.clone())
        else {
            self.show_error(format!("cannot execute unopened document {uri}"))
                .await;
            return;
        };

        let command = match self.config.read().await.httpyac_command() {
            Ok(command) => command,
            Err(error) => {
                self.show_error(error).await;
                return;
            }
        };
        let request_input = match httpyac::request_input(&path, &text).await {
            Ok(input) => input,
            Err(error) => {
                self.show_error(error).await;
                return;
            }
        };
        let exchange = match httpyac::send_exchange(&command, request_input.path(), line).await {
            Ok(exchange) => Arc::new(exchange),
            Err(error) => {
                self.show_error(error).await;
                return;
            }
        };

        self.cache.insert(uri.clone(), line, Arc::clone(&exchange));
        let _ = self.client.code_lens_refresh().await;
        self.open_response(&uri, line, &exchange, View::Full).await;
    }

    async fn run_show(&self, uri: Url, line: u32, view: View) {
        let Some(cached) = self.cached_response(&uri, line).await else {
            return;
        };

        self.open_response(&uri, line, &cached.exchange, view).await;
    }

    async fn run_save(&self, uri: Url, line: u32) {
        let Some(cached) = self.cached_response(&uri, line).await else {
            return;
        };
        let dir = match parent_dir(&uri) {
            Ok(dir) => dir,
            Err(error) => {
                self.show_error(error).await;
                return;
            }
        };

        self.write_response_file(
            &uri,
            line,
            &cached.exchange,
            View::Full,
            dir,
            "failed to save response",
        )
        .await;
    }

    async fn open_response(
        &self,
        uri: &Url,
        line: u32,
        exchange: &Arc<httpyac::Exchange>,
        view: View,
    ) {
        let dir = env::temp_dir().join("zed-http").join("responses");
        self.write_response_file(uri, line, exchange, view, dir, "failed to display response")
            .await;
    }

    async fn write_response_file(
        &self,
        uri: &Url,
        line: u32,
        exchange: &httpyac::Exchange,
        view: View,
        dir: PathBuf,
        error_context: &str,
    ) {
        let base = uri_basename(uri).unwrap_or_else(|| "response".to_owned());
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f");
        let target = dir.join(format!("{base}-line{}-{timestamp}.http-resp", line + 1));
        let target_uri = match file_uri_from_path(&target) {
            Ok(uri) => uri,
            Err(error) => {
                self.show_error(format!("{error_context}: {error}")).await;
                return;
            }
        };
        let body = response_format::format(exchange, view);

        if let Err(error) = self.write_and_open(target_uri, &body).await {
            self.show_error(format!("{error_context}: {error}")).await;
        }
    }

    async fn write_and_open(&self, target_uri: Url, body: &str) -> Result<(), String> {
        if let Some(parent) = target_uri
            .to_file_path()
            .ok()
            .and_then(|path| path.parent().map(ToOwned::to_owned))
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| error.to_string())?;
        }

        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: target_uri.clone(),
                    options: Some(CreateFileOptions {
                        overwrite: Some(true),
                        ignore_if_exists: None,
                    }),
                    annotation_id: None,
                })),
                DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: target_uri,
                        version: None,
                    },
                    edits: vec![OneOf::Left(TextEdit {
                        range: Range {
                            start: Position {
                                line: 0,
                                character: 0,
                            },
                            end: Position {
                                line: 0,
                                character: 0,
                            },
                        },
                        new_text: body.to_owned(),
                    })],
                }),
            ])),
            change_annotations: None,
        };

        match self.client.apply_edit(edit).await {
            Ok(response) if response.applied => Ok(()),
            Ok(response) => Err(response
                .failure_reason
                .unwrap_or_else(|| "the editor rejected the workspace edit".to_owned())),
            Err(error) => Err(error.to_string()),
        }
    }
}

fn command_args(uri: &Url, line: u32) -> Vec<Value> {
    vec![Value::String(uri.to_string()), Value::from(line)]
}

fn lens(range: Range, title: &str, command: &str, uri: &Url, line: u32) -> CodeLens {
    CodeLens {
        range,
        command: Some(Command {
            title: title.to_owned(),
            command: command.to_owned(),
            arguments: Some(command_args(uri, line)),
        }),
        data: None,
    }
}

fn action(title: &str, command: &str, uri: &Url, line: u32) -> CodeActionOrCommand {
    CodeActionOrCommand::CodeAction(CodeAction {
        title: title.to_owned(),
        kind: Some(CodeActionKind::new("zed-http")),
        diagnostics: None,
        edit: None,
        command: Some(Command {
            title: title.to_owned(),
            command: command.to_owned(),
            arguments: Some(command_args(uri, line)),
        }),
        is_preferred: None,
        disabled: None,
        data: None,
    })
}

fn parse_args(arguments: &[Value]) -> Option<(Url, u32)> {
    let uri = Url::parse(arguments.first()?.as_str()?).ok()?;
    let line = u32::try_from(arguments.get(1)?.as_u64()?).ok()?;
    Some((uri, line))
}

fn uri_basename(uri: &Url) -> Option<String> {
    uri.to_file_path()
        .ok()?
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
}

/// Sync kind is FULL, so a change must carry the whole document.
/// An empty `content_changes` array is ignored so the cache and request
/// index stay aligned.
fn full_document_text(changes: &[TextDocumentContentChangeEvent]) -> Option<&str> {
    changes.last().map(|change| change.text.as_str())
}

fn file_uri_from_path(path: &Path) -> Result<Url, String> {
    Url::from_file_path(path)
        .map_err(|()| format!("cannot convert {} into a file URI", path.display()))
}

fn parent_dir(uri: &Url) -> Result<PathBuf, String> {
    let path = uri
        .to_file_path()
        .map_err(|_| format!("cannot save response for non-file URI {uri}"))?;
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("cannot save response; {uri} has no parent directory"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_live_settings_over_spawn_env() {
        let command = resolve_httpyac_command(
            Some("/settings/httpyac"),
            Some("/spawned/httpyac"),
            Some("/node"),
            Some("/script.js"),
        )
        .expect("configured path should resolve");

        assert_eq!(
            command,
            HttpYacCommand::new("/settings/httpyac", Vec::new())
        );
    }

    #[test]
    fn uses_spawn_env_before_configuration_arrives() {
        let command = resolve_httpyac_command(None, Some("/spawned/httpyac"), None, None)
            .expect("spawned custom path should resolve before settings arrive");

        assert_eq!(command, HttpYacCommand::new("/spawned/httpyac", Vec::new()));
    }

    #[test]
    fn empty_configured_path_falls_back_to_spawn_env() {
        let command = resolve_httpyac_command(
            Some(""),
            Some("/spawned/httpyac"),
            Some("/node"),
            Some("/script.js"),
        )
        .expect("empty settings should not hide the spawned path");

        assert_eq!(command, HttpYacCommand::new("/spawned/httpyac", Vec::new()));
    }

    #[test]
    fn uses_managed_runtime_when_no_custom_path() {
        let command = resolve_httpyac_command(None, None, Some("/node"), Some("/script.js"))
            .expect("managed runtime should resolve");

        assert_eq!(
            command,
            HttpYacCommand::new("/node", vec!["/script.js".into()])
        );
    }

    #[test]
    fn errors_without_launcher_env() {
        let error = resolve_httpyac_command(None, None, None, None)
            .expect_err("missing launcher env should fail");

        assert!(error.contains(HTTPYAC_NODE_ENV));
    }

    #[test]
    fn empty_content_changes_are_ignored() {
        assert_eq!(full_document_text(&[]), None);
    }

    #[test]
    fn full_sync_uses_last_change_text() {
        let changes = [TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "GET https://example.com\n".into(),
        }];

        assert_eq!(
            full_document_text(&changes),
            Some("GET https://example.com\n")
        );
    }

    #[test]
    fn relative_path_cannot_become_file_uri() {
        let error = file_uri_from_path(Path::new("relative/out.http-resp"))
            .expect_err("relative paths cannot be file URIs");

        assert!(error.contains("relative/out.http-resp"));
    }

    #[test]
    fn absolute_path_becomes_file_uri() {
        let path = env::temp_dir().join("zed-http-response.http-resp");
        let uri = file_uri_from_path(&path).expect("absolute temp path should convert");

        assert_eq!(uri.to_file_path().ok().as_deref(), Some(path.as_path()));
    }

    #[test]
    fn parent_dir_rejects_non_file_uri() {
        let uri = Url::parse("untitled:buffer").expect("untitled URI should parse");
        let error = parent_dir(&uri).expect_err("non-file URIs cannot be saved");

        assert!(error.contains("non-file"));
    }

    #[test]
    fn parent_dir_uses_file_uri_directory() {
        let path = env::temp_dir().join("example.http");
        let uri = Url::from_file_path(&path).expect("absolute temp path should convert");
        let dir = parent_dir(&uri).expect("file URI should have a parent");

        assert_eq!(
            dir,
            path.parent().expect("temp file has a parent").to_owned()
        );
    }
}
