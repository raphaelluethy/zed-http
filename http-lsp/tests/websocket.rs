#[allow(dead_code)]
mod common;

use std::{net::SocketAddr, time::Duration};

use common::Workspace;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{Request, Response},
        Message,
    },
};
use zed_http_lsp::report::OutputView;

/// Echoes text messages. `delay` answers `late` after 300 ms, and `token?` answers with the
/// `X-Token` header the client sent in the handshake.
async fn start_echo_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut token = String::from("none");
                // The error type is fixed by tungstenite's `Callback` trait.
                #[allow(clippy::result_large_err)]
                let callback = |request: &Request, response: Response| {
                    if let Some(value) = request.headers().get("x-token") {
                        token = value.to_str().unwrap().to_owned();
                    }
                    Ok(response)
                };
                let Ok(mut socket) = accept_hdr_async(stream, callback).await else {
                    return;
                };
                while let Some(Ok(message)) = socket.next().await {
                    let Message::Text(text) = message else {
                        continue;
                    };
                    let reply = match text.as_str() {
                        "delay" => {
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            "late".to_owned()
                        }
                        "token?" => format!("token: {token}"),
                        other => format!("echo: {other}"),
                    };
                    if socket.send(Message::text(reply)).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    address
}

#[tokio::test]
async fn exchanges_messages_and_waits_for_the_server() {
    let address = start_echo_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = format!(
        "\
# @timeout 500ms
WEBSOCKET ws://{address}/socket
X-Token: {{{{token}}}}

===
delay
=== wait-for-server
token?
===
{{
  \"multi\": true
}}
"
    );
    let report = workspace.run(&runner, &text, None).await;
    let rendered = report.render(OutputView::Full);
    assert_eq!(report.summary().failed, 0, "{rendered}");
    assert_eq!(report.executions[0].status, Some(101));
    let transcript = rendered
        .split_once("→ delay")
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| panic!("{rendered}"));
    assert_eq!(
        transcript,
        "\n← late\n→ token?\n→ {\n    \"multi\": true\n  }\n← token: env-token\n← echo: {\n    \"multi\": true\n  }\n",
        "{rendered}"
    );
}

#[tokio::test]
async fn without_waiting_messages_are_sent_back_to_back() {
    let address = start_echo_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = format!("# @timeout 600ms\nWEBSOCKET ws://{address}\n\ndelay\n===\nnext\n");
    let report = workspace.run(&runner, &text, None).await;
    let rendered = report.render(OutputView::Full);
    assert!(
        rendered.contains("→ delay\n→ next\n← late\n← echo: next\n"),
        "{rendered}"
    );
}

#[tokio::test]
async fn reports_refused_upgrades_and_connection_failures() {
    let http = common::start_server().await;
    let workspace = Workspace::new(http);
    let runner = workspace.runner();
    let report = workspace
        .run(&runner, "WEBSOCKET {{baseUrl}}/json\n", None)
        .await;
    let rendered = report.render(OutputView::Full);
    assert_eq!(report.summary().failed, 1, "{rendered}");
    assert!(
        rendered.contains("refused the WebSocket upgrade"),
        "{rendered}"
    );

    let unused = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let report = workspace
        .run(&runner, &format!("WEBSOCKET ws://{unused}\n\nhi\n"), None)
        .await;
    let rendered = report.render(OutputView::Full);
    assert!(rendered.contains("error: failed to connect"), "{rendered}");
}
