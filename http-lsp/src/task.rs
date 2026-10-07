//! Terminal runs. Zed starts each gutter task as a new process, so the task forwards its request
//! over a private local socket to the language server of its workspace, which runs it with the
//! session it keeps in memory for as long as Zed runs. `client.global` values, cookies and named
//! responses therefore carry over between runs, like session cookies, and are gone when Zed or
//! the language server restarts. When no language server is reachable, the task runs the
//! request itself with a fresh session.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{runner::Runner, terminal};

/// Bumped when the messages change, so a task never talks to an incompatible server.
const PROTOCOL_VERSION: u32 = 2;
const RUN_PROTOCOL_VERSION: u32 = 1;
const MAX_HTTP_FILE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskOperation {
    #[default]
    Run,
    Complete {
        text: String,
        position: tower_lsp::lsp_types::Position,
    },
    Hover {
        text: String,
        position: tower_lsp::lsp_types::Position,
    },
}

impl TaskOperation {
    #[cfg(unix)]
    fn text(&self) -> Option<&str> {
        match self {
            TaskOperation::Run => None,
            TaskOperation::Complete { text, .. } | TaskOperation::Hover { text, .. } => Some(text),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRequest {
    pub version: u32,
    pub path: PathBuf,
    /// The 0-based line of the request to send, or `None` for every request.
    pub line: Option<u32>,
    pub environment: Option<String>,
    pub color: bool,
    #[serde(default)]
    pub operation: TaskOperation,
}

impl TaskRequest {
    pub fn new(path: PathBuf, line: Option<u32>, environment: Option<String>, color: bool) -> Self {
        Self {
            version: RUN_PROTOCOL_VERSION,
            path,
            line,
            environment,
            color,
            operation: TaskOperation::Run,
        }
    }

    pub fn with_operation(mut self, operation: TaskOperation) -> Self {
        self.version = if matches!(operation, TaskOperation::Run) {
            RUN_PROTOCOL_VERSION
        } else {
            PROTOCOL_VERSION
        };
        self.operation = operation;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskOutcome {
    pub output: String,
    /// Every executed request succeeded.
    pub success: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[cfg_attr(not(unix), allow(dead_code))]
enum TaskResponse {
    Done { output: String, success: bool },
    Failed { error: String },
    Unsupported,
}

/// Reads the file, runs the selected requests and renders them for the terminal.
pub async fn execute(runner: &Runner, request: &TaskRequest) -> Result<TaskOutcome, String> {
    if !matches!(request.operation, TaskOperation::Run) {
        return Err("this zed-http-lsp cannot serve completion or hover requests".into());
    }
    let text = read_http_file(&request.path).await?;
    let report = runner
        .run(
            &request.path,
            &text,
            request.line,
            request.environment.as_deref(),
        )
        .await?;
    let summary = report.summary();
    if summary.executed == 0 {
        return Err("no requests were executed".into());
    }
    Ok(TaskOutcome {
        output: terminal::render(&report, request.color),
        success: summary.failed == 0,
    })
}

async fn read_http_file(path: &Path) -> Result<String, String> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let mut text = String::new();
    file.take(MAX_HTTP_FILE_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .await
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    if text.len() > MAX_HTTP_FILE_BYTES {
        return Err("HTTP file is larger than 16 MiB".into());
    }
    Ok(text)
}

#[cfg(unix)]
pub use unix::{forward, listen, Listener};

#[cfg(not(unix))]
pub struct Listener;

/// Sockets are Unix-only for now; elsewhere tasks always run standalone.
#[cfg(not(unix))]
pub async fn forward(_: &Path, _: &TaskRequest) -> Option<Result<TaskOutcome, String>> {
    None
}

#[cfg(not(unix))]
pub fn listen<F, Fut>(_: &Path, _: F) -> Result<Option<Listener>, String>
where
    F: Fn(TaskRequest) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<TaskOutcome, String>> + Send + 'static,
{
    Ok(None)
}

#[cfg(unix)]
mod unix {
    use std::{
        env, fs,
        future::Future,
        io::ErrorKind,
        os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };

    use sha2::{Digest, Sha256};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::{UnixListener, UnixStream},
        task::JoinHandle,
    };

    use super::{
        TaskOperation, TaskOutcome, TaskRequest, TaskResponse, MAX_HTTP_FILE_BYTES,
        PROTOCOL_VERSION, RUN_PROTOCOL_VERSION,
    };

    const MAX_REQUEST_BYTES: u64 = 16 * 1024 * 1024 * 6 + 64 * 1024;
    const MAX_REPLY_BYTES: u64 = 128 * 1024 * 1024;
    const CONNECT_GRACE: Duration = Duration::from_millis(250);

    /// A bound socket. Dropping it stops accepting and removes the socket file.
    pub struct Listener {
        path: PathBuf,
        accept: JoinHandle<()>,
        _lock: fs::File,
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            self.accept.abort();
            fs::remove_file(&self.path).ok();
        }
    }

    fn lock_path(socket: &Path) -> PathBuf {
        socket.with_extension("lock")
    }

    fn open_lock(path: &Path) -> Result<fs::File, String> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        let uid = unsafe { libc::getuid() };
        if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(format!(
                "{} is not a private file owned by this user",
                path.display()
            ));
        }
        Ok(file)
    }

    fn remove_refused_socket(path: &Path) {
        let Ok(lock) = open_lock(&lock_path(path)) else {
            return;
        };
        if lock.try_lock().is_err() {
            return;
        }
        if std::os::unix::net::UnixStream::connect(path).is_err() {
            fs::remove_file(path).ok();
        }
    }

    /// Serves task requests for the workspace rooted at `root`. Returns `None` when another live
    /// language server already serves it, for example a second Zed window on the same folder.
    pub fn listen<F, Fut>(root: &Path, run: F) -> Result<Option<Listener>, String>
    where
        F: Fn(TaskRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<TaskOutcome, String>> + Send + 'static,
    {
        let path = socket_path(root)?;
        let lock = open_lock(&lock_path(&path))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Ok(None),
            Err(fs::TryLockError::Error(error)) => {
                return Err(format!(
                    "failed to lock {}: {error}",
                    lock_path(&path).display()
                ))
            }
        }
        if path.exists() {
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                return Ok(None);
            }
            // Left behind by a language server that did not shut down cleanly.
            fs::remove_file(&path)
                .map_err(|error| format!("failed to remove {}: {error}", path.display()))?;
        }
        let listener = UnixListener::bind(&path)
            .map_err(|error| format!("failed to listen on {}: {error}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).ok();
        let run = Arc::new(run);
        let accept = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let run = Arc::clone(&run);
                        connections.spawn(async move { serve(stream, run.as_ref()).await });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Some(Listener {
            path,
            accept,
            _lock: lock,
        }))
    }

    /// Handles one task connection. The task keeps its end open until it has the response, so
    /// end-of-file before that means it was stopped (or Zed restarted it): the run is then
    /// dropped, which cancels its requests and kills its script workers.
    async fn serve<F, Fut>(stream: UnixStream, run: &F)
    where
        F: Fn(TaskRequest) -> Fut,
        Fut: Future<Output = Result<TaskOutcome, String>>,
    {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        let read = (&mut reader)
            .take(MAX_REQUEST_BYTES)
            .read_line(&mut line)
            .await;
        let acceptable = |request: &TaskRequest| {
            request.version == PROTOCOL_VERSION
                || (request.version == RUN_PROTOCOL_VERSION
                    && matches!(request.operation, TaskOperation::Run))
        };
        let response = match read.map(|_| serde_json::from_str::<TaskRequest>(&line)) {
            Ok(Ok(request))
                if request
                    .operation
                    .text()
                    .is_some_and(|text| text.len() > MAX_HTTP_FILE_BYTES) =>
            {
                TaskResponse::Failed {
                    error: "HTTP text is larger than 16 MiB".to_owned(),
                }
            }
            Ok(Ok(request)) if acceptable(&request) => {
                let disconnected = async {
                    let mut buffer = [0; 64];
                    while matches!(reader.read(&mut buffer).await, Ok(read) if read > 0) {}
                };
                tokio::select! {
                    outcome = run(request) => match outcome {
                        Ok(outcome) => TaskResponse::Done {
                            output: outcome.output,
                            success: outcome.success,
                        },
                        Err(error) => TaskResponse::Failed { error },
                    },
                    () = disconnected => return,
                }
            }
            Ok(_) => TaskResponse::Unsupported,
            Err(error) => TaskResponse::Failed {
                error: error.to_string(),
            },
        };
        if let Ok(mut message) = serde_json::to_vec(&response) {
            message.push(b'\n');
            writer.write_all(&message).await.ok();
            writer.shutdown().await.ok();
        }
    }

    /// Sends `request` to the language server of the workspace rooted at `root`. Returns `None`
    /// when no compatible server is reachable, so the caller can run the request itself.
    pub async fn forward(
        root: &Path,
        request: &TaskRequest,
    ) -> Option<Result<TaskOutcome, String>> {
        let path = socket_path(root).ok()?;
        let deadline = tokio::time::Instant::now() + CONNECT_GRACE;
        let mut stream = loop {
            match UnixStream::connect(&path).await {
                Ok(stream) => break stream,
                Err(error) => {
                    if error.kind() == ErrorKind::ConnectionRefused {
                        // Left behind by a language server that was killed.
                        remove_refused_socket(&path);
                    }
                    if matches!(
                        error.kind(),
                        ErrorKind::ConnectionRefused | ErrorKind::NotFound
                    ) && tokio::time::Instant::now() < deadline
                    {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        continue;
                    }
                    return None;
                }
            }
        };
        let mut message = match serde_json::to_vec(request) {
            Ok(message) => message,
            Err(error) => return Some(Err(format!("could not encode the request: {error}"))),
        };
        message.push(b'\n');
        if let Err(error) = stream.write_all(&message).await {
            return Some(Err(format!(
                "lost the language server connection while sending the request: {error}"
            )));
        }
        let mut response = Vec::new();
        let read = (&mut stream)
            .take(MAX_REPLY_BYTES + 1)
            .read_to_end(&mut response)
            .await;
        match read {
            Err(error) => {
                return Some(Err(format!("lost the language server connection: {error}")))
            }
            Ok(_) if response.len() as u64 > MAX_REPLY_BYTES => {
                return Some(Err("the language server response was too large".to_owned()))
            }
            Ok(_) => {}
        }
        match serde_json::from_slice(&response) {
            Ok(TaskResponse::Done { output, success }) => Some(Ok(TaskOutcome { output, success })),
            Ok(TaskResponse::Failed { error }) => Some(Err(error)),
            Ok(TaskResponse::Unsupported) => None,
            Err(_) if response.is_empty() => Some(Err(
                "the language server closed the connection; it may have restarted".into(),
            )),
            Err(error) => Some(Err(format!("invalid language server response: {error}"))),
        }
    }

    /// `<runtime dir>/zed-http-<uid>/<hash of the workspace root>.sock`. The hash is short
    /// because socket paths are limited to about 100 bytes and macOS temp dirs are long.
    fn socket_path(root: &Path) -> Result<PathBuf, String> {
        let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
        let name: String = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(private_directory()?.join(format!("{name}.sock")))
    }

    /// A directory only the current user can enter. A pre-existing one is accepted only when it
    /// is a real directory owned by this user with no group or other permissions, because
    /// `/tmp` is shared.
    fn private_directory() -> Result<PathBuf, String> {
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        let base = env::var_os("XDG_RUNTIME_DIR")
            .filter(|directory| !directory.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(env::temp_dir);
        let directory = base.join(format!("zed-http-{uid}"));
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("failed to create {}: {error}", directory.display())),
        }
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|error| format!("failed to inspect {}: {error}", directory.display()))?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(format!(
                "{} is not a private directory owned by this user",
                directory.display()
            ));
        }
        Ok(directory)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn forwards_requests_to_the_listening_server() {
            let root = env::temp_dir().join(format!("zed-http-task-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let request = TaskRequest::new(root.join("a.http"), Some(3), None, false);
            assert!(forward(&root, &request).await.is_none());

            let listener = listen(&root, |request: TaskRequest| async move {
                Ok(TaskOutcome {
                    output: format!("line {:?}", request.line),
                    success: true,
                })
            })
            .unwrap()
            .unwrap();
            let outcome = forward(&root, &request).await.unwrap().unwrap();
            assert_eq!(outcome.output, "line Some(3)");

            // A second server for the same workspace leaves the live one in place.
            assert!(listen(&root, |_| async { Err("unused".to_owned()) })
                .unwrap()
                .is_none());

            let socket = socket_path(&root).unwrap();
            drop(listener);
            assert!(!socket.exists());
            assert!(forward(&root, &request).await.is_none());
            fs::remove_dir_all(&root).ok();
        }

        #[tokio::test]
        async fn a_stopped_task_cancels_its_run() {
            let root = env::temp_dir().join(format!("zed-http-task-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
            let dropped_tx = Arc::new(std::sync::Mutex::new(Some(dropped_tx)));
            let _listener = listen(&root, move |_| {
                // Signals when the run future is dropped without finishing.
                struct OnDrop(Option<tokio::sync::oneshot::Sender<()>>);
                impl Drop for OnDrop {
                    fn drop(&mut self) {
                        if let Some(sender) = self.0.take() {
                            sender.send(()).ok();
                        }
                    }
                }
                let guard = OnDrop(dropped_tx.lock().unwrap().take());
                async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            })
            .unwrap()
            .unwrap();

            let request = TaskRequest::new(root.join("a.http"), None, None, false);
            let path = socket_path(&root).unwrap();
            let mut stream = UnixStream::connect(&path).await.unwrap();
            let mut message = serde_json::to_vec(&request).unwrap();
            message.push(b'\n');
            stream.write_all(&message).await.unwrap();
            // The task process exits without waiting for the response.
            drop(stream);
            tokio::time::timeout(std::time::Duration::from_secs(5), dropped_rx)
                .await
                .expect("the run was not cancelled")
                .unwrap();
            fs::remove_dir_all(&root).ok();
        }

        #[tokio::test]
        async fn replaces_stale_sockets_and_reports_failures() {
            let root = env::temp_dir().join(format!("zed-http-task-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let socket = socket_path(&root).unwrap();
            drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
            assert!(socket.exists());
            let request = TaskRequest::new(root.join("a.http"), None, None, false);
            assert!(forward(&root, &request).await.is_none());
            assert!(!socket.exists(), "a refused socket is removed");
            drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());

            let _listener = listen(&root, |_| async { Err("boom".to_owned()) })
                .unwrap()
                .unwrap();
            assert_eq!(forward(&root, &request).await, Some(Err("boom".to_owned())));
            fs::remove_dir_all(&root).ok();
        }

        fn ready_outcome(_: TaskRequest) -> Result<TaskOutcome, String> {
            Ok(TaskOutcome {
                output: String::new(),
                success: true,
            })
        }

        #[tokio::test]
        async fn only_one_server_can_claim_a_root_and_the_lock_survives_it() {
            let root = env::temp_dir().join(format!("zed-http-task-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let listener = listen(&root, |_| async {
                ready_outcome(TaskRequest::new("".into(), None, None, false))
            })
            .unwrap()
            .unwrap();
            let socket = socket_path(&root).unwrap();

            assert!(listen(&root, |_| async {
                ready_outcome(TaskRequest::new("".into(), None, None, false))
            })
            .unwrap()
            .is_none());
            let lock = open_lock(&lock_path(&socket)).unwrap();
            assert!(matches!(
                lock.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            drop(lock);
            drop(listener);

            assert!(!socket.exists());
            let claimed = listen(&root, |_| async {
                ready_outcome(TaskRequest::new("".into(), None, None, false))
            })
            .unwrap();
            assert!(claimed.is_some());
            fs::remove_dir_all(&root).ok();
        }

        #[tokio::test]
        async fn racing_claims_produce_exactly_one_owner() {
            let root = env::temp_dir().join(format!("zed-http-task-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let root = root.clone();
                    std::thread::spawn(move || {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap();
                        runtime.block_on(async {
                            listen(&root, |_| async {
                                ready_outcome(TaskRequest::new("".into(), None, None, false))
                            })
                        })
                    })
                })
                .collect();
            let owners: Vec<_> = handles
                .into_iter()
                .filter_map(|handle| handle.join().unwrap().unwrap())
                .collect();
            assert_eq!(owners.len(), 1);
            drop(owners);
            fs::remove_dir_all(&root).ok();
        }

        #[tokio::test]
        async fn accepts_legacy_runs_but_rejects_legacy_assists() {
            let root = env::temp_dir().join(format!("zed-http-task-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = Arc::clone(&calls);
            let _listener = listen(&root, move |_| {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async {
                    Ok(TaskOutcome {
                        output: "ran".to_owned(),
                        success: true,
                    })
                }
            })
            .unwrap()
            .unwrap();
            let socket = socket_path(&root).unwrap();
            let file = root.join("a.http").to_string_lossy().into_owned();
            let ask = |payload: String| {
                let socket = socket.clone();
                async move {
                    let mut stream = UnixStream::connect(&socket).await.unwrap();
                    stream.write_all(payload.as_bytes()).await.unwrap();
                    let mut reply = Vec::new();
                    stream.read_to_end(&mut reply).await.unwrap();
                    serde_json::from_slice::<TaskResponse>(&reply).unwrap()
                }
            };

            let run = format!(
                "{{\"version\":1,\"path\":\"{file}\",\"line\":null,\"environment\":null,\"color\":false}}\n"
            );
            let response = ask(run).await;
            assert!(matches!(response, TaskResponse::Done { .. }));
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

            let assist = format!(
                "{{\"version\":1,\"path\":\"{file}\",\"line\":null,\"environment\":null,\"color\":false,\"operation\":{{\"kind\":\"hover\",\"text\":\"GET https://x\\n\",\"position\":{{\"line\":0,\"character\":0}}}}}}\n"
            );
            let response = ask(assist).await;
            assert!(matches!(response, TaskResponse::Unsupported));
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

            let assist = format!(
                "{{\"version\":2,\"path\":\"{file}\",\"line\":null,\"environment\":null,\"color\":false,\"operation\":{{\"kind\":\"hover\",\"text\":\"GET https://x\\n\",\"position\":{{\"line\":0,\"character\":0}}}}}}\n"
            );
            let response = ask(assist).await;
            assert!(matches!(response, TaskResponse::Done { .. }));
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
            fs::remove_dir_all(&root).ok();
        }

        #[tokio::test]
        async fn dropping_the_listener_cancels_accepted_connections() {
            let root = env::temp_dir().join(format!("zed-http-task-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
            let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
            let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
            let dropped_tx = Arc::new(std::sync::Mutex::new(Some(dropped_tx)));
            let listener = listen(&root, move |_| {
                if let Some(sender) = started_tx.lock().unwrap().take() {
                    sender.send(()).ok();
                }
                struct OnDrop(Option<tokio::sync::oneshot::Sender<()>>);
                impl Drop for OnDrop {
                    fn drop(&mut self) {
                        if let Some(sender) = self.0.take() {
                            sender.send(()).ok();
                        }
                    }
                }
                let guard = OnDrop(dropped_tx.lock().unwrap().take());
                async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            })
            .unwrap()
            .unwrap();
            let socket = socket_path(&root).unwrap();
            let mut stream = UnixStream::connect(&socket).await.unwrap();
            let request = TaskRequest::new(root.join("a.http"), None, None, false);
            let mut message = serde_json::to_vec(&request).unwrap();
            message.push(b'\n');
            stream.write_all(&message).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), started_rx)
                .await
                .expect("the request never reached the server")
                .unwrap();
            drop(listener);
            tokio::time::timeout(Duration::from_secs(5), dropped_rx)
                .await
                .expect("the in-flight run was not cancelled")
                .unwrap();
            let mut buffer = [0_u8; 1];
            let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
                .await
                .expect("the connection was not cancelled");
            assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
            fs::remove_dir_all(&root).ok();
        }

        #[tokio::test]
        async fn a_lost_connection_mid_request_is_an_error_not_a_fallback() {
            let root = env::temp_dir().join(format!("zed-http-task-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let socket = socket_path(&root).unwrap();
            let server = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            let collector = std::thread::spawn(move || {
                use std::io::Read;
                let (mut stream, _) = server.accept().unwrap();
                let mut byte = [0_u8; 1];
                stream.read_exact(&mut byte).ok();
            });
            let big = "x".repeat(32 * 1024 * 1024);
            let request = TaskRequest::new(root.join("a.http"), None, Some(big), false);
            let result = forward(&root, &request).await;
            assert!(
                matches!(&result, Some(Err(error)) if error.contains("while sending")),
                "{result:?}"
            );
            collector.join().unwrap();
            fs::remove_dir_all(&root).ok();
        }

        #[tokio::test]
        async fn runs_stay_on_the_legacy_protocol_while_assists_use_two() {
            let root = env::temp_dir().join(format!("zed-http-task-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let socket = socket_path(&root).unwrap();
            let server = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            let versions = Arc::new(std::sync::Mutex::new(Vec::new()));
            let collected = Arc::clone(&versions);
            let collector = std::thread::spawn(move || {
                use std::io::{BufRead, Write};
                for _ in 0..2 {
                    let (mut stream, _) = server.accept().unwrap();
                    let mut line = String::new();
                    std::io::BufReader::new(&mut stream)
                        .read_line(&mut line)
                        .unwrap();
                    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                    let version = request["version"].as_u64().unwrap();
                    collected.lock().unwrap().push(version);
                    let reply = if version == RUN_PROTOCOL_VERSION as u64 {
                        "{\"status\":\"done\",\"output\":\"legacy\",\"success\":true}\n"
                    } else {
                        "{\"status\":\"unsupported\"}\n"
                    };
                    stream.write_all(reply.as_bytes()).unwrap();
                }
            });
            let request = TaskRequest::new(root.join("a.http"), None, None, false);
            assert_eq!(
                forward(&root, &request).await,
                Some(Ok(TaskOutcome {
                    output: "legacy".to_owned(),
                    success: true
                }))
            );
            let request = request.with_operation(TaskOperation::Hover {
                text: "GET https://x\n".to_owned(),
                position: tower_lsp::lsp_types::Position::new(0, 0),
            });
            assert!(forward(&root, &request).await.is_none());
            collector.join().unwrap();
            assert_eq!(
                *versions.lock().unwrap(),
                vec![RUN_PROTOCOL_VERSION as u64, PROTOCOL_VERSION as u64]
            );
            fs::remove_dir_all(&root).ok();
        }

        #[tokio::test]
        async fn a_failed_request_invokes_the_callback_once() {
            let root = env::temp_dir().join(format!("zed-http-task-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = Arc::clone(&calls);
            let listener = listen(&root, move |_| {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Err("nope".to_owned()) }
            })
            .unwrap()
            .unwrap();
            let request = TaskRequest::new(root.join("a.http"), None, None, false);
            assert_eq!(forward(&root, &request).await, Some(Err("nope".to_owned())));
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            drop(listener);
            assert!(forward(&root, &request).await.is_none());
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            fs::remove_dir_all(&root).ok();
        }
    }
}
