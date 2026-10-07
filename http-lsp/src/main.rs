use std::io::{IsTerminal, Write};
use tower_lsp::{LspService, Server};
use zed_http_lsp::{backend::Backend, runner::Runner, script, session::Session, terminal};

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
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|error| error.to_string())?;
    let mut text = String::new();
    use tokio::io::AsyncReadExt;
    file.take(16 * 1024 * 1024 + 1)
        .read_to_string(&mut text)
        .await
        .map_err(|error| error.to_string())?;
    if text.len() > 16 * 1024 * 1024 {
        return Err("HTTP file is larger than 16 MiB".into());
    }
    let runner = Runner::new(Session::new());
    runner.set_workspace_roots(vec![
        std::env::current_dir().map_err(|error| error.to_string())?
    ]);
    let environment = std::env::var("ZED_HTTP_ENV").ok();
    let report = runner
        .run(&path, &text, line, environment.as_deref())
        .await?;
    let color = terminal::colors_enabled(std::io::stdout().is_terminal());
    let output = terminal::render(&report, color);
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(output.as_bytes())
        .map_err(|error| error.to_string())?;
    stdout.flush().map_err(|error| error.to_string())?;
    let summary = report.summary();
    if summary.executed == 0 {
        return Err("no requests were executed".into());
    }
    Ok(summary.failed == 0)
}

async fn serve() {
    let (service, socket) = LspService::new(Backend::new);
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
}
