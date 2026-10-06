use tower_lsp::{LspService, Server};
use zed_http_lsp::{backend::Backend, script};

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
        .block_on(serve());
}

async fn serve() {
    let (service, socket) = LspService::new(Backend::new);
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
}
