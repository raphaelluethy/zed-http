//! The language server: completion, hover and parse diagnostics for `.http` files. It also runs
//! the requests that gutter tasks forward to it (see `task.rs`), so they share its in-memory
//! session for as long as Zed runs.

use std::{
    collections::{HashMap, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::sync::{Mutex, RwLock};
use tower_lsp::{jsonrpc::Result as LspResult, lsp_types::*, Client, LanguageServer};

use crate::{
    assist::{self, Known},
    runner::Runner,
    session::Session,
    syntax::{self, Document},
    task::{self, TaskOperation, TaskOutcome, TaskRequest},
    variables::{load_environment, Environment, PRIVATE_ENV_FILE, PUBLIC_ENV_FILE},
};

/// An open file, parsed once per change and shared by completion and hover.
#[derive(Clone)]
struct OpenDocument {
    text: Arc<str>,
    document: Arc<Document>,
}

const MAX_REMOTE_DOCUMENTS: usize = 8;
const MAX_REMOTE_TEXT_BYTES: usize = 4 * 1024 * 1024;
#[cfg(unix)]
const ASSIST_TIMEOUT: Duration = Duration::from_secs(2);
const RETRY_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone)]
struct Knowledge {
    client: Client,
    session: Arc<Session>,
    workspace_roots: Arc<RwLock<Vec<PathBuf>>>,
    /// Loaded environments by request file directory. Only used while the client watches the
    /// env files for us, because a change would otherwise go unnoticed.
    environments: Arc<RwLock<HashMap<PathBuf, Environment>>>,
    watching_env_files: Arc<AtomicBool>,
    remote_documents: Arc<Mutex<VecDeque<(PathBuf, OpenDocument)>>>,
}

impl Knowledge {
    async fn known(&self, uri: &Url) -> Known {
        let environment = match uri.to_file_path() {
            Ok(path) => self.environment(&path).await,
            Err(()) => Environment::default(),
        };
        Known {
            environment_name: environment.name,
            environment: environment.variables,
            globals: self.session.globals(),
            responses: self.session.responses(),
        }
    }

    async fn known_path(&self, path: &Path) -> Known {
        let environment = self.environment(path).await;
        Known {
            environment_name: environment.name,
            environment: environment.variables,
            globals: self.session.globals(),
            responses: self.session.responses(),
        }
    }

    async fn environment(&self, path: &Path) -> Environment {
        let directory = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let cache = self.watching_env_files.load(Ordering::Relaxed);
        if cache {
            if let Some(environment) = self.environments.read().await.get(&directory) {
                return environment.clone();
            }
        }
        let roots = self.workspace_roots.read().await.clone();
        let path = path.to_path_buf();
        // Env files are read with blocking I/O, which must stay off the async workers.
        let loaded = tokio::task::spawn_blocking(move || {
            load_environment(&path, &roots, env::var("ZED_HTTP_ENV").ok().as_deref())
        })
        .await
        .unwrap_or_else(|error| Err(error.to_string()));
        let environment = match loaded {
            Ok(environment) => environment,
            Err(error) => {
                self.client
                    .log_message(MessageType::WARNING, format!("zed-http: {error}"))
                    .await;
                Environment::default()
            }
        };
        if cache {
            self.environments
                .write()
                .await
                .insert(directory, environment.clone());
        }
        environment
    }

    async fn remote_document(&self, path: &Path, text: &str) -> Result<OpenDocument, String> {
        {
            let mut cache = self.remote_documents.lock().await;
            if let Some(index) = cache
                .iter()
                .position(|(seen, open)| seen == path && &*open.text == text)
            {
                let (seen, open) = cache.remove(index).expect("entry exists");
                cache.push_back((seen, open.clone()));
                return Ok(open);
            }
            cache.retain(|(seen, _)| seen != path);
        }
        let text: Arc<str> = Arc::from(text);
        let parsed = {
            let text = Arc::clone(&text);
            tokio::task::spawn_blocking(move || syntax::parse(&text))
                .await
                .map_err(|error| format!("the parser worker failed: {error}"))?
        };
        let open = OpenDocument {
            text,
            document: Arc::new(parsed),
        };
        if open.text.len() <= MAX_REMOTE_TEXT_BYTES {
            let mut cache = self.remote_documents.lock().await;
            cache.retain(|(seen, _)| seen != path);
            let mut bytes: usize =
                cache.iter().map(|(_, open)| open.text.len()).sum::<usize>() + open.text.len();
            while cache.len() >= MAX_REMOTE_DOCUMENTS || bytes > MAX_REMOTE_TEXT_BYTES {
                let Some((_, evicted)) = cache.pop_front() else {
                    break;
                };
                bytes = bytes.saturating_sub(evicted.text.len());
            }
            cache.push_back((path.to_path_buf(), open.clone()));
        }
        Ok(open)
    }

    async fn complete(
        &self,
        path: &Path,
        text: &str,
        position: Position,
    ) -> Result<TaskOutcome, String> {
        let open = self.remote_document(path, text).await?;
        let known = self.known_path(path).await;
        let items = assist::complete(&open.text, &open.document, position, &known);
        let output = serde_json::to_string(&items).map_err(|error| error.to_string())?;
        Ok(TaskOutcome {
            output,
            success: true,
        })
    }

    async fn hover(
        &self,
        path: &Path,
        text: &str,
        position: Position,
    ) -> Result<TaskOutcome, String> {
        let open = self.remote_document(path, text).await?;
        let known = self.known_path(path).await;
        let hover = assist::hover(&open.text, &open.document, position, &known);
        let output = serde_json::to_string(&hover).map_err(|error| error.to_string())?;
        Ok(TaskOutcome {
            output,
            success: true,
        })
    }
}

struct TaskSupervisor(tokio::task::JoinHandle<()>);

impl Drop for TaskSupervisor {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone)]
pub struct Backend {
    client: Client,
    runner: Arc<Runner>,
    documents: Arc<RwLock<HashMap<Url, OpenDocument>>>,
    knowledge: Knowledge,
    can_watch_files: Arc<AtomicBool>,
    listeners: Arc<Mutex<HashMap<PathBuf, task::Listener>>>,
    supervisor: Arc<Mutex<Option<TaskSupervisor>>>,
}

/// Accepts forwarded gutter-task requests for each workspace root. Runs are independent
/// tasks, so a slow request never holds up another; the session is safe to share.
async fn ensure_listeners(
    listeners: &Mutex<HashMap<PathBuf, task::Listener>>,
    knowledge: &Knowledge,
    runner: &Arc<Runner>,
    client: &Client,
    log_errors: bool,
) {
    let roots = knowledge.workspace_roots.read().await.clone();
    let mut listeners = listeners.lock().await;
    for root in roots {
        if listeners.contains_key(&root) {
            continue;
        }
        let runner = Arc::clone(runner);
        let knowledge = knowledge.clone();
        let run = move |mut request: TaskRequest| {
            let runner = Arc::clone(&runner);
            let knowledge = knowledge.clone();
            async move {
                if request.environment.is_none() {
                    request.environment = env::var("ZED_HTTP_ENV").ok();
                }
                match &request.operation {
                    TaskOperation::Run => task::execute(&runner, &request).await,
                    TaskOperation::Complete { text, position } => {
                        knowledge.complete(&request.path, text, *position).await
                    }
                    TaskOperation::Hover { text, position } => {
                        knowledge.hover(&request.path, text, *position).await
                    }
                }
            }
        };
        match task::listen(&root, run) {
            Ok(Some(listener)) => {
                listeners.insert(root, listener);
            }
            Ok(None) => {}
            Err(error) => {
                if log_errors {
                    client
                        .log_message(
                            MessageType::WARNING,
                            format!("zed-http: gutter tasks will not share this session: {error}"),
                        )
                        .await;
                }
            }
        }
    }
}

#[cfg(unix)]
enum AssistForward {
    Output(String),
    Claimed,
    Failed,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        let session = Session::new();
        Self {
            client: client.clone(),
            runner: Arc::new(Runner::new(Arc::clone(&session))),
            documents: Arc::new(RwLock::new(HashMap::new())),
            knowledge: Knowledge {
                client,
                session,
                workspace_roots: Arc::new(RwLock::new(Vec::new())),
                environments: Arc::new(RwLock::new(HashMap::new())),
                watching_env_files: Arc::new(AtomicBool::new(false)),
                remote_documents: Arc::new(Mutex::new(VecDeque::new())),
            },
            can_watch_files: Arc::new(AtomicBool::new(false)),
            listeners: Arc::new(Mutex::new(HashMap::new())),
            supervisor: Arc::new(Mutex::new(None)),
        }
    }

    /// Asks the client to report env file changes, which lets environments be cached.
    async fn watch_env_files(&self) {
        if !self.can_watch_files.load(Ordering::Relaxed) {
            return;
        }
        let watchers = [PUBLIC_ENV_FILE, PRIVATE_ENV_FILE]
            .map(|name| FileSystemWatcher {
                glob_pattern: GlobPattern::String(format!("**/{name}")),
                kind: None,
            })
            .to_vec();
        let registration = Registration {
            id: "zed-http-env-files".to_owned(),
            method: "workspace/didChangeWatchedFiles".to_owned(),
            register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                watchers,
            })
            .ok(),
        };
        if self
            .client
            .register_capability(vec![registration])
            .await
            .is_ok()
        {
            self.knowledge
                .watching_env_files
                .store(true, Ordering::Relaxed);
        }
    }

    async fn document(&self, uri: &Url) -> Option<OpenDocument> {
        self.documents.read().await.get(uri).cloned()
    }

    async fn update(&self, uri: Url, text: String, version: Option<i32>) {
        let document = syntax::parse(&text);
        let diagnostics = assist::diagnostics(&document, &text);
        self.documents.write().await.insert(
            uri.clone(),
            OpenDocument {
                text: text.into(),
                document: Arc::new(document),
            },
        );
        self.client
            .publish_diagnostics(uri, diagnostics, version)
            .await;
    }

    /// The environment and session a gutter run of this file would use.
    async fn known(&self, uri: &Url) -> Known {
        self.knowledge.known(uri).await
    }

    #[cfg(unix)]
    async fn assist_root(&self, uri: &Url) -> Option<PathBuf> {
        let path = uri.to_file_path().ok()?;
        let parent = path.parent()?;
        let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        self.knowledge
            .workspace_roots
            .read()
            .await
            .iter()
            .filter(|root| parent.starts_with(root))
            .max_by_key(|root| root.components().count())
            .cloned()
    }

    #[cfg(unix)]
    async fn owns_root(&self, root: &Path) -> bool {
        self.listeners.lock().await.contains_key(root)
    }

    #[cfg(unix)]
    async fn forward_assist(
        &self,
        uri: &Url,
        root: &Path,
        text: &str,
        position: Position,
        operation: impl FnOnce(String, Position) -> TaskOperation,
    ) -> AssistForward {
        let Ok(path) = uri.to_file_path() else {
            return AssistForward::Failed;
        };
        let request = TaskRequest::new(path, None, None, false)
            .with_operation(operation(text.to_owned(), position));
        let forwarded = tokio::time::timeout(ASSIST_TIMEOUT, task::forward(root, &request)).await;
        match forwarded {
            Ok(Some(Ok(outcome))) if outcome.success => AssistForward::Output(outcome.output),
            Ok(Some(Ok(outcome))) => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!(
                            "zed-http: the workspace language server failed this request: {}",
                            outcome.output
                        ),
                    )
                    .await;
                AssistForward::Failed
            }
            Ok(Some(Err(error))) => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!(
                            "zed-http: the workspace language server failed this request: {error}"
                        ),
                    )
                    .await;
                AssistForward::Failed
            }
            _ => {
                ensure_listeners(
                    &self.listeners,
                    &self.knowledge,
                    &self.runner,
                    &self.client,
                    false,
                )
                .await;
                if self.owns_root(root).await {
                    return AssistForward::Claimed;
                }
                self.client
                    .log_message(
                        MessageType::WARNING,
                        "zed-http: no language server serves this workspace, so results do not \
                         share its session",
                    )
                    .await;
                AssistForward::Failed
            }
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        let roots = workspace_roots(&params);
        let can_watch = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.did_change_watched_files)
            .and_then(|watched| watched.dynamic_registration)
            .unwrap_or(false);
        self.can_watch_files.store(can_watch, Ordering::Relaxed);
        self.runner.set_workspace_roots(roots.clone());
        *self.knowledge.workspace_roots.write().await = roots;
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(["{", "$", "@", ".", ":"].map(str::to_owned).to_vec()),
                    ..CompletionOptions::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "zed-http-lsp".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        ensure_listeners(
            &self.listeners,
            &self.knowledge,
            &self.runner,
            &self.client,
            true,
        )
        .await;
        self.watch_env_files().await;

        if !cfg!(unix) {
            return;
        }
        let listeners = Arc::clone(&self.listeners);
        let knowledge = self.knowledge.clone();
        let runner = Arc::clone(&self.runner);
        let client = self.client.clone();
        let supervisor = tokio::spawn(async move {
            loop {
                tokio::time::sleep(RETRY_INTERVAL).await;
                ensure_listeners(&listeners, &knowledge, &runner, &client, false).await;
            }
        });
        *self.supervisor.lock().await = Some(TaskSupervisor(supervisor));
    }

    async fn did_change_watched_files(&self, _: DidChangeWatchedFilesParams) {
        self.knowledge.environments.write().await.clear();
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let document = params.text_document;
        self.update(document.uri, document.text, Some(document.version))
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(text) = full_document_text(&params.content_changes) {
            self.update(
                params.text_document.uri,
                text.to_owned(),
                Some(params.text_document.version),
            )
            .await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.write().await.remove(&uri);
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }

    async fn completion(&self, params: CompletionParams) -> LspResult<Option<CompletionResponse>> {
        let position = params.text_document_position;
        let uri = position.text_document.uri;
        let Some(open) = self.document(&uri).await else {
            return Ok(None);
        };
        #[cfg(unix)]
        if let Some(root) = self.assist_root(&uri).await {
            if !self.owns_root(&root).await {
                let forwarded = self
                    .forward_assist(
                        &uri,
                        &root,
                        &open.text,
                        position.position,
                        |text, position| TaskOperation::Complete { text, position },
                    )
                    .await;
                match forwarded {
                    AssistForward::Output(output) => {
                        return Ok(serde_json::from_str::<Vec<CompletionItem>>(&output)
                            .ok()
                            .map(CompletionResponse::Array))
                    }
                    AssistForward::Failed => return Ok(None),
                    AssistForward::Claimed => {}
                }
            }
        }
        let known = self.known(&uri).await;
        let items = assist::complete(&open.text, &open.document, position.position, &known);
        Ok((!items.is_empty()).then_some(CompletionResponse::Array(items)))
    }

    async fn hover(&self, params: HoverParams) -> LspResult<Option<Hover>> {
        let position = params.text_document_position_params;
        let uri = position.text_document.uri;
        let Some(open) = self.document(&uri).await else {
            return Ok(None);
        };
        #[cfg(unix)]
        if let Some(root) = self.assist_root(&uri).await {
            if !self.owns_root(&root).await {
                let forwarded = self
                    .forward_assist(
                        &uri,
                        &root,
                        &open.text,
                        position.position,
                        |text, position| TaskOperation::Hover { text, position },
                    )
                    .await;
                match forwarded {
                    AssistForward::Output(output) => {
                        return Ok(serde_json::from_str::<Option<Hover>>(&output).unwrap_or(None))
                    }
                    AssistForward::Failed => return Ok(None),
                    AssistForward::Claimed => {}
                }
            }
        }
        let known = self.known(&uri).await;
        Ok(assist::hover(
            &open.text,
            &open.document,
            position.position,
            &known,
        ))
    }

    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> LspResult<Option<Vec<FoldingRange>>> {
        let Some(open) = self.document(&params.text_document.uri).await else {
            return Ok(None);
        };
        Ok(Some(assist::folding_ranges(&open.text, &open.document)))
    }

    async fn shutdown(&self) -> LspResult<()> {
        // Dropping the listeners removes their sockets.
        self.supervisor.lock().await.take();
        self.listeners.lock().await.clear();
        Ok(())
    }
}

/// Workspace folders bound the env file search and key the task sockets; `rootUri` is the
/// fallback for older clients.
#[allow(deprecated)]
fn workspace_roots(params: &InitializeParams) -> Vec<PathBuf> {
    let folders = params
        .workspace_folders
        .iter()
        .flatten()
        .map(|folder| &folder.uri);
    let mut roots: Vec<PathBuf> = folders
        .chain(params.root_uri.as_ref())
        .filter_map(|uri| uri.to_file_path().ok())
        .map(|path| fs::canonicalize(&path).unwrap_or(path))
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

fn full_document_text(changes: &[TextDocumentContentChangeEvent]) -> Option<&str> {
    changes
        .iter()
        .rev()
        .find(|change| change.range.is_none())
        .map(|change| change.text.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_full_document_changes_are_indexable() {
        let changes = vec![
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(2, 0), Position::new(2, 0))),
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

    fn knowledge() -> Knowledge {
        let (service, _socket) = tower_lsp::LspService::new(Backend::new);
        service.inner().knowledge.clone()
    }

    #[tokio::test]
    async fn remote_documents_are_cached_by_path_and_text() {
        let knowledge = knowledge();
        let path = Path::new("/tmp/requests.http");
        let first = knowledge
            .remote_document(path, "GET https://a\n")
            .await
            .unwrap();
        let again = knowledge
            .remote_document(path, "GET https://a\n")
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&first.document, &again.document));

        let edited = knowledge
            .remote_document(path, "GET https://b\n")
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&first.document, &edited.document));
        assert_eq!(
            knowledge
                .remote_documents
                .lock()
                .await
                .iter()
                .filter(|(seen, _)| seen == path)
                .count(),
            1
        );

        for text in ["GET https://a\n> {%", "GET https://a"] {
            knowledge.remote_document(path, text).await.unwrap();
        }

        for index in 0..12 {
            knowledge
                .remote_document(Path::new(&format!("/tmp/{index}.http")), "GET https://x\n")
                .await
                .unwrap();
        }
        {
            let cache = knowledge.remote_documents.lock().await;
            assert!(cache.len() <= MAX_REMOTE_DOCUMENTS);
            assert!(
                cache.iter().map(|(_, open)| open.text.len()).sum::<usize>()
                    <= MAX_REMOTE_TEXT_BYTES
            );
        }

        let big = "x".repeat(MAX_REMOTE_TEXT_BYTES / 2 + 1024);
        for index in 0..4 {
            knowledge
                .remote_document(Path::new(&format!("/tmp/big{index}.http")), &big)
                .await
                .unwrap();
        }
        let cache = knowledge.remote_documents.lock().await;
        assert!(cache.len() <= 2);
        assert!(
            cache.iter().map(|(_, open)| open.text.len()).sum::<usize>() <= MAX_REMOTE_TEXT_BYTES
        );
        drop(cache);

        let huge = "x".repeat(MAX_REMOTE_TEXT_BYTES + 1024);
        let parsed = knowledge
            .remote_document(Path::new("/tmp/huge.http"), &huge)
            .await
            .unwrap();
        assert_eq!(parsed.text.len(), huge.len());
        assert!(knowledge
            .remote_documents
            .lock()
            .await
            .iter()
            .all(|(seen, _)| seen != Path::new("/tmp/huge.http")));
    }
}
