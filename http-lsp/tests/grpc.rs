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
    time::{Duration, Instant},
};

use common::Workspace;
use prost::Message as _;
use prost_reflect::prost_types::{FileDescriptorProto, Timestamp};
use tokio_stream::{wrappers::TcpListenerStream, Stream};
use tonic::{
    body::Body,
    server::{Grpc, NamedService, ServerStreamingService, UnaryService},
    transport::Server,
    Request, Response, Status,
};
use tonic_prost::ProstCodec;
use tonic_reflection::pb::v1::{
    server_reflection_response::MessageResponse,
    server_reflection_server::{ServerReflection, ServerReflectionServer},
    FileDescriptorResponse, ServerReflectionRequest, ServerReflectionResponse,
};
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
    #[prost(int32, repeated, tag = "4")]
    values: Vec<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct StreamRequest {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(int32, tag = "2")]
    count: i32,
}

/// `test.v1.Greeter` from `fixtures/grpc`, written against tonic's server primitives so no
/// code generation (and no `protoc`) is needed. `SayHello` fails for `fail` and answers
/// `authority?` with the call's `:authority`; `StreamGreetings` ends with an error for `error`
/// and sends a million packed zeros per message for `zeros`.
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
            let authority = request
                .extensions()
                .get::<Authority>()
                .map(|authority| authority.0.clone())
                .unwrap_or_default();
            let name = request.into_inner().name;
            if name == "authority?" {
                return Ok(Response::new(HelloReply {
                    message: format!("authority: {authority}"),
                    ..HelloReply::default()
                }));
            }
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
                values: Vec::new(),
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
                        values: if name == "zeros" {
                            vec![0; 1_000_000]
                        } else {
                            Vec::new()
                        },
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

    fn call(&mut self, mut request: http::Request<Body>) -> Self::Future {
        let authority = request
            .uri()
            .authority()
            .map(ToString::to_string)
            .unwrap_or_default();
        request.extensions_mut().insert(Authority(authority));
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

#[derive(Clone)]
struct Authority(String);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reflection {
    V1,
    V1Alpha,
    None,
    /// Answers with more files than the client accepts.
    TooManyFiles,
    /// Answers with a server-sent UNAVAILABLE status.
    Unavailable,
    /// Never answers.
    Hang,
}

/// A misbehaving v1 reflection service.
struct FakeReflection(Reflection);

#[tonic::async_trait]
impl ServerReflection for FakeReflection {
    type ServerReflectionInfoStream =
        Pin<Box<dyn Stream<Item = Result<ServerReflectionResponse, Status>> + Send>>;

    async fn server_reflection_info(
        &self,
        _: Request<tonic::Streaming<ServerReflectionRequest>>,
    ) -> Result<Response<Self::ServerReflectionInfoStream>, Status> {
        match self.0 {
            Reflection::Unavailable => Err(Status::unavailable("reflection is warming up")),
            Reflection::Hang => Ok(Response::new(Box::pin(tokio_stream::pending()))),
            _ => {
                let files = (0..600)
                    .map(|index| {
                        FileDescriptorProto {
                            name: Some(format!("file{index}.proto")),
                            ..FileDescriptorProto::default()
                        }
                        .encode_to_vec()
                    })
                    .collect();
                let response = ServerReflectionResponse {
                    message_response: Some(MessageResponse::FileDescriptorResponse(
                        FileDescriptorResponse {
                            file_descriptor_proto: files,
                        },
                    )),
                    ..ServerReflectionResponse::default()
                };
                Ok(Response::new(Box::pin(tokio_stream::iter([Ok(response)]))))
            }
        }
    }
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
        fake => router = router.add_service(ServerReflectionServer::new(FakeReflection(fake))),
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

fn copy_protos(workspace: &Workspace) {
    let protos = workspace.root.join("api/protos");
    fs::create_dir_all(&protos).unwrap();
    for name in ["greeter.proto", "types.proto"] {
        fs::copy(fixture_dir().join(name), protos.join(name)).unwrap();
    }
}

#[tokio::test]
async fn bounds_the_files_accepted_from_reflection() {
    let address = start_server(Reflection::TooManyFiles).await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = format!("GRPC {address}/test.v1.Greeter/SayHello\n");
    let report = workspace.run(&runner, &text, None).await;
    let rendered = output(&report);
    assert!(
        rendered.contains("server reflection sent more than 512 files"),
        "{rendered}"
    );

    // The .proto fallback still applies.
    copy_protos(&workspace);
    let report = workspace.run(&runner, &text, None).await;
    assert_eq!(report.summary().failed, 0, "{}", output(&report));
}

#[tokio::test]
async fn server_sent_unavailable_from_reflection_still_tries_proto_files() {
    let address = start_server(Reflection::Unavailable).await;
    let workspace = Workspace::new(address);
    copy_protos(&workspace);
    let runner = workspace.runner();
    let text = format!("GRPC {address}/test.v1.Greeter/SayHello\n\n{{\"name\": \"Di\"}}\n");
    let report = workspace.run(&runner, &text, None).await;
    let rendered = output(&report);
    assert_eq!(report.summary().failed, 0, "{rendered}");
    assert!(rendered.contains("Hello, Di"), "{rendered}");
}

#[tokio::test]
async fn timeout_covers_descriptor_lookup() {
    let address = start_server(Reflection::Hang).await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = format!("# @timeout 300ms\nGRPC {address}/test.v1.Greeter/SayHello\n");
    let started = Instant::now();
    let report = workspace.run(&runner, &text, None).await;
    let rendered = output(&report);
    assert!(started.elapsed() < Duration::from_secs(5), "{rendered}");
    assert!(
        rendered.contains("timed out after 0.3 s looking up the service descriptors"),
        "{rendered}"
    );
}

#[tokio::test]
async fn host_header_sets_the_authority() {
    let address = start_server(Reflection::V1).await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = format!(
        "GRPC {address}/test.v1.Greeter/SayHello\nHost: api.internal:9999\n\n{{\"name\": \"authority?\"}}\n"
    );
    let report = workspace.run(&runner, &text, None).await;
    let rendered = output(&report);
    assert!(
        rendered.contains("\"message\": \"authority: api.internal:9999\""),
        "{rendered}"
    );

    let text = format!("GRPC {address}/test.v1.Greeter/SayHello\n\n{{\"name\": \"authority?\"}}\n");
    let report = workspace.run(&runner, &text, None).await;
    let rendered = output(&report);
    assert!(
        rendered.contains(&format!("\"message\": \"authority: {address}\"")),
        "{rendered}"
    );
}

#[tokio::test]
async fn budgets_rendered_json_not_wire_size() {
    let address = start_server(Reflection::V1).await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    // Each message is about 1 MB on the wire but about 9 MB of JSON.
    let text = format!(
        "GRPC {address}/test.v1.Greeter/StreamGreetings\n\n{{\"name\": \"zeros\", \"count\": 6}}\n"
    );
    let report = workspace.run(&runner, &text, None).await;
    let execution = &report.executions[0];
    let error = execution.error.clone().unwrap_or_default();
    assert!(error.contains("response JSON exceeded 32 MiB"), "{error}");
    let body = execution
        .body
        .as_ref()
        .and_then(|body| body.formatted.as_ref());
    assert!(body.is_some_and(|body| body.len() <= 32 * 1024 * 1024 + 16));
}
