#[allow(dead_code)]
mod common;

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

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
/// `X-Token` header the client sent in the handshake. `silent` gets no answer, `flood` starts an
/// endless stream of messages, and connections to `/stall` are never read from.
async fn start_echo_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut token = String::from("none");
                let mut stall = false;
                // The error type is fixed by tungstenite's `Callback` trait.
                #[allow(clippy::result_large_err)]
                let callback = |request: &Request, response: Response| {
                    if let Some(value) = request.headers().get("x-token") {
                        token = value.to_str().unwrap().to_owned();
                    }
                    stall = request.uri().path() == "/stall";
                    Ok(response)
                };
                let Ok(mut socket) = accept_hdr_async(stream, callback).await else {
                    return;
                };
                if stall {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    return;
                }
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
                        "silent" => continue,
                        "flood" => loop {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            if socket.send(Message::text("more")).await.is_err() {
                                return;
                            }
                        },
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
# @timeout 1s
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
        "\n← late\n→ token?\n→ {\n    \"multi\": true\n  }\n← token: env-token\n← echo: {\n    \"multi\": true\n  }\n# stopped listening after 1 s\n",
        "{rendered}"
    );
}

#[tokio::test]
async fn without_waiting_messages_are_sent_back_to_back() {
    let address = start_echo_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = format!("# @timeout 1s\nWEBSOCKET ws://{address}\n\ndelay\n===\nnext\n");
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

#[tokio::test]
async fn an_unmet_wait_for_server_fails_the_request() {
    let address = start_echo_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = format!(
        "# @timeout 300ms\nWEBSOCKET ws://{address}\n\nsilent\n=== wait-for-server\nnever sent\n"
    );
    let report = workspace.run(&runner, &text, None).await;
    let rendered = report.render(OutputView::Full);
    assert_eq!(report.summary().failed, 1, "{rendered}");
    assert!(
        rendered.contains("wait-for-server not satisfied: no server message arrived within 0.3 s; 1 message was not sent"),
        "{rendered}"
    );
    assert!(rendered.contains("→ silent\n"), "{rendered}");
    assert!(!rendered.contains("→ never sent"), "{rendered}");
}

#[tokio::test]
async fn an_explicit_timeout_shortens_the_overall_cap() {
    let address = start_echo_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = format!("# @timeout 400ms\nWEBSOCKET ws://{address}\n\nflood\n");
    let started = Instant::now();
    let report = workspace.run(&runner, &text, None).await;
    let rendered = report.render(OutputView::Full);
    assert!(started.elapsed() < Duration::from_secs(5), "{rendered}");
    assert_eq!(report.summary().failed, 0, "{rendered}");
    assert!(rendered.contains("← more\n"), "{rendered}");
    assert!(
        rendered.contains("# stopped listening after 0.4 s"),
        "{rendered}"
    );
}

#[tokio::test]
async fn sending_to_a_server_that_stops_reading_times_out() {
    let address = start_echo_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    // Large enough to fill the socket buffers of a peer that never reads.
    let message = "x".repeat(24 * 1024 * 1024);
    let text = format!("# @timeout 1s\nWEBSOCKET ws://{address}/stall\n\n{message}\n");
    let started = Instant::now();
    let report = workspace.run(&runner, &text, None).await;
    let error = report.executions[0].error.clone().unwrap_or_default();
    assert!(started.elapsed() < Duration::from_secs(10), "{error}");
    assert!(error.contains("sending a message"), "{error}");
}
