//! gRPC over tonic without generated stubs: `GRPC host:port/package.Service/Method` with a JSON
//! body. Descriptors come from server reflection (v1, then v1alpha); when the server does not
//! offer it, `.proto` files in the workspace are compiled with protox. Messages are
//! `DynamicMessage`s carried by a small codec, so any service can be called. Unary and
//! server-streaming methods are supported.
//!
//! One deadline covers the whole request (connecting, descriptor discovery and the call):
//! `@timeout`, or 5 minutes. Server streams are additionally read for at most 30 s unless
//! `@timeout` is given. Reflection data, decoded messages and the rendered JSON are each bounded.

use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use base64::Engine as _;
use hyper_util::rt::TokioIo;
use prost::Message as _;
use prost_reflect::{
    prost_types::FileDescriptorProto, DescriptorPool, DynamicMessage, MessageDescriptor,
    MethodDescriptor, SerializeOptions,
};
use tokio::time::timeout_at;
use tonic::{
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
    metadata::{AsciiMetadataValue, BinaryMetadataValue, MetadataKey, MetadataMap},
    transport::{Channel, Endpoint, Uri},
    Code, Status,
};

use super::{
    error_chain,
    net::{self, Alpn, DEFAULT_CONNECT_TIMEOUT},
    Context, PreparedBody, PreparedRequest, Response, DEFAULT_TIMEOUT, MAX_BODY_BYTES,
};

/// How long a server stream is read unless `@timeout` says otherwise.
const DEFAULT_STREAM_WINDOW: Duration = Duration::from_secs(30);
/// Messages kept from a server stream; reading stops after this many.
const MAX_MESSAGES: usize = 1000;
/// Largest response message accepted on the wire (the usual gRPC default). Decoding expands
/// messages, so this stays well below the rendered-JSON budget.
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
/// Distinct files accepted from server reflection.
const MAX_REFLECTED_FILES: usize = 512;
/// Descriptor bytes accepted from server reflection, across all of its responses.
const MAX_REFLECTED_BYTES: usize = 16 * 1024 * 1024;
/// Bounds for the `.proto` search.
const MAX_SCAN_DEPTH: usize = 8;
const MAX_SCAN_ENTRIES: usize = 20_000;
const MAX_PROTO_BYTES: u64 = 4 * 1024 * 1024;

/// A parsed `GRPC` request line.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    tls: bool,
    authority: String,
    service: String,
    method: String,
}

impl Target {
    fn parse(url: &str) -> Result<Self, String> {
        let url = url.trim();
        let (tls, rest) = match url.split_once("://") {
            Some((scheme, rest)) => match scheme.to_ascii_lowercase().as_str() {
                "grpcs" | "https" => (true, rest),
                "grpc" | "http" => (false, rest),
                other => return Err(format!("unsupported gRPC scheme {other:?}")),
            },
            None => (false, url),
        };
        let invalid = || format!("expected `host:port/package.Service/Method`, got {url:?}");
        let (authority, path) = rest.split_once('/').ok_or_else(invalid)?;
        let (service, method) = path
            .trim_matches('/')
            .rsplit_once('/')
            .ok_or_else(invalid)?;
        if authority.is_empty() || service.is_empty() || method.is_empty() {
            return Err(invalid());
        }
        Ok(Self {
            tls,
            authority: authority.to_owned(),
            service: service.to_owned(),
            method: method.to_owned(),
        })
    }

    fn scheme(&self) -> &'static str {
        if self.tls {
            "https"
        } else {
            "http"
        }
    }

    fn uri(&self) -> String {
        format!("{}://{}", self.scheme(), self.authority)
    }
}

pub async fn send(request: &PreparedRequest, context: &Context<'_>) -> Result<Response, String> {
    let target = Target::parse(&request.url)?;
    let (metadata, host) = metadata(request)?;
    let started = Instant::now();
    let directives = &request.directives;
    let budget = directives.timeout.unwrap_or(DEFAULT_TIMEOUT);
    let deadline = tokio::time::Instant::now() + budget;
    let timed_out = |stage: &str| format!("timed out after {} {stage}", seconds(budget));

    let channel = timeout_at(
        deadline,
        connect(
            &target,
            host.as_deref(),
            directives
                .connection_timeout
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT),
        ),
    )
    .await
    .map_err(|_| timed_out("connecting"))??;
    let method = timeout_at(
        deadline,
        resolve_method(&channel, &target, &metadata, context),
    )
    .await
    .map_err(|_| timed_out("looking up the service descriptors"))??;
    if method.is_client_streaming() {
        return Err(format!(
            "{} is a {} method; only unary and server-streaming gRPC calls are supported",
            method.full_name(),
            if method.is_server_streaming() {
                "bidirectional streaming"
            } else {
                "client-streaming"
            }
        ));
    }
    let message = request_message(&request.body, method.input())?;

    let mut grpc = tonic::client::Grpc::new(channel)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_BODY_BYTES);
    let path = format!("/{}/{}", method.parent_service().full_name(), method.name())
        .parse()
        .map_err(|_| format!("invalid gRPC method path for {}", method.full_name()))?;
    let mut call = tonic::Request::new(message);
    *call.metadata_mut() = metadata;
    let codec = DynamicCodec(method.output());
    let streaming = method.is_server_streaming();
    let mut exchange = Exchange::new(streaming);

    if streaming {
        // Without `@timeout`, an open stream is only listened to for a while.
        let (window, read_deadline) = match directives.timeout {
            Some(_) => (budget, deadline),
            None => {
                let window_end = tokio::time::Instant::now() + DEFAULT_STREAM_WINDOW;
                (DEFAULT_STREAM_WINDOW.min(budget), window_end.min(deadline))
            }
        };
        let result = match timeout_at(read_deadline, async {
            grpc.ready().await.map_err(transport_status)?;
            grpc.server_streaming(call, path, codec).await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Status::deadline_exceeded(format!(
                "no response within {}",
                seconds(window)
            ))),
        };
        match result {
            Err(status) => exchange.finish(status),
            Ok(response) => {
                exchange.add_metadata(response.metadata().clone());
                let mut stream = response.into_inner();
                loop {
                    match timeout_at(read_deadline, stream.message()).await {
                        Err(_) => {
                            exchange.stopped = Some(format!(
                                "the stream was still open after {}; stopped listening",
                                seconds(window)
                            ));
                            break;
                        }
                        Ok(Err(status)) => {
                            exchange.finish(status);
                            break;
                        }
                        Ok(Ok(Some(message))) => {
                            if let Err(limit) = exchange.push(&message) {
                                exchange.error = Some(limit);
                                break;
                            }
                        }
                        Ok(Ok(None)) => {
                            match timeout_at(read_deadline, stream.trailers()).await {
                                Ok(Ok(Some(trailers))) => exchange.add_metadata(trailers),
                                Ok(Err(status)) => exchange.finish(status),
                                _ => {}
                            }
                            exchange.status.get_or_insert((Code::Ok, String::new()));
                            break;
                        }
                    }
                }
            }
        }
    } else {
        let result = match timeout_at(deadline, async {
            grpc.ready().await.map_err(transport_status)?;
            grpc.unary(call, path, codec).await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Status::deadline_exceeded(timed_out(
                "waiting for the response",
            ))),
        };
        match result {
            Err(status) => exchange.finish(status),
            Ok(response) => {
                let (metadata, message, _) = response.into_parts();
                exchange.add_metadata(metadata);
                if let Err(limit) = exchange.push(&message) {
                    exchange.error = Some(limit);
                }
                exchange.status = Some((Code::Ok, String::new()));
            }
        }
    }

    Ok(exchange.into_response(request.url.clone(), started.elapsed()))
}

fn seconds(duration: Duration) -> String {
    format!("{} s", duration.as_secs_f64())
}

/// What came back from a call, possibly partially.
struct Exchange {
    streaming: bool,
    headers: Vec<(String, String)>,
    /// Each message as pretty JSON, already indented for its place in the body.
    rendered: Vec<Vec<u8>>,
    rendered_bytes: usize,
    /// The final status; `None` when the stream was abandoned.
    status: Option<(Code, String)>,
    /// A local failure, such as a size limit.
    error: Option<String>,
    stopped: Option<String>,
}

impl Exchange {
    fn new(streaming: bool) -> Self {
        Self {
            streaming,
            headers: Vec::new(),
            rendered: Vec::new(),
            rendered_bytes: 0,
            status: None,
            error: None,
            stopped: None,
        }
    }

    fn add_metadata(&mut self, metadata: MetadataMap) {
        for (name, value) in metadata.into_headers().iter() {
            self.headers.push((
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            ));
        }
    }

    fn finish(&mut self, status: Status) {
        self.add_metadata(status.metadata().clone());
        let mut message = status.message().to_owned();
        if let Some(source) = std::error::Error::source(&status) {
            let detail = error_chain(source);
            if !message.contains(&detail) {
                if !message.is_empty() {
                    message.push_str(": ");
                }
                message.push_str(&detail);
            }
        }
        self.status = Some((status.code(), message));
    }

    /// Renders a message into the remaining JSON budget, so expansion (such as packed zeros
    /// turning into long arrays) is bounded before anything is retained.
    fn push(&mut self, message: &DynamicMessage) -> Result<(), String> {
        let limit = MAX_BODY_BYTES.saturating_sub(self.rendered_bytes);
        let rendered = render_json(message, limit, self.streaming)?;
        self.rendered_bytes += rendered.len() + 4;
        self.rendered.push(rendered);
        if self.rendered.len() >= MAX_MESSAGES {
            return Err(format!("stopped after receiving {MAX_MESSAGES} messages"));
        }
        Ok(())
    }

    fn into_response(self, url: String, elapsed: Duration) -> Response {
        let body = if self.streaming {
            if self.rendered.is_empty() {
                b"[]".to_vec()
            } else {
                let mut body = b"[\n  ".to_vec();
                body.extend(self.rendered.join(&b",\n  "[..]));
                body.extend_from_slice(b"\n]");
                body
            }
        } else {
            self.rendered.into_iter().next().unwrap_or_default()
        };

        let (status_line, error) = match (&self.status, self.error) {
            (_, Some(error)) => (String::new(), Some(error)),
            (Some((Code::Ok, message)), None) => (status_text(Code::Ok, message), None),
            (Some((code, message)), None) => (String::new(), Some(status_text(*code, message))),
            (None, None) => (format!("gRPC: {}", self.stopped.unwrap_or_default()), None),
        };
        Response {
            success: error.is_none(),
            status: None,
            status_line,
            url,
            headers: self.headers,
            content_type: (!body.is_empty()).then(|| "application/json".to_owned()),
            body,
            formatted: None,
            elapsed,
            error,
        }
    }
}

/// A writer that refuses to grow past `limit` bytes. With `indent`, every line after the first
/// is indented by two spaces, which places the JSON inside the body's array.
struct LimitedWriter {
    buffer: Vec<u8>,
    limit: usize,
    indent: bool,
    exceeded: bool,
}

impl io::Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let newlines = if self.indent {
            bytes.iter().filter(|&&byte| byte == b'\n').count()
        } else {
            0
        };
        if self.buffer.len() + bytes.len() + 2 * newlines > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("JSON budget exceeded"));
        }
        for &byte in bytes {
            self.buffer.push(byte);
            if self.indent && byte == b'\n' {
                self.buffer.extend_from_slice(b"  ");
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn render_json(message: &DynamicMessage, limit: usize, indent: bool) -> Result<Vec<u8>, String> {
    let mut writer = LimitedWriter {
        buffer: Vec::new(),
        limit,
        indent,
        exceeded: false,
    };
    let options = SerializeOptions::new().skip_default_fields(false);
    let mut serializer = serde_json::Serializer::pretty(&mut writer);
    let result = message.serialize_with_options(&mut serializer, &options);
    if writer.exceeded {
        return Err(format!(
            "stopped because the response JSON exceeded {} MiB",
            MAX_BODY_BYTES / 1024 / 1024
        ));
    }
    result.map_err(|error| format!("failed to convert a response message to JSON: {error}"))?;
    Ok(writer.buffer)
}

fn status_text(code: Code, message: &str) -> String {
    let mut text = format!("gRPC {} {}", code as i32, code_name(code));
    if !message.is_empty() {
        text.push_str(": ");
        text.push_str(message);
    }
    text
}

/// The canonical status names from the gRPC specification.
fn code_name(code: Code) -> &'static str {
    match code {
        Code::Ok => "OK",
        Code::Cancelled => "CANCELLED",
        Code::Unknown => "UNKNOWN",
        Code::InvalidArgument => "INVALID_ARGUMENT",
        Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
        Code::NotFound => "NOT_FOUND",
        Code::AlreadyExists => "ALREADY_EXISTS",
        Code::PermissionDenied => "PERMISSION_DENIED",
        Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
        Code::FailedPrecondition => "FAILED_PRECONDITION",
        Code::Aborted => "ABORTED",
        Code::OutOfRange => "OUT_OF_RANGE",
        Code::Unimplemented => "UNIMPLEMENTED",
        Code::Internal => "INTERNAL",
        Code::Unavailable => "UNAVAILABLE",
        Code::DataLoss => "DATA_LOSS",
        Code::Unauthenticated => "UNAUTHENTICATED",
    }
}

fn transport_status(error: tonic::transport::Error) -> Status {
    Status::unavailable(error_chain(&error))
}

fn request_message(
    body: &PreparedBody,
    descriptor: MessageDescriptor,
) -> Result<DynamicMessage, String> {
    let bytes = match body {
        PreparedBody::Bytes(bytes) if !bytes.iter().all(u8::is_ascii_whitespace) => bytes,
        _ => return Ok(DynamicMessage::new(descriptor)),
    };
    let name = descriptor.full_name().to_owned();
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let message = DynamicMessage::deserialize(descriptor, &mut deserializer)
        .and_then(|message| deserializer.end().map(|()| message))
        .map_err(|error| format!("the request body is not a valid {name}: {error}"))?;
    Ok(message)
}

/// Request headers become call metadata; `-bin` headers carry base64 values. A `Host` header
/// is returned separately: it becomes the `:authority` of the call.
fn metadata(request: &PreparedRequest) -> Result<(MetadataMap, Option<String>), String> {
    let mut metadata = MetadataMap::new();
    let mut host = None;
    for header in &request.headers {
        let name = header.name.to_ascii_lowercase();
        if name == "host" {
            host = Some(header.value.trim().to_owned());
            continue;
        }
        if name.ends_with("-bin") {
            let key = MetadataKey::from_bytes(name.as_bytes())
                .map_err(|_| format!("invalid metadata name {:?}", header.name))?;
            let value = base64::engine::general_purpose::STANDARD
                .decode(header.value.trim())
                .or_else(|_| {
                    base64::engine::general_purpose::STANDARD_NO_PAD.decode(header.value.trim())
                })
                .map_err(|_| format!("metadata {} must be base64", header.name))?;
            metadata.append_bin(key, BinaryMetadataValue::from_bytes(&value));
        } else {
            let key = MetadataKey::from_bytes(name.as_bytes())
                .map_err(|_| format!("invalid metadata name {:?}", header.name))?;
            let value = AsciiMetadataValue::try_from(header.value.as_str())
                .map_err(|_| format!("invalid value for metadata {}", header.name))?;
            metadata.append(key, value);
        }
    }
    Ok((metadata, host))
}

/// Connects to the URL's host (which also names the server for TLS). A `Host` header only
/// changes the `:authority` sent with each call.
async fn connect(
    target: &Target,
    host: Option<&str>,
    timeout: Duration,
) -> Result<Channel, String> {
    let mut endpoint = Endpoint::from_shared(target.uri())
        .map_err(|_| format!("invalid gRPC address {:?}", target.authority))?;
    if let Some(host) = host {
        let origin = format!("{}://{host}", target.scheme())
            .parse::<Uri>()
            .ok()
            .filter(|origin| origin.host().is_some())
            .ok_or_else(|| format!("invalid Host header {host:?}"))?;
        endpoint = endpoint.origin(origin);
    }
    let tls = target.tls;
    let connector = tower::service_fn(move |uri: Uri| async move {
        let host = uri
            .host()
            .ok_or_else(|| format!("missing host in {uri}"))?
            .to_owned();
        let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
        net::connect(&host, port, tls.then_some(Alpn::Http2), timeout)
            .await
            .map(TokioIo::new)
    });
    endpoint
        .connect_with_connector(connector)
        .await
        .map_err(|error| error_chain(&error))
}

/// Why reflection could not provide descriptors.
enum ReflectionError {
    /// This reflection version is not offered; try the next one, then `.proto` files.
    Unimplemented(String),
    /// Reflection answered but cannot be used (unknown service, refused, broken or oversized
    /// data); `.proto` files may still work.
    Unusable(String),
    /// The connection itself failed; no point in going on.
    Transport(String),
}

async fn resolve_method(
    channel: &Channel,
    target: &Target,
    metadata: &MetadataMap,
    context: &Context<'_>,
) -> Result<MethodDescriptor, String> {
    let reflection_problem = match reflect(channel, &target.service, metadata).await {
        // The server described the service, so a missing method is final.
        Ok(pool) if pool.get_service_by_name(&target.service).is_some() => {
            return find_method(&pool, target)
        }
        Ok(_) => format!("server reflection does not describe {}", target.service),
        Err(ReflectionError::Transport(error)) => return Err(error),
        Err(ReflectionError::Unimplemented(error) | ReflectionError::Unusable(error)) => error,
    };

    let base_dir = context.base_dir.to_owned();
    let roots = context.workspace_roots.to_vec();
    let service = target.service.clone();
    let compiled = tokio::task::spawn_blocking(move || compile_protos(&base_dir, &roots, &service))
        .await
        .map_err(|error| format!("the .proto search failed: {error}"))?;
    match compiled {
        Ok(pool) => find_method(&pool, target),
        Err(proto_problem) => Err(format!(
            "no descriptors for {}: {reflection_problem}; {proto_problem}",
            target.service
        )),
    }
}

fn find_method(pool: &DescriptorPool, target: &Target) -> Result<MethodDescriptor, String> {
    let service = pool
        .get_service_by_name(&target.service)
        .ok_or_else(|| format!("service {} is not defined", target.service))?;
    let method = service
        .methods()
        .find(|method| method.name() == target.method);
    method.ok_or_else(|| {
        let available: Vec<_> = service
            .methods()
            .map(|method| method.name().to_owned())
            .collect();
        format!(
            "service {} has no method {} (available: {})",
            target.service,
            target.method,
            available.join(", ")
        )
    })
}

/// Only failures of the connection itself skip the `.proto` fallback; any status the server
/// sent (UNAVAILABLE from a proxy, UNKNOWN, ...) still lets local files be tried.
fn classify(status: Status) -> ReflectionError {
    if status.code() == Code::Unimplemented && std::error::Error::source(&status).is_none() {
        return ReflectionError::Unimplemented(
            "the server does not implement reflection".to_owned(),
        );
    }
    let detail = status_text(status.code(), status.message());
    if is_transport_failure(&status) {
        let mut message = format!("could not reach the server: {detail}");
        if let Some(source) = std::error::Error::source(&status) {
            let source = error_chain(source);
            if !message.contains(&source) {
                message.push_str(": ");
                message.push_str(&source);
            }
        }
        return ReflectionError::Transport(message);
    }
    match status.code() {
        Code::Unauthenticated | Code::PermissionDenied => {
            ReflectionError::Unusable(format!("server reflection was refused: {detail}"))
        }
        _ => ReflectionError::Unusable(format!("server reflection failed: {detail}")),
    }
}

/// Whether a status was produced locally from an I/O or connection error.
fn is_transport_failure(status: &Status) -> bool {
    let mut source = std::error::Error::source(status);
    while let Some(error) = source {
        if error.is::<io::Error>() || error.is::<tonic::transport::Error>() {
            return true;
        }
        source = error.source();
    }
    false
}

async fn reflect(
    channel: &Channel,
    service: &str,
    metadata: &MetadataMap,
) -> Result<DescriptorPool, ReflectionError> {
    let files = match reflect_v1(channel.clone(), service, metadata).await {
        Err(ReflectionError::Unimplemented(_)) => {
            reflect_v1alpha(channel.clone(), service, metadata).await?
        }
        result => result?,
    };
    let built = tokio::task::spawn_blocking(move || {
        let mut pool = DescriptorPool::global();
        pool.add_file_descriptor_protos(files.into_values())
            .map(|()| pool)
    })
    .await
    .map_err(|error| ReflectionError::Unusable(format!("building descriptors failed: {error}")))?;
    built.map_err(|error| {
        ReflectionError::Unusable(format!(
            "server reflection returned unusable descriptors: {error}"
        ))
    })
}

/// Collects reflected files within the file-count and byte budgets.
#[derive(Default)]
struct ReflectedFiles {
    files: BTreeMap<String, FileDescriptorProto>,
    bytes: usize,
}

impl ReflectedFiles {
    fn add(&mut self, encoded: &[u8]) -> Result<(), ReflectionError> {
        self.bytes += encoded.len();
        if self.bytes > MAX_REFLECTED_BYTES {
            return Err(ReflectionError::Unusable(format!(
                "server reflection sent more than {} MiB of descriptors",
                MAX_REFLECTED_BYTES / 1024 / 1024
            )));
        }
        let file = FileDescriptorProto::decode(encoded).map_err(|error| {
            ReflectionError::Unusable(format!(
                "server reflection returned an invalid descriptor: {error}"
            ))
        })?;
        if !self.files.contains_key(file.name()) && self.files.len() >= MAX_REFLECTED_FILES {
            return Err(ReflectionError::Unusable(format!(
                "server reflection sent more than {MAX_REFLECTED_FILES} files"
            )));
        }
        self.files.insert(file.name().to_owned(), file);
        Ok(())
    }

    /// The first dependency that is neither received, requested, nor a well-known type.
    fn missing(&self, requested: &HashSet<String>) -> Option<String> {
        let global = DescriptorPool::global();
        self.files
            .values()
            .flat_map(|file| file.dependency.iter())
            .find(|name| {
                !self.files.contains_key(*name)
                    && !requested.contains(*name)
                    && global.get_file_by_name(name).is_none()
            })
            .cloned()
    }
}

/// The v1 and v1alpha reflection services are identical apart from their package, so one body
/// serves both. It asks for the file defining the service, then for every dependency the
/// server did not send along, one at a time on the same stream.
macro_rules! reflection_client {
    ($name:ident, $($module:ident)::+) => {
        async fn $name(
            channel: Channel,
            service: &str,
            metadata: &MetadataMap,
        ) -> Result<BTreeMap<String, FileDescriptorProto>, ReflectionError> {
            use $($module)::+::{
                server_reflection_client::ServerReflectionClient,
                server_reflection_request::MessageRequest,
                server_reflection_response::MessageResponse, ServerReflectionRequest,
            };

            let request = |message| ServerReflectionRequest {
                host: String::new(),
                message_request: Some(message),
            };
            let (sender, receiver) = tokio::sync::mpsc::channel(4);
            let _ = sender
                .send(request(MessageRequest::FileContainingSymbol(
                    service.to_owned(),
                )))
                .await;
            let mut call = tonic::Request::new(tokio_stream::wrappers::ReceiverStream::new(
                receiver,
            ));
            *call.metadata_mut() = metadata.clone();
            let mut client = ServerReflectionClient::new(channel)
                .max_decoding_message_size(MAX_REFLECTED_BYTES);
            let mut responses = client
                .server_reflection_info(call)
                .await
                .map_err(classify)?
                .into_inner();

            let mut received = ReflectedFiles::default();
            let mut requested = HashSet::new();
            let mut asking_for_service = true;
            loop {
                let response = responses
                    .message()
                    .await
                    .map_err(classify)?
                    .ok_or_else(|| {
                        ReflectionError::Unusable(
                            "the server reflection stream ended early".to_owned(),
                        )
                    })?;
                match response.message_response {
                    Some(MessageResponse::FileDescriptorResponse(response)) => {
                        for encoded in &response.file_descriptor_proto {
                            received.add(encoded)?;
                        }
                    }
                    Some(MessageResponse::ErrorResponse(error)) if asking_for_service => {
                        return Err(ReflectionError::Unusable(format!(
                            "server reflection does not know {service}: {}",
                            status_text(Code::from_i32(error.error_code), &error.error_message)
                        )));
                    }
                    // A missing dependency may still be a well-known type the pool already has.
                    Some(MessageResponse::ErrorResponse(_)) => {}
                    _ => {
                        return Err(ReflectionError::Unusable(
                            "server reflection sent an unexpected response".to_owned(),
                        ))
                    }
                }
                asking_for_service = false;

                let Some(missing) = received.missing(&requested) else {
                    return Ok(received.files);
                };
                if requested.len() >= MAX_REFLECTED_FILES {
                    return Err(ReflectionError::Unusable(format!(
                        "server reflection needed more than {MAX_REFLECTED_FILES} files"
                    )));
                }
                requested.insert(missing.clone());
                if sender
                    .send(request(MessageRequest::FileByFilename(missing)))
                    .await
                    .is_err()
                {
                    return Err(ReflectionError::Unusable(
                        "the server reflection stream closed early".to_owned(),
                    ));
                }
            }
        }
    };
}

reflection_client!(reflect_v1, tonic_reflection::pb::v1);
reflection_client!(reflect_v1alpha, tonic_reflection::pb::v1alpha);

/// Compiles the `.proto` files that declare `service`, searching the workspace root that
/// contains `base_dir` (or `base_dir` itself outside any root). Closer files are tried first.
fn compile_protos(
    base_dir: &Path,
    workspace_roots: &[PathBuf],
    service: &str,
) -> Result<DescriptorPool, String> {
    let root = workspace_roots
        .iter()
        .filter(|root| base_dir.starts_with(root))
        .max_by_key(|root| root.components().count())
        .cloned()
        .unwrap_or_else(|| base_dir.to_owned());
    let (package, name) = service.rsplit_once('.').unwrap_or(("", service));

    let mut candidates = Vec::new();
    find_protos(&root, 0, &mut 0, &mut candidates);
    candidates.retain(|path| {
        fs::metadata(path).is_ok_and(|metadata| metadata.len() <= MAX_PROTO_BYTES)
            && fs::read_to_string(path).is_ok_and(|source| declares_service(&source, package, name))
    });
    candidates.sort_by_key(|path| {
        let shared = path
            .components()
            .zip(base_dir.components())
            .take_while(|(a, b)| a == b)
            .count();
        (std::cmp::Reverse(shared), path.clone())
    });
    if candidates.is_empty() {
        return Err(format!(
            "no .proto file under {} declares it",
            root.display()
        ));
    }

    let mut errors = Vec::new();
    for path in &candidates {
        let Some(dir) = path.parent() else { continue };
        let ancestors: Vec<&Path> = dir
            .ancestors()
            .take_while(|ancestor| ancestor.starts_with(&root))
            .collect();
        // Try names relative to the root first (the protoc `-I root` convention), then relative
        // to the file's own directory.
        for includes in [
            ancestors.iter().rev().copied().collect::<Vec<_>>(),
            ancestors.clone(),
        ] {
            let compiled = protox::Compiler::new(&includes).and_then(|mut compiler| {
                compiler.include_imports(true).open_file(path)?;
                Ok(compiler.descriptor_pool())
            });
            match compiled {
                Ok(pool) if pool.get_service_by_name(service).is_some() => return Ok(pool),
                Ok(_) => {}
                Err(error) => {
                    errors.push(format!("{}: {error}", path.display()));
                    if ancestors.len() == 1 {
                        break;
                    }
                }
            }
        }
    }
    Err(format!(
        "compiling the .proto files that declare it failed: {}",
        errors.join("; ")
    ))
}

fn find_protos(dir: &Path, depth: usize, visited: &mut usize, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        *visited += 1;
        if *visited > MAX_SCAN_ENTRIES {
            return;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth < MAX_SCAN_DEPTH
                && !name.starts_with('.')
                && !matches!(name.as_ref(), "node_modules" | "target")
            {
                subdirs.push(entry.path());
            }
        } else if kind.is_file() && name.ends_with(".proto") {
            found.push(entry.path());
        }
    }
    subdirs.sort();
    for subdir in subdirs {
        find_protos(&subdir, depth + 1, visited, found);
    }
}

/// A cheap check before compiling: the source declares `package <package>;` (or no package,
/// for an unqualified service) and `service <name> {`. Comments and string literals are
/// skipped, and declarations may span lines or share one.
fn declares_service(source: &str, package: &str, name: &str) -> bool {
    let tokens = proto_tokens(source);
    let mut declared_package = "";
    let mut has_service = false;
    for window in tokens.windows(3) {
        match window {
            ["package", value, ";"] => declared_package = value,
            ["service", value, "{"] if *value == name => has_service = true,
            _ => {}
        }
    }
    has_service && declared_package == package
}

/// Splits protobuf source into identifiers (dotted names included) and punctuation, dropping
/// whitespace and comments. String literals become a single `"` token.
fn proto_tokens(source: &str) -> Vec<&str> {
    let bytes = source.as_bytes();
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.';
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
        } else if source[index..].starts_with("//") {
            index = source[index..]
                .find('\n')
                .map_or(bytes.len(), |end| index + end + 1);
        } else if source[index..].starts_with("/*") {
            index = source[index + 2..]
                .find("*/")
                .map_or(bytes.len(), |end| index + 2 + end + 2);
        } else if byte == b'"' || byte == b'\'' {
            let mut end = index + 1;
            while end < bytes.len() && bytes[end] != byte {
                end += if bytes[end] == b'\\' { 2 } else { 1 };
            }
            tokens.push("\"");
            index = (end + 1).min(bytes.len());
        } else if is_word(byte) {
            let start = index;
            while index < bytes.len() && is_word(bytes[index]) {
                index += 1;
            }
            tokens.push(&source[start..index]);
        } else {
            let width = source[index..].chars().next().map_or(1, char::len_utf8);
            tokens.push(&source[index..index + width]);
            index += width;
        }
    }
    tokens
}

/// Encodes and decodes `DynamicMessage`s; the decoder knows the response type.
struct DynamicCodec(MessageDescriptor);

impl Codec for DynamicCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynamicEncoder;
    type Decoder = DynamicDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        DynamicEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        DynamicDecoder(self.0.clone())
    }
}

struct DynamicEncoder;

impl Encoder for DynamicEncoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        item.encode(dst)
            .map_err(|error| Status::internal(format!("failed to encode the request: {error}")))
    }
}

struct DynamicDecoder(MessageDescriptor);

impl Decoder for DynamicDecoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        DynamicMessage::decode(self.0.clone(), src)
            .map(Some)
            .map_err(|error| Status::internal(format!("failed to decode a response: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets() {
        assert_eq!(
            Target::parse("localhost:50051/pkg.v1.Greeter/SayHello").unwrap(),
            Target {
                tls: false,
                authority: "localhost:50051".to_owned(),
                service: "pkg.v1.Greeter".to_owned(),
                method: "SayHello".to_owned(),
            }
        );
        let secure = Target::parse("grpcs://api.example.test/Svc/Call").unwrap();
        assert!(secure.tls);
        assert_eq!(secure.uri(), "https://api.example.test");
        assert!(Target::parse("https://h:1/a.B/C").unwrap().tls);
        assert!(!Target::parse("grpc://h:1/a.B/C").unwrap().tls);
        assert!(Target::parse("localhost:50051/Greeter").is_err());
        assert!(Target::parse("ftp://h/a.B/C").is_err());
    }

    #[test]
    fn builds_metadata_from_headers() {
        let request = PreparedRequest {
            method: "GRPC".to_owned(),
            url: String::new(),
            http_version: None,
            headers: vec![
                crate::syntax::Header {
                    name: "Authorization".to_owned(),
                    value: "Bearer x".to_owned(),
                },
                crate::syntax::Header {
                    name: "trace-bin".to_owned(),
                    value: "AQI=".to_owned(),
                },
                crate::syntax::Header {
                    name: "Host".to_owned(),
                    value: "api.internal:8443".to_owned(),
                },
            ],
            body: PreparedBody::Empty,
            directives: Default::default(),
        };
        let (metadata, host) = metadata(&request).unwrap();
        assert_eq!(metadata.get("authorization").unwrap(), "Bearer x");
        assert_eq!(
            metadata.get_bin("trace-bin").unwrap().to_bytes().unwrap(),
            vec![1u8, 2]
        );
        assert!(metadata.get("host").is_none());
        assert_eq!(host.as_deref(), Some("api.internal:8443"));
    }

    #[test]
    fn spots_service_declarations() {
        let plain = "syntax = \"proto3\";\npackage demo.v1;\nservice Greeter {\n}\n";
        assert!(declares_service(plain, "demo.v1", "Greeter"));
        assert!(!declares_service(plain, "demo", "Greeter"));
        assert!(!declares_service(plain, "demo.v1", "Other"));

        let compact = "syntax=\"proto3\";package demo.v1;service Greeter{rpc A(B)returns(B);}";
        assert!(declares_service(compact, "demo.v1", "Greeter"));
        let multiline = "/* package other;\n service Greeter { */\npackage\n  demo.v1\n;\n// service Fake {\nservice\n  Greeter\n{\n}\n";
        assert!(declares_service(multiline, "demo.v1", "Greeter"));
        assert!(!declares_service(multiline, "other", "Greeter"));
        let unqualified = "option x = \"package demo.v1; service Greeter {\";\nservice Greeter {}";
        assert!(declares_service(unqualified, "", "Greeter"));
        assert!(!declares_service(unqualified, "demo.v1", "Greeter"));
        // A field named `service` is not a declaration.
        assert!(!declares_service(
            "package demo.v1; message M { string service = 1; }",
            "demo.v1",
            "service"
        ));
    }

    #[test]
    fn limits_and_indents_rendered_json() {
        let pool = protox::compile(
            [PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grpc/types.proto")],
            [PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grpc")],
        )
        .map(|set| DescriptorPool::from_file_descriptor_set(set).unwrap())
        .unwrap();
        let descriptor = pool.get_message_by_name("test.v1.HelloRequest").unwrap();
        let mut message = DynamicMessage::new(descriptor);
        message.set_field_by_name("name", prost_reflect::Value::String("a".to_owned()));

        let rendered = render_json(&message, 1024, true).unwrap();
        assert_eq!(
            String::from_utf8(rendered).unwrap(),
            "{\n    \"name\": \"a\"\n  }"
        );
        let error = render_json(&message, 8, false).unwrap_err();
        assert!(error.contains("exceeded"), "{error}");
    }
}
