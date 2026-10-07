use std::io::{IsTerminal, Write};
use tower_lsp::{LspService, Server};
use zed_http_lsp::{
    backend::Backend,
    runner::Runner,
    script,
    session::Session,
    task::{self, TaskRequest},
    terminal,
};

fn main() {
    // Scripts run in short-lived copies of this executable so they can be killed.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == script::WORKER_FLAG)
    {
        std::process::exit(script::worker_main());
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start the async runtime")
        .block_on(async {
            if std::env::args_os().len() > 1 {
                match run_request().await {
                    Ok(true) => {}
                    Ok(false) => std::process::exit(1),
                    Err(error) => {
                        eprintln!("zed-http: {error}");
                        std::process::exit(1);
                    }
                }
            } else {
                serve().await;
            }
        });
}

/// Task rows are one-based; the native runner selects requests by zero-based line.
async fn run_request() -> Result<bool, String> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--run")) {
        return Err("usage: zed-http-lsp --run FILE [--line ROW]".into());
    }
    let path = args.next().ok_or("--run requires a file path")?;
    let path = std::path::PathBuf::from(path);
    let line = match args.next() {
        None => None,
        Some(flag) if flag == "--line" => {
            let row = args.next().ok_or("--line requires a positive row number")?;
            let row = row
                .to_str()
                .and_then(|row| row.parse::<u32>().ok())
                .filter(|row| *row > 0)
                .ok_or("--line requires a positive row number")?;
            Some(row - 1)
        }
        Some(_) => return Err("usage: zed-http-lsp --run FILE [--line ROW]".into()),
    };
    if args.next().is_some() {
        return Err("unexpected argument after the request selection".into());
    }
    let root = std::env::current_dir().map_err(|error| error.to_string())?;
    let request = TaskRequest::new(
        std::path::absolute(&path).map_err(|error| error.to_string())?,
        line,
        std::env::var("ZED_HTTP_ENV").ok(),
        terminal::colors_enabled(std::io::stdout().is_terminal()),
    );
    // Zed runs tasks in the worktree root, which identifies its language server.
    let outcome = match task::forward(&root, &request).await {
        Some(outcome) => outcome?,
        None => {
            if cfg!(unix) {
                eprintln!(
                    "zed-http: the HTTP language server is not running, so this run starts \
                     without the globals and cookies of earlier runs"
                );
            }
            let runner = Runner::new(Session::new());
            runner.set_workspace_roots(vec![root]);
            task::execute(&runner, &request).await?
        }
    };
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(outcome.output.as_bytes())
        .map_err(|error| error.to_string())?;
    stdout.flush().map_err(|error| error.to_string())?;
    Ok(outcome.success)
}

async fn serve() {
    let (service, socket) = LspService::new(Backend::new);
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
}
