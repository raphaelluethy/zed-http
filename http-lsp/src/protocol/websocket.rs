//! WebSocket over tokio-tungstenite. The body is split into messages on `===` lines; a
//! `=== wait-for-server` line makes the next message wait for one server message first. After
//! the last message is sent, server messages are collected until the connection has been idle
//! for 2 s, within an overall cap of 30 s for the whole exchange. An explicit `@timeout` replaces
//! both: the exchange then ends after that long, or after that long without a server message.
//! Collection is also bounded by a message count and a byte budget. A `wait-for-server` that is
//! never satisfied, or messages left unsent, fail the request. The exchange renders as a `→`/`←`
//! transcript.

use std::{
    fmt::Write as _,
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use tokio::time::timeout_at;
use tokio_tungstenite::{
    client_async_with_config,
    tungstenite::{
        client::IntoClientRequest,
        http::{HeaderName, HeaderValue},
        protocol::{frame::coding::CloseCode, CloseFrame, WebSocketConfig},
        Error as WsError, Message,
    },
    WebSocketStream,
};

use super::{
    error_chain,
    net::{self, Alpn, BoxedIo, DEFAULT_CONNECT_TIMEOUT},
    Context, PreparedBody, PreparedRequest, Response, MAX_BODY_BYTES,
};

const DEFAULT_IDLE: Duration = Duration::from_secs(2);
const DEFAULT_CAP: Duration = Duration::from_secs(30);
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// Server messages kept per request; collection stops after this many.
const MAX_MESSAGES: usize = 1000;

/// One outgoing message and how many server messages to wait for before sending it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Frame {
    waits: usize,
    text: String,
}

/// Splits a body into messages. Returns the messages and the number of `wait-for-server`
/// markers after the last one.
fn parse_frames(body: &str) -> (Vec<Frame>, usize) {
    let mut frames = Vec::new();
    let mut waits = 0;
    let mut lines: Vec<&str> = Vec::new();
    let mut flush = |lines: &mut Vec<&str>, waits: &mut usize| {
        let text = lines.join("\n");
        lines.clear();
        let text = text.trim_matches(|c| c == '\n' || c == '\r');
        if !text.trim().is_empty() {
            frames.push(Frame {
                waits: std::mem::take(waits),
                text: text.to_owned(),
            });
        }
    };
    for line in body.lines() {
        let trimmed = line.trim();
        let separator = trimmed.strip_prefix("===").map(str::trim);
        let separator = separator.map(|rest| {
            rest.split_once("//")
                .map_or(rest, |(before, _)| before)
                .trim()
        });
        match separator {
            Some("") => flush(&mut lines, &mut waits),
            Some("wait-for-server") => {
                flush(&mut lines, &mut waits);
                waits += 1;
            }
            _ => lines.push(line),
        }
    }
    flush(&mut lines, &mut waits);
    (frames, waits)
}

#[derive(Default)]
struct Transcript {
    text: String,
    received: usize,
    received_bytes: usize,
}

impl Transcript {
    fn push(&mut self, arrow: &str, message: &str) {
        let mut lines = message.lines();
        let _ = writeln!(self.text, "{arrow} {}", lines.next().unwrap_or_default());
        for line in lines {
            let _ = writeln!(self.text, "  {line}");
        }
    }
}

/// Why reading from the server stopped.
enum Stop {
    /// The idle window or the overall cap elapsed.
    TimedOut,
    /// The server closed the connection (with a close frame or not).
    Closed,
    Failed(String),
}

/// Reads until one data message arrives (recording it) or `deadline` passes. Pings and pongs
/// are answered by tungstenite and not recorded.
async fn receive(
    socket: &mut WebSocketStream<BoxedIo>,
    transcript: &mut Transcript,
    deadline: tokio::time::Instant,
) -> Result<(), Stop> {
    loop {
        let message = match timeout_at(deadline, socket.next()).await {
            Err(_) => return Err(Stop::TimedOut),
            Ok(None) => return Err(Stop::Closed),
            Ok(Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed))) => {
                return Err(Stop::Closed)
            }
            Ok(Some(Err(error))) => {
                return Err(Stop::Failed(format!(
                    "WebSocket error: {}",
                    error_chain(&error)
                )))
            }
            Ok(Some(Ok(message))) => message,
        };
        let size = match &message {
            Message::Text(text) => text.len(),
            Message::Binary(bytes) => bytes.len(),
            _ => 0,
        };
        match message {
            Message::Text(text) => transcript.push("←", text.as_str()),
            Message::Binary(bytes) => {
                transcript.push("←", &format!("<binary {} bytes>", bytes.len()))
            }
            Message::Close(frame) => {
                let detail = match frame {
                    Some(frame) if frame.reason.is_empty() => format!(" {}", frame.code),
                    Some(frame) => format!(" {} {}", frame.code, frame.reason),
                    None => String::new(),
                };
                transcript.push("←", &format!("<close{detail}>"));
                return Err(Stop::Closed);
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
        }
        transcript.received += 1;
        transcript.received_bytes += size;
        if transcript.received >= MAX_MESSAGES {
            return Err(Stop::Failed(format!(
                "stopped after receiving {MAX_MESSAGES} messages"
            )));
        }
        if transcript.received_bytes > MAX_BODY_BYTES {
            return Err(Stop::Failed(format!(
                "stopped after receiving more than {} MiB",
                MAX_BODY_BYTES / 1024 / 1024
            )));
        }
        return Ok(());
    }
}

pub async fn send(request: &PreparedRequest, _context: &Context<'_>) -> Result<Response, String> {
    let mut url = Url::parse(&request.url)
        .map_err(|error| format!("invalid URL {:?}: {error}", request.url))?;
    let tls = match url.scheme() {
        "ws" | "http" => false,
        "wss" | "https" => true,
        other => return Err(format!("unsupported WebSocket scheme {other:?}")),
    };
    // `set_scheme` refuses some special-scheme changes, so rebuild from text when needed.
    if matches!(url.scheme(), "http" | "https") {
        let rest = &request.url[url.scheme().len()..];
        url = Url::parse(&format!("{}{rest}", if tls { "wss" } else { "ws" }))
            .map_err(|error| format!("invalid URL {:?}: {error}", request.url))?;
    }
    let host = url
        .host_str()
        .ok_or_else(|| format!("URL {:?} has no host", request.url))?
        .to_owned();
    let port = url
        .port_or_known_default()
        .unwrap_or(if tls { 443 } else { 80 });

    let mut handshake = url
        .as_str()
        .into_client_request()
        .map_err(|error| format!("invalid WebSocket request: {}", error_chain(&error)))?;
    for header in &request.headers {
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| format!("invalid header name {:?}", header.name))?;
        let value = HeaderValue::from_str(&header.value)
            .map_err(|_| format!("invalid value for header {}", header.name))?;
        handshake.headers_mut().insert(name, value);
    }

    let body = match &request.body {
        PreparedBody::Empty => String::new(),
        PreparedBody::Bytes(bytes) => String::from_utf8(bytes.clone())
            .map_err(|_| "WebSocket messages must be UTF-8 text".to_owned())?,
    };
    let (frames, trailing_waits) = parse_frames(&body);

    let directives = &request.directives;
    let idle = directives.timeout.unwrap_or(DEFAULT_IDLE);
    let cap = directives.timeout.unwrap_or(DEFAULT_CAP);
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + cap;

    let stream = timeout_at(
        deadline,
        net::connect(
            &host,
            port,
            tls.then_some(Alpn::Http1),
            directives
                .connection_timeout
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT),
        ),
    )
    .await
    .map_err(|_| format!("timed out connecting to {host}:{port}"))??;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_BODY_BYTES))
        .max_frame_size(Some(MAX_BODY_BYTES));
    let handshake = timeout_at(
        deadline,
        client_async_with_config(handshake, stream, Some(config)),
    )
    .await
    .map_err(|_| "timed out during the WebSocket handshake".to_owned())?;
    let (mut socket, handshake_response) = match handshake {
        Ok(result) => result,
        Err(WsError::Http(response)) => {
            let status = response.status();
            let body = response.body().clone().unwrap_or_default();
            return Ok(Response {
                success: false,
                status: Some(status.as_u16()),
                status_line: format!("{:?}", response.version()),
                url: request.url.clone(),
                headers: header_pairs(response.headers()),
                content_type: response
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
                body,
                formatted: None,
                elapsed: started.elapsed(),
                error: Some(format!(
                    "the server refused the WebSocket upgrade with HTTP {status}"
                )),
            });
        }
        Err(error) => {
            return Err(format!(
                "WebSocket handshake failed: {}",
                error_chain(&error)
            ))
        }
    };

    let mut transcript = Transcript::default();
    let mut error = None;
    let mut closed = false;
    // Messages are sent and awaited first; any failure here fails the request.
    'frames: for (sent, frame) in frames.iter().enumerate() {
        for _ in 0..frame.waits {
            if let Err(stop) = receive(&mut socket, &mut transcript, deadline).await {
                closed = matches!(stop, Stop::Closed);
                error = Some(unmet_wait(stop, frames.len() - sent, cap));
                break 'frames;
            }
        }
        match timeout_at(deadline, socket.send(Message::text(frame.text.clone()))).await {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => {
                error = Some(format!(
                    "failed to send a message: {}",
                    error_chain(&failure)
                ));
                break;
            }
            Err(_) => {
                error = Some(format!(
                    "timed out after {} sending a message; the server is not reading",
                    seconds(cap)
                ));
                break;
            }
        }
        transcript.push("→", &frame.text);
    }
    if error.is_none() {
        for _ in 0..trailing_waits {
            if let Err(stop) = receive(&mut socket, &mut transcript, deadline).await {
                closed = matches!(stop, Stop::Closed);
                error = Some(unmet_wait(stop, 0, cap));
                break;
            }
        }
    }
    // Then whatever else the server sends is collected until it goes quiet.
    if error.is_none() {
        loop {
            let idle_deadline = (tokio::time::Instant::now() + idle).min(deadline);
            match receive(&mut socket, &mut transcript, idle_deadline).await {
                Ok(()) => {}
                Err(Stop::Closed) => {
                    closed = true;
                    break;
                }
                Err(Stop::Failed(message)) => {
                    error = Some(message);
                    break;
                }
                Err(Stop::TimedOut) => {
                    if tokio::time::Instant::now() >= deadline {
                        transcript
                            .text
                            .push_str(&format!("# stopped listening after {}\n", seconds(cap)));
                    }
                    break;
                }
            }
        }
    }
    if !closed {
        close(&mut socket).await;
    }

    Ok(Response {
        success: error.is_none(),
        status: Some(handshake_response.status().as_u16()),
        status_line: format!("{:?}", handshake_response.version()),
        url: request.url.clone(),
        headers: header_pairs(handshake_response.headers()),
        content_type: None,
        body: transcript.text.clone().into_bytes(),
        formatted: Some(if transcript.text.is_empty() {
            "<no messages exchanged>\n".to_owned()
        } else {
            transcript.text
        }),
        elapsed: started.elapsed(),
        error,
    })
}

/// The error for a `wait-for-server` that was not satisfied.
fn unmet_wait(stop: Stop, unsent: usize, cap: Duration) -> String {
    let reason = match stop {
        Stop::Failed(message) => return message,
        Stop::TimedOut => format!("no server message arrived within {}", seconds(cap)),
        Stop::Closed => "the server closed the connection".to_owned(),
    };
    match unsent {
        0 => format!("wait-for-server not satisfied: {reason}"),
        1 => format!("wait-for-server not satisfied: {reason}; 1 message was not sent"),
        n => format!("wait-for-server not satisfied: {reason}; {n} messages were not sent"),
    }
}

fn seconds(duration: Duration) -> String {
    format!("{} s", duration.as_secs_f64())
}

/// Sends a close frame and waits briefly for the server to acknowledge it.
async fn close(socket: &mut WebSocketStream<BoxedIo>) {
    let deadline = tokio::time::Instant::now() + CLOSE_GRACE;
    let frame = CloseFrame {
        code: CloseCode::Normal,
        reason: "".into(),
    };
    if timeout_at(deadline, socket.close(Some(frame)))
        .await
        .is_err()
    {
        return;
    }
    while let Ok(Some(Ok(_))) = timeout_at(deadline, socket.next()).await {}
}

fn header_pairs(
    headers: &tokio_tungstenite::tungstenite::http::HeaderMap,
) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_messages_and_wait_markers() {
        let body = "===\n{\"a\": 1}\n=== wait-for-server\n=== wait-for-server\nsecond\nline\n===\n\nthird\n=== wait-for-server\n";
        let (frames, trailing) = parse_frames(body);
        assert_eq!(
            frames,
            vec![
                Frame {
                    waits: 0,
                    text: "{\"a\": 1}".to_owned()
                },
                Frame {
                    waits: 2,
                    text: "second\nline".to_owned()
                },
                Frame {
                    waits: 0,
                    text: "third".to_owned()
                },
            ]
        );
        assert_eq!(trailing, 1);
        assert_eq!(
            parse_frames("hello"),
            (
                vec![Frame {
                    waits: 0,
                    text: "hello".to_owned()
                }],
                0
            )
        );
        assert_eq!(parse_frames(""), (Vec::new(), 0));
    }

    #[test]
    fn separator_comments_are_ignored_but_messages_keep_slashes() {
        let body = "=== // first\ndelay\n=== wait-for-server // hold\nhttps://example.test/a//b\n\
                    {\"u\": \"a//b\"}\n=== custom // note\nkept\n===\nlast // one\n";
        let (frames, trailing) = parse_frames(body);
        assert_eq!(
            frames,
            vec![
                Frame {
                    waits: 0,
                    text: "delay".to_owned()
                },
                Frame {
                    waits: 1,
                    text: "https://example.test/a//b\n{\"u\": \"a//b\"}\n=== custom // note\nkept"
                        .to_owned()
                },
                Frame {
                    waits: 0,
                    text: "last // one".to_owned()
                },
            ]
        );
        assert_eq!(trailing, 0);
        assert_eq!(
            parse_frames("a\n===wait-for-server\nb"),
            (
                vec![
                    Frame {
                        waits: 0,
                        text: "a".to_owned()
                    },
                    Frame {
                        waits: 1,
                        text: "b".to_owned()
                    },
                ],
                0
            )
        );
        assert_eq!(
            parse_frames("x\n=== wait-for-server // a\n=== wait-for-server // b\ny").0[1].waits,
            2
        );
    }

    #[test]
    fn renders_multiline_messages() {
        let mut transcript = Transcript::default();
        transcript.push("→", "{\n  \"a\": 1\n}");
        transcript.push("←", "ok");
        assert_eq!(transcript.text, "→ {\n    \"a\": 1\n  }\n← ok\n");
    }
}
