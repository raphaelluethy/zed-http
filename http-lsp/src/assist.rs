//! Completion, hover and diagnostics for `.http` files. Positions are LSP positions: 0-based
//! lines and UTF-16 columns.

use std::{
    collections::{BTreeMap, HashMap},
    env,
    sync::Arc,
};

use serde_json::Value;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Diagnostic, DiagnosticSeverity,
    Documentation, FoldingRange, FoldingRangeKind, Hover, HoverContents, MarkupContent, MarkupKind,
    Position, Range, TextEdit,
};

use crate::{
    report::value_text,
    session::NamedResponse,
    syntax::{Document, LineContext, Script},
    variables::Variables,
};

/// Longest value shown in a hover or completion detail.
const MAX_SHOWN_VALUE: usize = 1000;

const METHODS: &[(&str, &str)] = &[
    ("GET", "HTTP GET request"),
    ("POST", "HTTP POST request"),
    ("PUT", "HTTP PUT request"),
    ("PATCH", "HTTP PATCH request"),
    ("DELETE", "HTTP DELETE request"),
    ("HEAD", "HTTP HEAD request"),
    ("OPTIONS", "HTTP OPTIONS request"),
    ("TRACE", "HTTP TRACE request"),
    ("CONNECT", "HTTP CONNECT request"),
    ("GRAPHQL", "GraphQL query, sent as a JSON POST"),
    (
        "WEBSOCKET",
        "WebSocket connection; `===` separates messages",
    ),
    ("GRPC", "gRPC call: host:port/package.Service/Method"),
];

const KEYWORDS: &[(&str, &str)] = &[
    (
        "run",
        "Run a named request (`run #name`) or another file (`run ./file.http`)",
    ),
    ("import", "Import the named requests of another file"),
];

const DIRECTIVES: &[(&str, &str, &str)] = &[
    (
        "name",
        "name ",
        "Names the request for `run #name` and `{{name.response…}}`",
    ),
    ("no-redirect", "no-redirect", "Do not follow redirects"),
    (
        "no-cookie-jar",
        "no-cookie-jar",
        "Send saved cookies but do not store new ones",
    ),
    (
        "no-log",
        "no-log",
        "Leave the response body out of the output",
    ),
    (
        "timeout",
        "timeout ",
        "Inactivity timeout: seconds, or with an ms, s or m suffix",
    ),
    (
        "connection-timeout",
        "connection-timeout ",
        "Connection timeout: seconds, or with an ms, s or m suffix",
    ),
];

const HEADERS: &[&str] = &[
    "Accept",
    "Accept-Charset",
    "Accept-Encoding",
    "Accept-Language",
    "Authorization",
    "Cache-Control",
    "Connection",
    "Content-Disposition",
    "Content-Encoding",
    "Content-Language",
    "Content-Length",
    "Content-Type",
    "Cookie",
    "Date",
    "Expect",
    "Forwarded",
    "From",
    "Host",
    "If-Match",
    "If-Modified-Since",
    "If-None-Match",
    "If-Range",
    "If-Unmodified-Since",
    "Origin",
    "Pragma",
    "Prefer",
    "Range",
    "Referer",
    "TE",
    "User-Agent",
    "X-API-Key",
    "X-Correlation-ID",
    "X-Forwarded-For",
    "X-Request-ID",
    "X-Requested-With",
];

const MEDIA_TYPES: &[&str] = &[
    "application/json",
    "application/xml",
    "application/x-www-form-urlencoded",
    "application/graphql",
    "application/octet-stream",
    "multipart/form-data; boundary=boundary",
    "text/plain",
    "text/html",
    "text/csv",
];

// Only advertise APIs implemented by script_prelude.js. These are HTTP-client helpers,
// not a replacement for a JavaScript language server.
const SCRIPT_MEMBERS: &[(&str, &str)] = &[
    ("client.global.set", "set(name, value): Save a value for subsequent requests in this workspace session. Use {{name}} in a request."),
    ("client.global.get", "get(name): Read a session global; returns null when absent."),
    ("client.global.clear", "clear(name): Remove a session global."),
    ("client.global.clearAll", "clearAll(): Remove all session globals."),
    ("client.global.isEmpty", "isEmpty(): Whether the session has no globals."),
    ("client.test", "test(name, callback): Queue a test to run after the script. Results appear in the terminal."),
    ("client.assert", "assert(condition, message): Fail the test or script if the condition is false."),
    ("client.log", "log(...values): Print values in the request's script output."),
    ("client.exit", "exit(): Stop the current script."),
    ("request.variables.set", "set(name, value): Set a variable for this request. Pre-request values are substituted before sending."),
    ("request.variables.get", "get(name): Read a request variable."),
    ("request.environment.get", "get(name): Read a variable from the selected environment."),
    ("request.headers.all", "all(): List request headers as name/value objects."),
    ("request.headers.findByName", "findByName(name): Find a request header, ignoring case."),
    ("request.method", "The request method."),
    ("request.url", "The request URL."),
    ("response.status", "The numeric HTTP response status, for example 200 or 401."),
    ("response.body", "The response body: a parsed value for JSON responses, otherwise text."),
    ("response.headers.valueOf", "valueOf(name): Read the first response header with this name, ignoring case."),
    ("response.headers.valuesOf", "valuesOf(name): Read all response headers with this name, ignoring case."),
    ("response.contentType.mimeType", "The response media type, without charset parameters."),
    ("response.contentType.charset", "The charset parameter of the response Content-Type, or null."),
];

fn header_documentation(name: &str) -> Option<&'static str> {
    Some(match name.to_ascii_lowercase().as_str() {
        "authorization" => "Credentials for the server. Use `Bearer {{token}}` to send a token saved by a sign-in response handler with `client.global.set(\"token\", response.body.token)`. Basic authentication takes a base64-encoded `username:password` value.",
        "content-type" => "The media type of the request body, for example `application/json`. Separate headers from the body with a blank line.",
        "accept" => "The response media types the client accepts, for example `application/json` or `*/*`.",
        "cookie" => "Cookies sent to the server, separated by semicolons: `name=value; other=value`.",
        "content-length" => "The request body size in bytes. Usually omit this header: the HTTP runner computes it from the body.",
        "accept-encoding" => "Accepted response compression formats. The runner supports gzip, deflate, Brotli and Zstandard decompression.",
        "cache-control" => "Caching directives such as `no-cache`, `no-store` or `max-age=0`.",
        "host" => "The destination host and optional port. Normally derived from the request URL.",
        "user-agent" => "Identifies the client sending this request.",
        _ => return None,
    })
}

/// Fold each request and its inline scripts without swallowing the next separator.
pub fn folding_ranges(text: &str, document: &Document) -> Vec<FoldingRange> {
    let lines: Vec<_> = text.lines().collect();
    let mut ranges = Vec::new();
    let mut push = |start: u32, end: u32| {
        if end > start {
            ranges.push(FoldingRange {
                start_line: start,
                start_character: None,
                end_line: end,
                end_character: None,
                kind: Some(FoldingRangeKind::Region),
                collapsed_text: None,
            });
        }
    };
    for block in &document.blocks {
        let end = (block.start_line..=block.last_line)
            .rev()
            .find(|line| {
                lines
                    .get(*line as usize)
                    .is_some_and(|line| !line.trim().is_empty())
            })
            .unwrap_or(block.start_line);
        push(block.start_line, end);
        for script in block.pre_scripts.iter().chain(&block.handlers) {
            if let Script::Inline { line, end_line, .. } = script {
                push(*line, *end_line);
            }
        }
    }
    ranges.sort_by_key(|range| (range.start_line, range.end_line));
    ranges.dedup_by_key(|range| (range.start_line, range.end_line));
    ranges
}

const DYNAMIC_VARIABLES: &[(&str, &str)] = &[
    ("$uuid", "A random UUID v4"),
    ("$random.uuid", "A random UUID v4"),
    ("$timestamp", "Seconds since the Unix epoch"),
    ("$isoTimestamp", "The current UTC time in ISO 8601"),
    (
        "$randomInt",
        "A random integer from 0 to 999; `$randomInt(min, max)` sets the range",
    ),
    (
        "$random.integer",
        "A random integer from 0 to 999; `$random.integer(min, max)` sets the range",
    ),
    (
        "$random.float",
        "A random float from 0 to 1000; `$random.float(min, max)` sets the range",
    ),
    (
        "$random.alphabetic",
        "10 random letters; `$random.alphabetic(n)` sets the length",
    ),
    (
        "$random.alphanumeric",
        "10 random letters and digits; `$random.alphanumeric(n)` sets the length",
    ),
    (
        "$random.hexadecimal",
        "10 random hex digits; `$random.hexadecimal(n)` sets the length",
    ),
    ("$random.email", "A random email address"),
    (
        "$env.",
        "An environment variable of the adapter process: `$env.NAME`",
    ),
    (
        "$processEnv",
        "An environment variable of the adapter process: `$processEnv NAME`",
    ),
];

/// Variables known before a request runs: what completion and hover can offer.
#[derive(Clone, Debug, Default)]
pub struct Known {
    pub environment_name: Option<String>,
    pub environment: BTreeMap<String, String>,
    pub globals: BTreeMap<String, Value>,
    pub responses: HashMap<String, Arc<NamedResponse>>,
}

pub fn diagnostics(document: &Document, _text: &str) -> Vec<Diagnostic> {
    document
        .errors
        .iter()
        .map(|error| Diagnostic {
            range: Range::new(
                Position::new(error.line, 0),
                Position::new(error.line, document.line_utf16_len(error.line)),
            ),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("zed-http".to_owned()),
            message: error.message.clone(),
            ..Diagnostic::default()
        })
        .collect()
}

pub fn complete(
    text: &str,
    document: &Document,
    position: Position,
    known: &Known,
) -> Vec<CompletionItem> {
    let Some(line) = document.line(text, position.line) else {
        return Vec::new();
    };
    let cursor = byte_offset(line, position.character);
    let prefix = &line[..cursor];
    let at = |byte: usize| Position::new(position.line, utf16_len(&line[..byte]));
    let range_from = |byte: usize| Range::new(at(byte), position);

    if let Some(start) = open_variable(prefix) {
        let typed = &prefix[start..];
        let start = start + (typed.len() - typed.trim_start().len());
        return variable_items(document, text, &prefix[start..], known, range_from(start));
    }

    if inline_script_prefix(prefix).is_some() {
        return script_items(prefix, position.line);
    }

    match document.line_context(position.line) {
        LineContext::Preamble => {
            let indent = prefix.len() - prefix.trim_start().len();
            let typed = &prefix[indent..];
            if let Some(start) = directive_start(typed) {
                return directive_items(range_from(indent + start));
            }
            if typed.chars().all(|c| c.is_ascii_alphabetic()) {
                return keyword_items(range_from(indent));
            }
            Vec::new()
        }
        LineContext::Headers => {
            if prefix.starts_with([' ', '\t']) {
                // An indented line continues the URL.
                return Vec::new();
            }
            if let Some((name, value_start)) = header_value_start(prefix) {
                return header_value_items(name, range_from(value_start));
            }
            if prefix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                return header_items(range_from(0));
            }
            Vec::new()
        }
        LineContext::Script => script_items(prefix, position.line),
        LineContext::Body => Vec::new(),
    }
}

pub fn hover(text: &str, document: &Document, position: Position, known: &Known) -> Option<Hover> {
    let line = document.line(text, position.line)?;
    let cursor = byte_offset(line, position.character);
    let variable = variable_spans(line)
        .into_iter()
        .find(|(start, end)| (*start..=*end).contains(&cursor));
    let Some((start, end)) = variable else {
        return documentation_hover(document, position, line, cursor);
    };
    let name = line[start + 2..end - 2].trim();
    if name.is_empty() {
        return None;
    }
    let markdown = describe_variable(name, document, text, known);
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: Some(Range::new(
            Position::new(position.line, utf16_len(&line[..start])),
            Position::new(position.line, utf16_len(&line[..end])),
        )),
    })
}

fn script_items(prefix: &str, line: u32) -> Vec<CompletionItem> {
    let start = member_start(prefix);
    let typed = &prefix[start..];
    let Some((object, _)) = typed.rsplit_once('.') else {
        return Vec::new();
    };
    let member_start = start + object.len() + 1;
    let range = Range::new(
        Position::new(line, utf16_len(&prefix[..member_start])),
        Position::new(line, utf16_len(prefix)),
    );
    let mut members = BTreeMap::new();
    for (path, documentation) in SCRIPT_MEMBERS {
        let Some(member) = path
            .strip_prefix(object)
            .and_then(|suffix| suffix.strip_prefix('.'))
        else {
            continue;
        };
        let name = member.split('.').next().unwrap();
        let nested = member.contains('.');
        members.entry(name).or_insert_with(|| {
            item(
                name,
                name,
                if nested || !documentation.contains('(') {
                    CompletionItemKind::PROPERTY
                } else {
                    CompletionItemKind::METHOD
                },
                None,
                Some(if nested {
                    format!("HTTP Client {object}.{name}")
                } else {
                    (*documentation).to_owned()
                }),
                0,
                range,
            )
        });
    }
    members.into_values().collect()
}

fn member_boundary(c: char) -> bool {
    !c.is_alphanumeric() && c != '_' && c != '.' && c != '$'
}

fn member_start(prefix: &str) -> usize {
    prefix
        .char_indices()
        .rev()
        .find(|(_, c)| member_boundary(*c))
        .map_or(0, |(index, c)| index + c.len_utf8())
}

fn inline_script_prefix(prefix: &str) -> Option<&str> {
    let rest = prefix
        .trim_start()
        .strip_prefix(['<', '>'])?
        .trim_start()
        .strip_prefix("{%")?;
    (!rest.contains("%}")).then_some(rest)
}

fn documentation_hover(
    document: &Document,
    position: Position,
    line: &str,
    cursor: usize,
) -> Option<Hover> {
    let context = if inline_script_prefix(&line[..cursor]).is_some() {
        LineContext::Script
    } else {
        document.line_context(position.line)
    };
    let (start, end, description) = match context {
        LineContext::Headers => {
            let (name, _) = line.split_once(':')?;
            if cursor > name.len() {
                return None;
            }
            (0, name.len(), header_documentation(name)?)
        }
        LineContext::Preamble => {
            let indent = line.len() - line.trim_start().len();
            let trimmed = line.trim_start();
            let rest = trimmed
                .strip_prefix("//")
                .or_else(|| trimmed.strip_prefix('#'))?;
            let name = rest.trim_start().strip_prefix('@')?;
            let start = indent + trimmed.len() - name.len();
            let end = start
                + line[start..]
                    .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                    .unwrap_or(line.len() - start);
            if cursor < start.saturating_sub(1) || cursor > end {
                return None;
            }
            let (_, _, description) = DIRECTIVES
                .iter()
                .find(|(name, _, _)| *name == &line[start..end])?;
            (start, end, *description)
        }
        LineContext::Script => {
            let start = member_start(&line[..cursor]);
            let end = cursor
                + line[cursor..]
                    .find(member_boundary)
                    .unwrap_or(line.len() - cursor);
            let (_, description) = SCRIPT_MEMBERS
                .iter()
                .find(|(name, _)| *name == &line[start..end])?;
            (start, end, *description)
        }
        LineContext::Body => return None,
    };
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("**{}**\n\n{description}", &line[start..end]),
        }),
        range: Some(Range::new(
            Position::new(position.line, utf16_len(&line[..start])),
            Position::new(position.line, utf16_len(&line[..end])),
        )),
    })
}

fn describe_variable(name: &str, document: &Document, text: &str, known: &Known) -> String {
    if let Some(variable) = name.strip_prefix("$env.") {
        let state = if env::var_os(variable).is_some() {
            "set"
        } else {
            "not set"
        };
        return format!("**{name}**: environment variable `{variable}` ({state})");
    }
    if name.starts_with('$') {
        let function = name.split('(').next().unwrap_or(name).trim();
        return match DYNAMIC_VARIABLES
            .iter()
            .find(|(label, _)| *label == function)
        {
            Some((_, description)) => format!("**{name}**: {description}"),
            None => format!("**{name}**: unknown dynamic variable"),
        };
    }

    let file = file_variables(document);
    let variables = Variables {
        request: BTreeMap::new(),
        globals: known.globals.clone(),
        file: file.clone(),
        environment: known.environment.clone(),
        responses: known.responses.clone(),
    };
    let environment_label = || match &known.environment_name {
        Some(environment) => format!("environment `{environment}`"),
        None => "environment".to_owned(),
    };
    // Highest precedence first, matching `Variables::lookup`.
    let mut layers = Vec::new();
    if let Some(value) = known.globals.get(name) {
        layers.push(("`client.global`".to_owned(), value_text(value)));
    }
    if let Some(value) = file.get(name) {
        layers.push(("file variable".to_owned(), value.clone()));
    }
    if let Some(value) = known.environment.get(name) {
        layers.push((environment_label(), value.clone()));
    }

    let set_by_script = script_set_names(text).iter().any(|set| set == name);
    let Some((source, value)) = layers.first() else {
        if let Some(value) = variables.lookup(name) {
            return format!("**{name}**: saved response\n\n{}", code_block(&value));
        }
        if let Some((request, _)) = name.split_once(".response.") {
            return format!("**{name}**: response of `{request}`, available after it has run");
        }
        if set_by_script {
            return format!("**{name}**: set by a script when the request runs");
        }
        return format!(
            "**{name}** is not defined. Define it in `http-client.env.json`, as `@{name} = value`, \
             or with `client.global.set` in a response handler."
        );
    };

    let mut markdown = format!("**{name}**: {source}\n\n{}", code_block(value));
    if value.contains("{{") {
        let expanded = variables.substitute(&format!("{{{{{name}}}}}"));
        markdown.push_str(&format!("\nExpands to\n\n{}", code_block(&expanded.text)));
    }
    for (shadowed, value) in layers.iter().skip(1) {
        markdown.push_str(&format!(
            "\nOverrides the {shadowed} value `{}`",
            inline(value)
        ));
    }
    if set_by_script && !known.globals.contains_key(name) {
        markdown.push_str("\n\nA script sets this when the request runs.");
    }
    markdown
}

fn variable_items(
    document: &Document,
    text: &str,
    typed: &str,
    known: &Known,
    range: Range,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |label: String,
                    kind: CompletionItemKind,
                    detail: String,
                    docs: Option<String>,
                    rank: u8| {
        if seen.insert(label.clone()) {
            items.push(item(&label, &label, kind, Some(detail), docs, rank, range));
        }
    };

    if typed.starts_with("$env.") {
        let mut names: Vec<String> = env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .collect();
        names.sort();
        for name in names {
            push(
                format!("$env.{name}"),
                CompletionItemKind::VARIABLE,
                "environment variable".into(),
                None,
                0,
            );
        }
        return items;
    }

    for (name, value) in &known.globals {
        push(
            name.clone(),
            CompletionItemKind::VARIABLE,
            "client.global".into(),
            Some(code_block(&value_text(value))),
            0,
        );
    }
    for (name, value) in file_variables(document) {
        push(
            name,
            CompletionItemKind::VARIABLE,
            "file variable".into(),
            Some(code_block(&value)),
            1,
        );
    }
    let environment = match &known.environment_name {
        Some(name) => format!("environment {name}"),
        None => "environment".to_owned(),
    };
    for (name, value) in &known.environment {
        push(
            name.clone(),
            CompletionItemKind::VARIABLE,
            environment.clone(),
            Some(code_block(value)),
            2,
        );
    }
    for name in script_set_names(text) {
        push(
            name,
            CompletionItemKind::VARIABLE,
            "set by a script".into(),
            None,
            3,
        );
    }
    for block in &document.blocks {
        let Some(name) = &block.name else { continue };
        let detail = format!("response of {name}");
        push(
            format!("{name}.response.body"),
            CompletionItemKind::REFERENCE,
            detail.clone(),
            None,
            4,
        );
        let Some(response) = known.responses.get(name) else {
            continue;
        };
        if let Value::Object(fields) = &response.body {
            for field in fields.keys() {
                push(
                    format!("{name}.response.body.$.{field}"),
                    CompletionItemKind::REFERENCE,
                    detail.clone(),
                    None,
                    4,
                );
            }
        }
        for (header, _) in &response.headers {
            push(
                format!("{name}.response.headers.{header}"),
                CompletionItemKind::REFERENCE,
                detail.clone(),
                None,
                4,
            );
        }
    }
    for (name, description) in DYNAMIC_VARIABLES {
        push(
            (*name).to_owned(),
            CompletionItemKind::FUNCTION,
            "dynamic variable".into(),
            Some((*description).to_owned()),
            5,
        );
    }
    items
}

fn keyword_items(range: Range) -> Vec<CompletionItem> {
    let methods = METHODS.iter().map(|(method, description)| {
        item(
            method,
            &format!("{method} "),
            CompletionItemKind::KEYWORD,
            None,
            Some((*description).to_owned()),
            0,
            range,
        )
    });
    let keywords = KEYWORDS.iter().map(|(keyword, description)| {
        item(
            keyword,
            &format!("{keyword} "),
            CompletionItemKind::KEYWORD,
            None,
            Some((*description).to_owned()),
            1,
            range,
        )
    });
    methods.chain(keywords).collect()
}

fn directive_items(range: Range) -> Vec<CompletionItem> {
    DIRECTIVES
        .iter()
        .map(|(label, insert, description)| {
            item(
                label,
                insert,
                CompletionItemKind::KEYWORD,
                None,
                Some((*description).to_owned()),
                0,
                range,
            )
        })
        .collect()
}

fn header_items(range: Range) -> Vec<CompletionItem> {
    HEADERS
        .iter()
        .map(|header| {
            item(
                header,
                &format!("{header}: "),
                CompletionItemKind::PROPERTY,
                None,
                header_documentation(header).map(str::to_owned),
                0,
                range,
            )
        })
        .collect()
}

fn header_value_items(name: &str, range: Range) -> Vec<CompletionItem> {
    let values: Vec<&str> = match name.to_ascii_lowercase().as_str() {
        "content-type" => MEDIA_TYPES.to_vec(),
        "accept" => [&["*/*"], MEDIA_TYPES].concat(),
        "authorization" => vec!["Bearer ", "Basic "],
        "accept-encoding" => vec![
            "gzip, deflate, br, zstd",
            "gzip",
            "deflate",
            "br",
            "zstd",
            "identity",
        ],
        "cache-control" => vec!["no-cache", "no-store", "max-age=0", "must-revalidate"],
        "connection" => vec!["keep-alive", "close"],
        "content-encoding" => vec!["gzip", "deflate", "br", "zstd"],
        "prefer" => vec!["return=representation", "return=minimal", "respond-async"],
        "x-requested-with" => vec!["XMLHttpRequest"],
        _ => Vec::new(),
    };
    values
        .into_iter()
        .map(|value| {
            item(
                value.trim_end(),
                value,
                CompletionItemKind::VALUE,
                None,
                None,
                0,
                range,
            )
        })
        .collect()
}

fn item(
    label: &str,
    insert: &str,
    kind: CompletionItemKind,
    detail: Option<String>,
    documentation: Option<String>,
    rank: u8,
    range: Range,
) -> CompletionItem {
    CompletionItem {
        label: label.to_owned(),
        kind: Some(kind),
        detail,
        documentation: documentation.map(|value| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            })
        }),
        sort_text: Some(format!("{rank}{label}")),
        filter_text: Some(label.to_owned()),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: insert.to_owned(),
        })),
        ..CompletionItem::default()
    }
}

/// File variables as the runner sees them: later definitions win.
fn file_variables(document: &Document) -> BTreeMap<String, String> {
    document
        .file_vars
        .iter()
        .map(|variable| (variable.name.clone(), variable.value.clone()))
        .collect()
}

/// Names passed to `client.global.set` or `request.variables.set` in the file's scripts.
fn script_set_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for call in ["client.global.set(", "request.variables.set("] {
        for (index, _) in text.match_indices(call) {
            let rest = text[index + call.len()..].trim_start();
            let Some(quote) = rest
                .chars()
                .next()
                .filter(|c| matches!(c, '"' | '\'' | '`'))
            else {
                continue;
            };
            let Some(end) = rest[1..].find(quote) else {
                continue;
            };
            let name = &rest[1..1 + end];
            if !name.is_empty() && !names.iter().any(|seen| seen == name) {
                names.push(name.to_owned());
            }
        }
    }
    names
}

/// The byte offset just after the last `{{` that is not closed before the cursor.
fn open_variable(prefix: &str) -> Option<usize> {
    let start = prefix.rfind("{{")? + 2;
    (!prefix[start..].contains("}}")).then_some(start)
}

/// The offset of the directive name in `# @name`, `// @name` or `#@name`.
fn directive_start(typed: &str) -> Option<usize> {
    let rest = typed
        .strip_prefix("//")
        .or_else(|| typed.strip_prefix('#'))?;
    let name = rest.trim_start().strip_prefix('@')?;
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
        .then(|| typed.len() - name.len())
}

/// For `Name: va`, the header name and the offset where its value starts.
fn header_value_start(prefix: &str) -> Option<(&str, usize)> {
    let (name, value) = prefix.split_once(':')?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let skipped = value.len() - value.trim_start().len();
    Some((name, name.len() + 1 + skipped))
}

/// Byte ranges of the `{{…}}` references on a line, braces included.
fn variable_spans(line: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut offset = 0;
    while let Some(start) = line[offset..].find("{{") {
        let start = offset + start;
        let Some(length) = line[start + 2..].find("}}") else {
            break;
        };
        let end = start + 2 + length + 2;
        spans.push((start, end));
        offset = end;
    }
    spans
}

fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

/// Converts a UTF-16 column to a byte offset, clamped to the line.
fn byte_offset(line: &str, character: u32) -> usize {
    let mut units = 0;
    for (index, c) in line.char_indices() {
        if units >= character {
            return index;
        }
        units += c.len_utf16() as u32;
    }
    line.len()
}

fn code_block(value: &str) -> String {
    format!("```text\n{}\n```", shorten(value))
}

fn inline(value: &str) -> String {
    shorten(value).replace('`', "'").replace('\n', " ")
}

fn shorten(value: &str) -> String {
    if value.len() <= MAX_SHOWN_VALUE {
        return value.to_owned();
    }
    let mut end = MAX_SHOWN_VALUE;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::syntax;

    const TEXT: &str = "@baseUrl = http://localhost:8080\n\n### Login\n# @name login\nPOST {{baseUrl}}/api/login\nContent-Type: application/json\n\n{}\n> {%\nclient.global.set(\"token\", response.body.token);\n%}\n\n###\nGET {{baseUrl}}/api\nAuthorization: Bearer {{tok\n";

    fn known() -> Known {
        Known {
            environment_name: Some("dev".to_owned()),
            environment: BTreeMap::from([("apiKey".to_owned(), "secret".to_owned())]),
            globals: BTreeMap::new(),
            responses: HashMap::from([(
                "login".to_owned(),
                Arc::new(NamedResponse {
                    status: Some(200),
                    headers: vec![("x-trace".to_owned(), "1".to_owned())],
                    body: json!({ "token": "abc" }),
                }),
            )]),
        }
    }

    fn complete(text: &str, position: Position, known: &Known) -> Vec<CompletionItem> {
        super::complete(text, &syntax::parse(text), position, known)
    }

    fn hover(text: &str, position: Position, known: &Known) -> Option<Hover> {
        super::hover(text, &syntax::parse(text), position, known)
    }

    fn labels(items: &[CompletionItem]) -> Vec<&str> {
        items.iter().map(|item| item.label.as_str()).collect()
    }

    fn end_of(text: &str, line: u32) -> Position {
        Position::new(line, utf16_len(text.lines().nth(line as usize).unwrap()))
    }

    #[test]
    fn completes_variables_from_every_source() {
        let items = complete(TEXT, end_of(TEXT, 14), &known());
        let labels = labels(&items);
        for expected in [
            "baseUrl",
            "apiKey",
            "token",
            "login.response.body",
            "login.response.body.$.token",
            "login.response.headers.x-trace",
            "$uuid",
            "$random.integer",
        ] {
            assert!(labels.contains(&expected), "missing {expected}: {labels:?}");
        }
        let token = items.iter().find(|item| item.label == "token").unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &token.text_edit else {
            panic!("expected a text edit");
        };
        // `Authorization: Bearer {{` is 24 UTF-16 units; the edit replaces `tok`.
        assert_eq!(edit.range.start, Position::new(14, 24));
        assert_eq!(edit.new_text, "token");
    }

    #[test]
    fn completes_methods_headers_values_and_directives() {
        let text = "###\nPO\n";
        assert!(labels(&complete(text, Position::new(1, 2), &Known::default())).contains(&"POST"));

        let text = "GET https://example.test\nCont\n";
        let items = complete(text, Position::new(1, 4), &Known::default());
        let content_type = items
            .iter()
            .find(|item| item.label == "Content-Type")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &content_type.text_edit else {
            panic!("expected a text edit");
        };
        assert_eq!(edit.new_text, "Content-Type: ");

        let text = "GET https://example.test\nContent-Type: app\n";
        let items = complete(text, Position::new(1, 17), &Known::default());
        assert!(labels(&items).contains(&"application/json"));

        let text = "### Login\n# @no\nGET https://example.test\n";
        let items = complete(text, Position::new(1, 5), &Known::default());
        assert!(labels(&items).contains(&"no-redirect"));
    }

    #[test]
    fn offers_nothing_in_bodies_and_scripts() {
        let text = "POST https://example.test\n\nbod\n> {%\ncli\n%}\n";
        assert!(complete(text, Position::new(2, 3), &Known::default()).is_empty());
        assert!(complete(text, Position::new(4, 3), &Known::default()).is_empty());
    }

    #[test]
    fn globals_win_over_file_variables_and_environment() {
        let mut known = known();
        known
            .environment
            .insert("token".to_owned(), "env-token".to_owned());
        known
            .globals
            .insert("token".to_owned(), json!("global-token"));
        let text = "@token = file-token\nGET https://example.test\nAuthorization: Bearer {{tok\n";
        let items = complete(text, end_of(text, 2), &known);
        let tokens: Vec<_> = items.iter().filter(|item| item.label == "token").collect();
        assert_eq!(tokens.len(), 1, "{items:?}");
        assert_eq!(tokens[0].detail.as_deref(), Some("client.global"));
        let Some(Documentation::MarkupContent(docs)) = &tokens[0].documentation else {
            panic!("expected documentation");
        };
        assert!(docs.value.contains("global-token"), "{}", docs.value);

        let text =
            "@token = file-token\nGET https://example.test\nAuthorization: Bearer {{token}}\n";
        let HoverContents::Markup(markup) =
            hover(text, Position::new(2, 26), &known).unwrap().contents
        else {
            panic!("expected markdown");
        };
        assert!(markup.value.contains("`client.global`"), "{}", markup.value);
        assert!(markup.value.contains("global-token"), "{}", markup.value);

        let variables = Variables {
            globals: known.globals.clone(),
            file: file_variables(&syntax::parse(text)),
            environment: known.environment.clone(),
            ..Variables::default()
        };
        assert_eq!(variables.substitute("{{token}}").text, "global-token");

        let known = Known {
            globals: BTreeMap::new(),
            ..known
        };
        let text = "@token = file-token\nGET https://example.test\nAuthorization: Bearer {{tok\n";
        let items = complete(text, end_of(text, 2), &known);
        let tokens: Vec<_> = items.iter().filter(|item| item.label == "token").collect();
        assert_eq!(tokens.len(), 1, "{items:?}");
        assert_eq!(tokens[0].detail.as_deref(), Some("file variable"));
    }

    #[test]
    fn hovers_show_the_winning_value_and_its_source() {
        let mut known = known();
        known
            .environment
            .insert("baseUrl".to_owned(), "http://env".to_owned());
        let result = hover(TEXT, Position::new(13, 8), &known).unwrap();
        let HoverContents::Markup(markup) = result.contents else {
            panic!("expected markdown");
        };
        assert!(markup.value.contains("file variable"), "{}", markup.value);
        assert!(markup.value.contains("http://localhost:8080"));
        assert!(markup
            .value
            .contains("Overrides the environment `dev` value `http://env`"));
        assert_eq!(result.range.unwrap().start, Position::new(13, 4));

        let text = "GET {{token}}\n> {% client.global.set('token', 'x') %}\n";
        let HoverContents::Markup(markup) = hover(text, Position::new(0, 7), &Known::default())
            .unwrap()
            .contents
        else {
            panic!("expected markdown");
        };
        assert!(markup.value.contains("set by a script"), "{}", markup.value);
    }

    #[test]
    fn reports_parse_errors_on_their_line() {
        let text = "GET https://example.test\nnot a header\n";
        let document = syntax::parse(text);
        let diagnostics = diagnostics(&document, text);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start.line, 1);
        assert_eq!(diagnostics[0].range.end.character, 12);
    }

    #[test]
    fn converts_utf16_columns() {
        assert_eq!(byte_offset("ä{{x", 1), 2);
        assert_eq!(byte_offset("😀x", 2), 4);
        assert_eq!(byte_offset("ab", 9), 2);
    }

    #[test]
    fn documents_headers_directives_and_script_helpers_in_context() {
        for (text, position, expected) in [
            ("GET https://example.test\nAuthorization: Bearer {{token}}\n", Position::new(1, 4), "sign-in response handler"),
            ("# @timeout 10s\nGET https://example.test\n", Position::new(0, 5), "Inactivity timeout"),
            ("GET https://example.test\n> {%\nclient.global.set('token', response.body.token);\n%}\n", Position::new(2, 9), "subsequent requests"),
            ("GET https://example.test\n> {% client.global.set('token', response.body.token); %}\n", Position::new(1, 12), "subsequent requests"),
        ] {
            let result = hover(text, position, &Known::default()).unwrap();
            let HoverContents::Markup(markup) = result.contents else { panic!("expected markup") };
            assert!(markup.value.contains(expected), "{}", markup.value);
        }
        let text = "POST https://example.test\n\nAuthorization: body text\n";
        assert!(hover(text, Position::new(2, 3), &Known::default()).is_none());
    }

    #[test]
    fn completes_script_members_with_utf16_edits() {
        for (text, row, expected) in [
            (
                "GET https://example.test\n> {%\nclient.global.se\n%}\n",
                2,
                "set",
            ),
            (
                "GET https://example.test\n> {% response.st %}\n",
                1,
                "status",
            ),
            (
                "< {% request.variables.se %}\nGET https://example.test\n",
                0,
                "set",
            ),
        ] {
            let line = text.lines().nth(row).unwrap();
            let prefix = line.split(" %}").next().unwrap();
            let position = Position::new(row as u32, utf16_len(prefix));
            let items = complete(text, position, &Known::default());
            assert!(labels(&items).contains(&expected), "{items:?}");
        }
        let items = script_items("😀client.global.se", 0);
        let set = items.iter().find(|item| item.label == "set").unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &set.text_edit else {
            panic!("expected edit")
        };
        assert_eq!(
            edit.range,
            Range::new(Position::new(0, 16), Position::new(0, 18))
        );
        assert_eq!(edit.new_text, "set");
    }

    #[test]
    fn folds_requests_and_scripts_using_parser_boundaries() {
        let text = "### Login\nGET https://example.test\n> {%\n\nclient.log('%}');\n\n%}\n\n### Next\nGET https://example.test\n";
        let ranges = folding_ranges(text, &syntax::parse(text));
        let lines: Vec<_> = ranges
            .iter()
            .map(|range| (range.start_line, range.end_line))
            .collect();
        assert_eq!(lines, [(1, 6), (2, 6)]);
    }
}
