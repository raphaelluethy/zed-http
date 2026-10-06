#[allow(dead_code)]
mod common;

use std::{
    convert::Infallible,
    fs,
    future::Future,
    net::SocketAddr,
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
};

use common::Workspace;
use prost_reflect::prost_types::Timestamp;
use tokio_stream::{wrappers::TcpListenerStream, Stream};
use tonic::{
    body::Body,
    server::{Grpc, NamedService, ServerStreamingService, UnaryService},
    transport::Server,
    Request, Response, Status,
};
use tonic_prost::ProstCodec;
use zed_http_lsp::report::{OutputView, Report};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

#[derive(Clone, PartialEq, prost::Message)]
struct HelloRequest {
    #[prost(string, tag = "1")]
    name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct HelloReply {
    #[prost(string, tag = "1")]
    message: String,
    #[prost(int32, tag = "2")]
    index: i32,
    #[prost(message, optional, tag = "3")]
    at: Option<Timestamp>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct StreamRequest {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(int32, tag = "2")]
    count: i32,
}

/// `test.v1.Greeter` from `fixtures/grpc`, written against tonic's server primitives so no
/// code generation (and no `protoc`) is needed.
#[derive(Clone)]
struct Greeter;

impl NamedService for Greeter {
    const NAME: &'static str = "test.v1.Greeter";
}

struct SayHello;

impl UnaryService<HelloRequest> for SayHello {
    type Response = HelloReply;
    type Future = BoxFuture<Result<Response<HelloReply>, Status>>;

    fn call(&mut self, request: Request<HelloRequest>) -> Self::Future {
        Box::pin(async move {
            let token = request
                .metadata()
                .get("x-token")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("none")
                .to_owned();
            let name = request.into_inner().name;
            if name == "fail" {
                return Err(Status::not_found(format!("no greeting for {name}")));
            }
            let mut response = Response::new(HelloReply {
                message: format!("Hello, {name} (token: {token})"),
                index: 0,
                at: Some(Timestamp {
                    seconds: 1,
                    nanos: 0,
                }),
            });
            response
                .metadata_mut()
                .insert("x-served-by", "greeter".parse().unwrap());
            Ok(response)
        })
    }
}

struct StreamGreetings;

impl ServerStreamingService<StreamRequest> for StreamGreetings {
    type Response = HelloReply;
    type ResponseStream = Pin<Box<dyn Stream<Item = Result<HelloReply, Status>> + Send>>;
    type Future = BoxFuture<Result<Response<Self::ResponseStream>, Status>>;

    fn call(&mut self, request: Request<StreamRequest>) -> Self::Future {
        Box::pin(async move {
            let StreamRequest { name, count } = request.into_inner();
            let mut items: Vec<Result<HelloReply, Status>> = (1..=count)
                .map(|index| {
                    Ok(HelloReply {
                        message: format!("Hello #{index}, {name}"),
                        index,
                        at: None,
                    })
                })
                .collect();
            if name == "error" {
                items.push(Err(Status::resource_exhausted("enough greetings")));
            }
            let stream: Self::ResponseStream = Box::pin(tokio_stream::iter(items));
            Ok(Response::new(stream))
        })
    }
}

impl tower::Service<http::Request<Body>> for Greeter {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        Box::pin(async move {
            Ok(match request.uri().path() {
                "/test.v1.Greeter/SayHello" => {
                    Grpc::new(ProstCodec::default())
                        .unary(SayHello, request)
                        .await
                }
                "/test.v1.Greeter/StreamGreetings" => {
                    Grpc::new(ProstCodec::default())
                        .server_streaming(StreamGreetings, request)
                        .await
                }
                _ => Status::unimplemented("unknown method").into_http(),
            })
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reflection {
    V1,
    V1Alpha,
    None,
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grpc")
}

async fn start_server(reflection: Reflection) -> SocketAddr {
    let descriptors = protox::compile(["greeter.proto"], [fixture_dir()]).unwrap();
    let builder =
        tonic_reflection::server::Builder::configure().register_file_descriptor_set(descriptors);
    let mut router = Server::builder().add_service(Greeter);
    match reflection {
        Reflection::V1 => router = router.add_service(builder.build_v1().unwrap()),
        Reflection::V1Alpha => router = router.add_service(builder.build_v1alpha().unwrap()),
        Reflection::None => {}
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(router.serve_with_incoming(TcpListenerStream::new(listener)));
    address
}

fn output(report: &Report) -> String {
    report.render(OutputView::Full)
}

#[tokio::test]
async fn calls_unary_methods_through_reflection() {
    for reflection in [Reflection::V1, Reflection::V1Alpha] {
        let address = start_server(reflection).await;
        let workspace = Workspace::new(address);
        let runner = workspace.runner();
        let text = format!(
            "GRPC {address}/test.v1.Greeter/SayHello\nX-Token: {{{{token}}}}\n\n{{\"name\": \"Ada\"}}\n"
        );
        let report = workspace.run(&runner, &text, None).await;
        let rendered = output(&report);
        assert_eq!(report.summary().failed, 0, "{rendered}");
        assert!(rendered.contains("\ngRPC 0 OK\n"), "{rendered}");
        assert!(rendered.contains("x-served-by: greeter"), "{rendered}");
        let body = report.executions[0]
            .body
            .as_ref()
            .unwrap()
            .content
            .clone()
            .unwrap();
        assert_eq!(body["message"], "Hello, Ada (token: env-token)");
        assert_eq!(body["at"], "1970-01-01T00:00:01Z");
        assert_eq!(body["index"], 0, "default fields are shown");
    }
}

#[tokio::test]
async fn falls_back_to_proto_files_without_reflection() {
    let address = start_server(Reflection::None).await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = format!("GRPC grpc://{address}/test.v1.Greeter/SayHello\n\n{{\"name\": \"Bob\"}}\n");

    // Without any .proto file the error explains both sources.
    let report = workspace.run(&runner, &text, None).await;
    let rendered = output(&report);
    assert_eq!(report.summary().failed, 1, "{rendered}");
    assert!(
        rendered.contains("the server does not implement reflection"),
        "{rendered}"
    );
    assert!(rendered.contains("no .proto file"), "{rendered}");

    let protos = workspace.root.join("api/protos");
    fs::create_dir_all(&protos).unwrap();
    for name in ["greeter.proto", "types.proto"] {
        fs::copy(fixture_dir().join(name), protos.join(name)).unwrap();
    }
    let report = workspace.run(&runner, &text, None).await;
    let rendered = output(&report);
    assert_eq!(report.summary().failed, 0, "{rendered}");
    assert!(
        rendered.contains("\"message\": \"Hello, Bob (token: none)\""),
        "{rendered}"
    );
}

#[tokio::test]
async fn reports_non_ok_statuses_and_bad_requests() {
    let address = start_server(Reflection::V1).await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = format!(
        "\
GRPC {address}/test.v1.Greeter/SayHello

{{\"name\": \"fail\"}}

###
GRPC {address}/test.v1.Greeter/SayHello

{{\"nope\": 1}}

###
GRPC {address}/test.v1.Greeter/Missing

###
GRPC {address}/test.v1.Greeter/Chat
"
    );
    let report = workspace.run(&runner, &text, None).await;
    let rendered = output(&report);
    assert_eq!(report.summary().failed, 4, "{rendered}");
    let errors: Vec<&str> = report
        .executions
        .iter()
        .map(|execution| execution.error.as_deref().unwrap_or_default())
        .collect();
    assert_eq!(errors[0], "gRPC 5 NOT_FOUND: no greeting for fail");
    assert!(
        errors[1].contains("not a valid test.v1.HelloRequest"),
        "{rendered}"
    );
    assert!(errors[2].contains("has no method Missing"), "{rendered}");
    assert!(errors[3].contains("bidirectional"), "{rendered}");
}

#[tokio::test]
async fn reads_server_streams() {
    let address = start_server(Reflection::V1).await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = format!(
        "\
GRPC {address}/test.v1.Greeter/StreamGreetings

{{\"name\": \"Cy\", \"count\": 3}}

###
GRPC {address}/test.v1.Greeter/StreamGreetings

{{\"name\": \"error\", \"count\": 1}}
"
    );
    let report = workspace.run(&runner, &text, None).await;
    let rendered = output(&report);

    let complete = &report.executions[0];
    assert!(!complete.failed(), "{rendered}");
    let messages = complete.body.as_ref().unwrap().content.clone().unwrap();
    assert_eq!(messages.as_array().unwrap().len(), 3, "{rendered}");
    assert_eq!(messages[2]["message"], "Hello #3, Cy");
    assert_eq!(messages[2]["index"], 3);

    let failed = &report.executions[1];
    assert!(failed.failed(), "{rendered}");
    assert_eq!(
        failed.error.as_deref(),
        Some("gRPC 8 RESOURCE_EXHAUSTED: enough greetings")
    );
    assert!(
        rendered.contains("\"message\": \"Hello #1, error\""),
        "partial messages are kept: {rendered}"
    );
}

#[tokio::test]
async fn reports_unreachable_servers() {
    let unused = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let workspace = Workspace::new(unused);
    let runner = workspace.runner();
    let report = workspace
        .run(
            &runner,
            &format!("GRPC {unused}/test.v1.Greeter/SayHello\n"),
            None,
        )
        .await;
    let rendered = output(&report);
    assert_eq!(report.summary().failed, 1, "{rendered}");
    assert!(rendered.contains("failed to connect"), "{rendered}");
}
