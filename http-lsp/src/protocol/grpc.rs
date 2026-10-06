//! gRPC over tonic without generated stubs: `GRPC host:port/package.Service/Method` with a JSON
//! body. Descriptors come from server reflection (v1, then v1alpha); when the server does not
//! offer it, `.proto` files in the workspace are compiled with protox. Messages are
//! `DynamicMessage`s carried by a small codec, so any service can be called. Unary and
//! server-streaming methods are supported.

use std::{
    collections::{BTreeMap, HashSet},
    fs,
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
use serde_json::Value;
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
/// Descriptor lookups (reflection round trips) give up after this long.
const REFLECTION_TIMEOUT: Duration = Duration::from_secs(30);
/// Files requested through reflection while resolving dependencies.
const MAX_REFLECTED_FILES: usize = 512;
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

    fn uri(&self) -> String {
        format!(
            "{}://{}",
            if self.tls { "https" } else { "http" },
            self.authority
        )
    }
}

pub async fn send(request: &PreparedRequest, context: &Context<'_>) -> Result<Response, String> {
    let target = Target::parse(&request.url)?;
    let metadata = metadata(request)?;
    let started = Instant::now();
    let directives = &request.directives;

    let channel = connect(
        &target,
        directives
            .connection_timeout
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT),
    )
    .await?;
    let method = resolve_method(&channel, &target, &metadata, context).await?;
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
        .max_decoding_message_size(MAX_BODY_BYTES)
        .max_encoding_message_size(MAX_BODY_BYTES);
    let path = format!("/{}/{}", method.parent_service().full_name(), method.name())
        .parse()
        .map_err(|_| format!("invalid gRPC method path for {}", method.full_name()))?;
    let mut call = tonic::Request::new(message);
    *call.metadata_mut() = metadata;
    let codec = DynamicCodec(method.output());

    let mut exchange = Exchange::default();
    if method.is_server_streaming() {
        let window = directives.timeout.unwrap_or(DEFAULT_STREAM_WINDOW);
        let deadline = tokio::time::Instant::now() + window;
        let result = match timeout_at(deadline, async {
            grpc.ready().await.map_err(transport_status)?;
            grpc.server_streaming(call, path, codec).await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Status::deadline_exceeded(format!(
                "no response within {} s",
                window.as_secs_f64()
            ))),
        };
        match result {
            Err(status) => exchange.finish(status),
            Ok(response) => {
                exchange.add_metadata(response.metadata().clone());
                let mut stream = response.into_inner();
                loop {
                    match timeout_at(deadline, stream.message()).await {
                        Err(_) => {
                            exchange.stopped = Some(format!(
                                "the stream was still open after {} s; stopped listening",
                                window.as_secs_f64()
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
                            match timeout_at(deadline, stream.trailers()).await {
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
        let deadline = directives.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let result = match tokio::time::timeout(deadline, async {
            grpc.ready().await.map_err(transport_status)?;
            grpc.unary(call, path, codec).await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Status::deadline_exceeded(format!(
                "no response within {} s",
                deadline.as_secs_f64()
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

    Ok(exchange.into_response(
        request.url.clone(),
        method.is_server_streaming(),
        started.elapsed(),
    ))
}

/// What came back from a call, possibly partially.
#[derive(Default)]
struct Exchange {
    headers: Vec<(String, String)>,
    messages: Vec<Value>,
    bytes: usize,
    /// The final status; `None` when the stream was abandoned.
    status: Option<(Code, String)>,
    /// A local failure, such as a size limit.
    error: Option<String>,
    stopped: Option<String>,
}

impl Exchange {
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

    fn push(&mut self, message: &DynamicMessage) -> Result<(), String> {
        let value = message_json(message)?;
        self.bytes += message.encoded_len();
        self.messages.push(value);
        if self.messages.len() >= MAX_MESSAGES {
            return Err(format!("stopped after receiving {MAX_MESSAGES} messages"));
        }
        if self.bytes > MAX_BODY_BYTES {
            return Err(format!(
                "stopped after receiving more than {} MiB",
                MAX_BODY_BYTES / 1024 / 1024
            ));
        }
        Ok(())
    }

    fn into_response(self, url: String, streaming: bool, elapsed: Duration) -> Response {
        let body = match (streaming, self.messages.len()) {
            (false, 0) => None,
            (false, _) => self.messages.into_iter().next(),
            (true, _) => Some(Value::Array(self.messages)),
        }
        .map(|value| serde_json::to_vec_pretty(&value).unwrap_or_default())
        .unwrap_or_default();

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

fn message_json(message: &DynamicMessage) -> Result<Value, String> {
    let options = SerializeOptions::new().skip_default_fields(false);
    message
        .serialize_with_options(serde_json::value::Serializer, &options)
        .map_err(|error| format!("failed to convert a response message to JSON: {error}"))
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

/// Request headers become call metadata; `-bin` headers carry base64 values.
fn metadata(request: &PreparedRequest) -> Result<MetadataMap, String> {
    let mut metadata = MetadataMap::new();
    for header in &request.headers {
        let name = header.name.to_ascii_lowercase();
        if name == "host" {
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
    Ok(metadata)
}

async fn connect(target: &Target, timeout: Duration) -> Result<Channel, String> {
    let endpoint = Endpoint::from_shared(target.uri())
        .map_err(|_| format!("invalid gRPC address {:?}", target.authority))?;
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
    /// Reflection works but does not know the service; try `.proto` files.
    Unavailable(String),
    /// Reflection was refused (for example missing credentials) or returned unusable data;
    /// `.proto` files may still work.
    Refused(String),
    /// The server could not be reached; no point in going on.
    Network(String),
}

async fn resolve_method(
    channel: &Channel,
    target: &Target,
    metadata: &MetadataMap,
    context: &Context<'_>,
) -> Result<MethodDescriptor, String> {
    let reflected = match tokio::time::timeout(
        REFLECTION_TIMEOUT,
        reflect(channel, &target.service, metadata),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(ReflectionError::Network(format!(
            "server reflection did not answer within {} s",
            REFLECTION_TIMEOUT.as_secs()
        ))),
    };
    let reflection_problem = match reflected {
        // The server described the service, so a missing method is final.
        Ok(pool) if pool.get_service_by_name(&target.service).is_some() => {
            return find_method(&pool, target)
        }
        Ok(_) => format!("server reflection does not describe {}", target.service),
        Err(ReflectionError::Network(error)) => return Err(error),
        Err(
            ReflectionError::Unimplemented(error)
            | ReflectionError::Unavailable(error)
            | ReflectionError::Refused(error),
        ) => error,
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

fn classify(status: Status) -> ReflectionError {
    let detail = status_text(status.code(), status.message());
    match status.code() {
        Code::Unimplemented => {
            ReflectionError::Unimplemented("the server does not implement reflection".to_owned())
        }
        Code::Unauthenticated | Code::PermissionDenied => {
            ReflectionError::Refused(format!("server reflection was refused: {detail}"))
        }
        Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled | Code::Unknown => {
            let mut message = format!("could not reach the server: {detail}");
            if let Some(source) = std::error::Error::source(&status) {
                let source = error_chain(source);
                if !message.contains(&source) {
                    message.push_str(": ");
                    message.push_str(&source);
                }
            }
            ReflectionError::Network(message)
        }
        _ => ReflectionError::Refused(format!("server reflection failed: {detail}")),
    }
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
    let mut pool = DescriptorPool::global();
    pool.add_file_descriptor_protos(files.into_values())
        .map_err(|error| {
            ReflectionError::Refused(format!(
                "server reflection returned unusable descriptors: {error}"
            ))
        })?;
    Ok(pool)
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
            let mut client =
                ServerReflectionClient::new(channel).max_decoding_message_size(MAX_BODY_BYTES);
            let mut responses = client
                .server_reflection_info(call)
                .await
                .map_err(classify)?
                .into_inner();

            let mut files = BTreeMap::new();
            let mut requested = HashSet::new();
            let mut asking_for_service = true;
            loop {
                let response = responses
                    .message()
                    .await
                    .map_err(classify)?
                    .ok_or_else(|| {
                        ReflectionError::Refused(
                            "the server reflection stream ended early".to_owned(),
                        )
                    })?;
                match response.message_response {
                    Some(MessageResponse::FileDescriptorResponse(response)) => {
                        for bytes in response.file_descriptor_proto {
                            let file = FileDescriptorProto::decode(bytes.as_slice()).map_err(
                                |error| {
                                    ReflectionError::Refused(format!(
                                        "server reflection returned an invalid descriptor: {error}"
                                    ))
                                },
                            )?;
                            files.insert(file.name().to_owned(), file);
                        }
                    }
                    Some(MessageResponse::ErrorResponse(error)) if asking_for_service => {
                        return Err(ReflectionError::Unavailable(format!(
                            "server reflection does not know {service}: {}",
                            status_text(Code::from_i32(error.error_code), &error.error_message)
                        )));
                    }
                    // A missing dependency may still be a well-known type the pool already has.
                    Some(MessageResponse::ErrorResponse(_)) => {}
                    _ => {
                        return Err(ReflectionError::Refused(
                            "server reflection sent an unexpected response".to_owned(),
                        ))
                    }
                }
                asking_for_service = false;

                let global = DescriptorPool::global();
                let missing = files
                    .values()
                    .flat_map(|file| file.dependency.iter())
                    .find(|name| {
                        !files.contains_key(*name)
                            && !requested.contains(*name)
                            && global.get_file_by_name(name).is_none()
                    })
                    .cloned();
                let Some(missing) = missing else {
                    return Ok(files);
                };
                if requested.len() >= MAX_REFLECTED_FILES {
                    return Err(ReflectionError::Refused(format!(
                        "server reflection needed more than {MAX_REFLECTED_FILES} files"
                    )));
                }
                requested.insert(missing.clone());
                if sender
                    .send(request(MessageRequest::FileByFilename(missing)))
                    .await
                    .is_err()
                {
                    return Err(ReflectionError::Refused(
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
    candidates.retain(|path| declares_service(path, package, name));
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

/// A cheap text check before compiling: the file has `package <package>;` (or none, for an
/// unqualified service) and a `service <name>` declaration.
fn declares_service(path: &Path, package: &str, name: &str) -> bool {
    if fs::metadata(path).map_or(true, |metadata| metadata.len() > MAX_PROTO_BYTES) {
        return false;
    }
    let Ok(source) = fs::read_to_string(path) else {
        return false;
    };
    let mut declared_package = "";
    let mut has_service = false;
    for line in source.lines() {
        let mut words = line.split(|c: char| c.is_whitespace() || c == ';' || c == '{');
        let words: Vec<&str> = words.by_ref().filter(|word| !word.is_empty()).collect();
        match words.as_slice() {
            ["package", value, ..] => declared_package = value,
            ["service", value, ..] if *value == name => has_service = true,
            _ => {}
        }
    }
    has_service && declared_package == package
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
                    value: "ignored".to_owned(),
                },
            ],
            body: PreparedBody::Empty,
            directives: Default::default(),
        };
        let metadata = metadata(&request).unwrap();
        assert_eq!(metadata.get("authorization").unwrap(), "Bearer x");
        assert_eq!(
            metadata.get_bin("trace-bin").unwrap().to_bytes().unwrap(),
            vec![1u8, 2]
        );
        assert!(metadata.get("host").is_none());
    }

    #[test]
    fn spots_service_declarations() {
        let dir = std::env::temp_dir().join(format!("zed-http-proto-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.proto");
        fs::write(
            &path,
            "syntax = \"proto3\";\npackage demo.v1;\nservice Greeter {\n}\n",
        )
        .unwrap();
        assert!(declares_service(&path, "demo.v1", "Greeter"));
        assert!(!declares_service(&path, "demo", "Greeter"));
        assert!(!declares_service(&path, "demo.v1", "Other"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
