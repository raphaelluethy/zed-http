mod backend;
mod core;

use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
    let (service, socket) = LspService::new(backend::Backend::new);
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
}
