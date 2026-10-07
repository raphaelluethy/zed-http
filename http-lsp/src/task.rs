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
const PROTOCOL_VERSION: u32 = 1;
const MAX_HTTP_FILE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRequest {
    pub version: u32,
    pub path: PathBuf,
    /// The 0-based line of the request to send, or `None` for every request.
    pub line: Option<u32>,
    pub environment: Option<String>,
    pub color: bool,
}

impl TaskRequest {
    pub fn new(path: PathBuf, line: Option<u32>, environment: Option<String>, color: bool) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            path,
            line,
            environment,
            color,
        }
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
        os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
        sync::Arc,
    };

    use sha2::{Digest, Sha256};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::{UnixListener, UnixStream},
        task::JoinHandle,
    };

    use super::{TaskOutcome, TaskRequest, TaskResponse, PROTOCOL_VERSION};

    const MAX_REQUEST_BYTES: u64 = 64 * 1024;

    /// A bound socket. Dropping it stops accepting and removes the socket file.
    pub struct Listener {
        path: PathBuf,
        accept: JoinHandle<()>,
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            self.accept.abort();
            fs::remove_file(&self.path).ok();
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
            while let Ok((stream, _)) = listener.accept().await {
                let run = Arc::clone(&run);
                tokio::spawn(async move { serve(stream, run.as_ref()).await });
            }
        });
        Ok(Some(Listener { path, accept }))
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
        let response = match read.map(|_| serde_json::from_str::<TaskRequest>(&line)) {
            Ok(Ok(request)) if request.version == PROTOCOL_VERSION => {
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
        let mut stream = match UnixStream::connect(&path).await {
            Ok(stream) => stream,
            Err(error) => {
                // Left behind by a language server that was killed.
                if error.kind() == ErrorKind::ConnectionRefused {
                    fs::remove_file(&path).ok();
                }
                return None;
            }
        };
        let mut message = serde_json::to_vec(request).ok()?;
        message.push(b'\n');
        stream.write_all(&message).await.ok()?;
        let mut response = Vec::new();
        if let Err(error) = stream.read_to_end(&mut response).await {
            return Some(Err(format!("lost the language server connection: {error}")));
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
    }
}
