//! The language server: completion, hover and parse diagnostics for `.http` files. It also runs
//! the requests that gutter tasks forward to it (see `task.rs`), so they share its in-memory
//! session for as long as Zed runs.

use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use tokio::sync::{Mutex, RwLock};
use tower_lsp::{jsonrpc::Result as LspResult, lsp_types::*, Client, LanguageServer};

use crate::{
    assist::{self, Known},
    runner::Runner,
    session::Session,
    syntax::{self, Document},
    task::{self, TaskRequest},
    variables::{load_environment, Environment, PRIVATE_ENV_FILE, PUBLIC_ENV_FILE},
};

/// An open file, parsed once per change and shared by completion and hover.
#[derive(Clone)]
struct OpenDocument {
    text: Arc<str>,
    document: Arc<Document>,
}

#[derive(Clone)]
pub struct Backend {
    client: Client,
    session: Arc<Session>,
    runner: Arc<Runner>,
    documents: Arc<RwLock<HashMap<Url, OpenDocument>>>,
    workspace_roots: Arc<RwLock<Vec<PathBuf>>>,
    /// Loaded environments by request file directory. Only used while the client watches the
    /// env files for us, because a change would otherwise go unnoticed.
    environments: Arc<RwLock<HashMap<PathBuf, Environment>>>,
    watching_env_files: Arc<AtomicBool>,
    can_watch_files: Arc<AtomicBool>,
    listeners: Arc<Mutex<Vec<task::Listener>>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        let session = Session::new();
        Self {
            client,
            runner: Arc::new(Runner::new(Arc::clone(&session))),
            session,
            documents: Arc::new(RwLock::new(HashMap::new())),
            workspace_roots: Arc::new(RwLock::new(Vec::new())),
            environments: Arc::new(RwLock::new(HashMap::new())),
            watching_env_files: Arc::new(AtomicBool::new(false)),
            can_watch_files: Arc::new(AtomicBool::new(false)),
            listeners: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Accepts forwarded gutter-task requests for each workspace root. Runs are independent
    /// tasks, so a slow request never holds up another; the session is safe to share.
    async fn listen_for_tasks(&self) {
        let roots = self.workspace_roots.read().await.clone();
        let mut listeners = self.listeners.lock().await;
        for root in roots {
            let runner = Arc::clone(&self.runner);
            let run = move |mut request: TaskRequest| {
                let runner = Arc::clone(&runner);
                async move {
                    if request.environment.is_none() {
                        request.environment = env::var("ZED_HTTP_ENV").ok();
                    }
                    task::execute(&runner, &request).await
                }
            };
            match task::listen(&root, run) {
                Ok(Some(listener)) => listeners.push(listener),
                Ok(None) => {}
                Err(error) => {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("zed-http: gutter tasks will not share this session: {error}"),
                        )
                        .await;
                }
            }
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
            self.watching_env_files.store(true, Ordering::Relaxed);
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
        *self.workspace_roots.write().await = roots;
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
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "zed-http-lsp".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.listen_for_tasks().await;
        self.watch_env_files().await;
    }

    async fn did_change_watched_files(&self, _: DidChangeWatchedFilesParams) {
        self.environments.write().await.clear();
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
        let Some(open) = self.document(&position.text_document.uri).await else {
            return Ok(None);
        };
        let known = self.known(&position.text_document.uri).await;
        let items = assist::complete(&open.text, &open.document, position.position, &known);
        Ok((!items.is_empty()).then_some(CompletionResponse::Array(items)))
    }

    async fn hover(&self, params: HoverParams) -> LspResult<Option<Hover>> {
        let position = params.text_document_position_params;
        let Some(open) = self.document(&position.text_document.uri).await else {
            return Ok(None);
        };
        let known = self.known(&position.text_document.uri).await;
        Ok(assist::hover(
            &open.text,
            &open.document,
            position.position,
            &known,
        ))
    }

    async fn shutdown(&self) -> LspResult<()> {
        // Dropping the listeners removes their sockets.
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
    folders
        .chain(params.root_uri.as_ref())
        .filter_map(|uri| uri.to_file_path().ok())
        .map(|path| fs::canonicalize(&path).unwrap_or(path))
        .collect()
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
}
