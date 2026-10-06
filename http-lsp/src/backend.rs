use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, RwLock},
};
use tower_lsp::{jsonrpc::Result as LspResult, lsp_types::*, Client, LanguageServer};

use crate::{
    report::{OutputView, Report, RunSummary},
    runner::Runner,
    session::Session,
    syntax::{self, RequestBlock},
};

const CMD_SEND: &str = "zed-http.send";
const CMD_SEND_ALL: &str = "zed-http.sendAll";
const CMD_SAVE: &str = "zed-http.save";
const CMD_GO_TO_LOCATIONS: &str = "editor.action.goToLocations";
const ALL_COMMANDS: &[&str] = &[CMD_SEND, CMD_SEND_ALL, CMD_SAVE];

#[derive(Clone)]
struct OpenDocument {
    text: String,
    requests: Option<Vec<RequestBlock>>,
}

#[derive(Clone)]
struct CachedResponse {
    report: Arc<Report>,
    full_uri: Url,
    headers_uri: Url,
}

#[derive(Clone)]
pub struct Backend {
    client: Client,
    runner: Arc<Runner>,
    documents: Arc<RwLock<HashMap<Url, OpenDocument>>>,
    cache: Arc<RwLock<HashMap<(Url, u32), CachedResponse>>>,
    execution_lock: Arc<Mutex<()>>,
    response_directory: Arc<PathBuf>,
    work_done_progress: Arc<AtomicBool>,
    next_progress_id: Arc<AtomicU64>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            runner: Arc::new(Runner::new(Session::new())),
            documents: Arc::new(RwLock::new(HashMap::new())),
            cache: Arc::new(RwLock::new(HashMap::new())),
            execution_lock: Arc::new(Mutex::new(())),
            response_directory: Arc::new(
                env::temp_dir()
                    .join("zed-http")
                    .join(format!("responses-{}", process::id())),
            ),
            work_done_progress: Arc::new(AtomicBool::new(false)),
            next_progress_id: Arc::new(AtomicU64::new(0)),
        }
    }

    async fn store_document(&self, uri: Url, text: String) {
        self.documents.write().await.insert(
            uri,
            OpenDocument {
                text,
                requests: None,
            },
        );
    }

    async fn request_blocks(&self, uri: &Url) -> Option<Vec<RequestBlock>> {
        let (text, cached) = self
            .documents
            .read()
            .await
            .get(uri)
            .map(|document| (document.text.clone(), document.requests.clone()))?;
        if let Some(requests) = cached {
            return Some(requests);
        }
        let document = syntax::parse(&text);
        if let Some(error) = document.error_summary() {
            self.client
                .log_message(MessageType::WARNING, format!("zed-http: {error}"))
                .await;
        }
        let requests = document.blocks;

        let mut documents = self.documents.write().await;
        if let Some(open_document) = documents.get_mut(uri) {
            if open_document.text == text {
                open_document.requests = Some(requests.clone());
            }
        }
        Some(requests)
    }

    async fn remove_cached_document(&self, uri: &Url) {
        self.cache
            .write()
            .await
            .retain(|(cached_uri, _), _| cached_uri != uri);
    }

    async fn show_error(&self, message: impl Into<String>) {
        self.client
            .show_message(MessageType::ERROR, format!("zed-http: {}", message.into()))
            .await;
    }

    async fn cached_response(&self, uri: &Url, line: u32) -> Option<CachedResponse> {
        let cached = self.cache.read().await.get(&(uri.clone(), line)).cloned();
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
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        self.runner.set_workspace_roots(workspace_roots(&params));
        let work_done_progress = params
            .capabilities
            .window
            .and_then(|window| window.work_done_progress)
            .unwrap_or(false);
        self.work_done_progress
            .store(work_done_progress, Ordering::Relaxed);
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "zed-http execution adapter".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
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
            .log_message(MessageType::INFO, "zed-http execution adapter ready")
            .await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        self.remove_cached_document(&uri).await;
        self.store_document(uri, params.text_document.text).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Some(text) = full_document_text(&params.content_changes) else {
            return;
        };
        let uri = params.text_document.uri;
        self.remove_cached_document(&uri).await;
        self.store_document(uri, text.to_owned()).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents
            .write()
            .await
            .remove(&params.text_document.uri);
        self.remove_cached_document(&params.text_document.uri).await;
    }

    async fn code_lens(&self, params: CodeLensParams) -> LspResult<Option<Vec<CodeLens>>> {
        let uri = params.text_document.uri;
        let Some(requests) = self.request_blocks(&uri).await else {
            return Ok(None);
        };
        let cache = self.cache.read().await;
        let mut lenses = Vec::with_capacity(requests.len() * 4 + 1);

        for (index, request) in requests.iter().enumerate() {
            let line = request.start_line;
            let range = line_range(line);
            if index == 0 {
                lenses.push(lens(range, "▶ Send All", CMD_SEND_ALL, &uri, line));
            }
            lenses.push(lens(range, "▶ Send", CMD_SEND, &uri, line));
            if let Some(cached) = cache.get(&(uri.clone(), line)) {
                lenses.push(location_lens(range, "👁 Show", &uri, line, &cached.full_uri));
                lenses.push(location_lens(
                    range,
                    "◉ Headers",
                    &uri,
                    line,
                    &cached.headers_uri,
                ));
                lenses.push(lens(range, "💾 Save", CMD_SAVE, &uri, line));
            }
        }

        Ok(Some(lenses))
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> LspResult<Option<Value>> {
        let Some((uri, line)) = parse_args(&params.arguments) else {
            self.show_error("command is missing its URI or line argument")
                .await;
            return Ok(None);
        };

        match params.command.as_str() {
            CMD_SEND => self.spawn_execute(uri, Some(line), line),
            CMD_SEND_ALL => self.spawn_execute(uri, None, line),
            CMD_SAVE => self.run_save(uri, line).await,
            command => self.show_error(format!("unknown command {command}")).await,
        }

        Ok(None)
    }

    async fn shutdown(&self) -> LspResult<()> {
        let _ = tokio::fs::remove_dir_all(self.response_directory.as_ref()).await;
        Ok(())
    }
}

impl Backend {
    /// Requests can run for minutes, so execute them in the background instead of holding the
    /// executeCommand request open past the client's request timeout.
    fn spawn_execute(&self, uri: Url, line: Option<u32>, cache_line: u32) {
        let backend = self.clone();
        tokio::spawn(async move { backend.run_execute(uri, line, cache_line).await });
    }

    async fn run_execute(&self, uri: Url, line: Option<u32>, cache_line: u32) {
        let title = if line.is_some() {
            "Sending request"
        } else {
            "Sending all requests"
        };
        let progress = self.begin_progress(title).await;
        let result = self.execute(&uri, line, cache_line).await;
        self.end_progress(progress).await;

        match result {
            Ok(summary) if summary.failed > 0 => {
                self.client
                    .show_message(
                        MessageType::WARNING,
                        format!(
                            "zed-http: {} of {} request(s) failed; use the Show or Headers code lens for details",
                            summary.failed, summary.executed
                        ),
                    )
                    .await;
            }
            Ok(_) => {
                self.client
                    .show_message(
                        MessageType::INFO,
                        "zed-http: response ready; use the Show or Headers code lens",
                    )
                    .await;
            }
            Err(error) => self.show_error(error).await,
        }
    }

    async fn execute(
        &self,
        uri: &Url,
        line: Option<u32>,
        cache_line: u32,
    ) -> Result<RunSummary, String> {
        let path = uri
            .to_file_path()
            .map_err(|()| format!("cannot execute non-file URI {uri}"))?;
        let text = self
            .documents
            .read()
            .await
            .get(uri)
            .map(|document| document.text.clone())
            .ok_or_else(|| format!("cannot execute unopened document {uri}"))?;

        let report = {
            let _execution_guard = self.execution_lock.lock().await;
            let environment = env::var("ZED_HTTP_ENV").ok();
            Arc::new(
                self.runner
                    .run(&path, &text, line, environment.as_deref())
                    .await?,
            )
        };

        let directory = self.response_directory.as_ref();
        let full_uri = self
            .write_response_file(uri, cache_line, &report, OutputView::Full, directory)
            .await
            .map_err(|error| format!("failed to store response: {error}"))?;
        let headers_uri = self
            .write_response_file(uri, cache_line, &report, OutputView::HeadersOnly, directory)
            .await
            .map_err(|error| format!("failed to store response headers: {error}"))?;
        let summary = report.summary();
        self.cache.write().await.insert(
            (uri.clone(), cache_line),
            CachedResponse {
                report,
                full_uri,
                headers_uri,
            },
        );
        let _ = self.client.code_lens_refresh().await;
        Ok(summary)
    }

    async fn begin_progress(&self, title: &str) -> Option<ProgressToken> {
        if !self.work_done_progress.load(Ordering::Relaxed) {
            return None;
        }
        let token = NumberOrString::String(format!(
            "zed-http/{}",
            self.next_progress_id.fetch_add(1, Ordering::Relaxed)
        ));
        self.client
            .send_request::<request::WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .await
            .ok()?;
        self.client
            .send_notification::<notification::Progress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: title.to_owned(),
                        cancellable: Some(false),
                        message: None,
                        percentage: None,
                    },
                )),
            })
            .await;
        Some(token)
    }

    async fn end_progress(&self, token: Option<ProgressToken>) {
        let Some(token) = token else {
            return;
        };
        self.client
            .send_notification::<notification::Progress>(ProgressParams {
                token,
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                    message: None,
                })),
            })
            .await;
    }

    async fn run_save(&self, uri: Url, line: u32) {
        let Some(cached) = self.cached_response(&uri, line).await else {
            return;
        };
        let Some(directory) = uri
            .to_file_path()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
        else {
            self.show_error(format!("cannot resolve the parent directory for {uri}"))
                .await;
            return;
        };
        match self
            .write_response_file(&uri, line, &cached.report, OutputView::Full, &directory)
            .await
        {
            Ok(target) => {
                self.client
                    .show_message(
                        MessageType::INFO,
                        format!("zed-http: saved response to {target}"),
                    )
                    .await;
            }
            Err(error) => {
                self.show_error(format!("failed to save response: {error}"))
                    .await
            }
        }
    }

    async fn write_response_file(
        &self,
        uri: &Url,
        line: u32,
        report: &Report,
        view: OutputView,
        directory: &Path,
    ) -> Result<Url, String> {
        let base = uri
            .to_file_path()
            .ok()
            .and_then(|path| {
                path.file_stem()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "response".to_owned());
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let target = directory.join(format!(
            "{base}-line{}-{timestamp}.http-resp",
            line.saturating_add(1)
        ));
        let body = report.render(view);
        let parent = target
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", target.display()))?;
        create_private_directory(parent).await?;
        write_private_file(&target, body.as_bytes()).await?;
        Url::from_file_path(&target)
            .map_err(|()| format!("cannot convert {} to a file URI", target.display()))
    }
}

async fn create_private_directory(path: &Path) -> Result<(), String> {
    let mut builder = tokio::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(path)
        .await
        .map_err(|error| format!("failed to create {}: {error}", path.display()))
}

async fn write_private_file(path: &Path, body: &[u8]) -> Result<(), String> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .await
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    file.write_all(body)
        .await
        .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    file.flush()
        .await
        .map_err(|error| format!("failed to flush {}: {error}", path.display()))
}

/// Workspace folders bound the env file search; `rootUri` is the fallback for older clients.
#[allow(deprecated)]
fn workspace_roots(params: &InitializeParams) -> Vec<PathBuf> {
    let folders = params
        .workspace_folders
        .iter()
        .flatten()
        .map(|folder| &folder.uri);
    folders
        .chain(params.root_uri.as_ref())
        .filter_map(|uri| uri.to_file_path().ok())
        .collect()
}

fn full_document_text(changes: &[TextDocumentContentChangeEvent]) -> Option<&str> {
    changes
        .iter()
        .rev()
        .find(|change| change.range.is_none())
        .map(|change| change.text.as_str())
}

fn line_range(line: u32) -> Range {
    Range {
        start: Position::new(line, 0),
        end: Position::new(line, 0),
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

fn location_lens(
    range: Range,
    title: &str,
    source_uri: &Url,
    source_line: u32,
    target_uri: &Url,
) -> CodeLens {
    CodeLens {
        range,
        command: Some(Command {
            title: title.to_owned(),
            command: CMD_GO_TO_LOCATIONS.to_owned(),
            arguments: Some(vec![
                Value::String(source_uri.to_string()),
                serde_json::json!({ "line": source_line, "character": 0 }),
                serde_json::json!([{
                    "uri": target_uri,
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 0 }
                    }
                }]),
            ]),
        }),
        data: None,
    }
}

fn parse_args(arguments: &[Value]) -> Option<(Url, u32)> {
    let uri = arguments.first()?.as_str()?;
    let line = arguments.get(1)?.as_u64()?.try_into().ok()?;
    Some((Url::parse(uri).ok()?, line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_arguments() {
        let arguments = vec![
            Value::String("file:///tmp/example.http".to_owned()),
            Value::from(4),
        ];
        let (uri, line) = parse_args(&arguments).unwrap();
        assert_eq!(uri.as_str(), "file:///tmp/example.http");
        assert_eq!(line, 4);
    }

    #[test]
    fn response_locations_use_zeds_client_side_navigation_command() {
        let source = Url::parse("file:///tmp/example.http").unwrap();
        let target = Url::parse("file:///tmp/example.http-resp").unwrap();
        let lens = location_lens(line_range(2), "Show", &source, 2, &target);
        let command = lens.command.unwrap();
        assert_eq!(command.command, CMD_GO_TO_LOCATIONS);
        assert_eq!(command.arguments.unwrap()[2][0]["uri"], target.as_str());
    }

    #[test]
    fn only_full_document_changes_are_indexable() {
        let changes = vec![
            TextDocumentContentChangeEvent {
                range: Some(line_range(2)),
                range_length: None,
                text: "partial".to_owned(),
            },
            TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "GET https://example.test".to_owned(),
            },
        ];
        assert_eq!(
            full_document_text(&changes),
            Some("GET https://example.test")
        );
    }
}
