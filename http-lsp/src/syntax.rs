//! Line-based parser for IntelliJ-style `.http` files. All line numbers are 0-based.

use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct Document {
    pub blocks: Vec<RequestBlock>,
    /// `@name = value` lines in file order; later definitions win.
    pub file_vars: Vec<FileVariable>,
    pub imports: Vec<Import>,
    pub runs: Vec<RunCommand>,
    pub errors: Vec<ParseError>,
}

impl Document {
    pub fn error_summary(&self) -> Option<String> {
        if self.errors.is_empty() {
            return None;
        }
        let messages: Vec<_> = self
            .errors
            .iter()
            .take(3)
            .map(|error| format!("line {}: {}", error.line + 1, error.message))
            .collect();
        Some(messages.join("; "))
    }

    /// The block whose range contains `line`.
    pub fn block_at(&self, line: u32) -> Option<&RequestBlock> {
        self.blocks.iter().find(|block| block.contains(line))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: u32,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileVariable {
    pub line: u32,
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    pub line: u32,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunCommand {
    pub line: u32,
    pub target: RunTarget,
    /// `(@name=value, …)` overrides following the target.
    pub overrides: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunTarget {
    /// `run #Name`
    Request(String),
    /// `run ./file.http`
    File(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestBlock {
    /// The request line, where the code lenses go.
    pub start_line: u32,
    /// First line of the block: its `###` separator, or the line after the previous block.
    pub first_line: u32,
    /// Last line of the block, inclusive.
    pub last_line: u32,
    pub name: Option<String>,
    pub directives: Directives,
    pub method: String,
    pub url: String,
    pub http_version: Option<String>,
    pub headers: Vec<Header>,
    pub body: Body,
    pub pre_scripts: Vec<Script>,
    pub handlers: Vec<Script>,
    pub redirect: Option<Redirect>,
}

impl RequestBlock {
    pub fn contains(&self, line: u32) -> bool {
        (self.first_line..=self.last_line).contains(&line)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Directives {
    pub no_redirect: bool,
    pub no_cookie_jar: bool,
    pub no_log: bool,
    pub timeout: Option<Duration>,
    pub connection_timeout: Option<Duration>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// Parts are joined with a newline. An empty list means no body.
    Parts(Vec<BodyPart>),
    Multipart {
        boundary: String,
        parts: Vec<MultipartPart>,
    },
}

impl Body {
    pub fn is_empty(&self) -> bool {
        matches!(self, Body::Parts(parts) if parts.is_empty())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultipartPart {
    pub headers: Vec<Header>,
    pub body: Vec<BodyPart>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodyPart {
    Text(String),
    /// `< path` (raw) or `<@ path` (with variable substitution).
    File {
        path: String,
        substitute: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Script {
    Inline { line: u32, source: String },
    File { line: u32, path: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redirect {
    pub path: String,
    /// `>>!` replaces an existing file; `>>` picks a unique name instead.
    pub overwrite: bool,
}

pub fn find_header<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str())
}

pub fn parse(text: &str) -> Document {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut parser = Parser::default();
    for (index, line) in text.lines().enumerate() {
        parser.line(index as u32, line);
    }
    // `lines()` drops a trailing empty line, but the cursor can sit there; the last block
    // extends to it.
    let line_count = text.matches('\n').count() as u32;
    parser.last_line = parser.last_line.max(line_count);
    parser.finish()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Preamble,
    Url,
    Headers,
    Body,
    Handlers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScriptKind {
    PreRequest,
    Handler,
}

struct OpenScript {
    kind: ScriptKind,
    line: u32,
    source: String,
    lexer: JsLexer,
}

/// Just enough JavaScript lexing to find the closing `%}` outside strings, template literals
/// and comments. Regular expression literals are not recognised.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum JsLexer {
    #[default]
    Code,
    Quoted(u8),
    Template,
    BlockComment,
}

impl JsLexer {
    /// Returns the offset of a closing `%}` in code, carrying the state over to the next line.
    fn find_close(&mut self, line: &str) -> Option<usize> {
        let bytes = line.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            let next = bytes.get(index + 1).copied();
            match *self {
                JsLexer::Code => match (byte, next) {
                    (b'%', Some(b'}')) => return Some(index),
                    (b'\'' | b'"', _) => *self = JsLexer::Quoted(byte),
                    (b'`', _) => *self = JsLexer::Template,
                    (b'/', Some(b'/')) => break,
                    (b'/', Some(b'*')) => {
                        *self = JsLexer::BlockComment;
                        index += 1;
                    }
                    _ => {}
                },
                JsLexer::Quoted(quote) => {
                    if byte == b'\\' {
                        index += 1;
                    } else if byte == quote {
                        *self = JsLexer::Code;
                    }
                }
                JsLexer::Template => {
                    if byte == b'\\' {
                        index += 1;
                    } else if byte == b'`' {
                        *self = JsLexer::Code;
                    }
                }
                JsLexer::BlockComment => {
                    if byte == b'*' && next == Some(b'/') {
                        *self = JsLexer::Code;
                        index += 1;
                    }
                }
            }
            index += 1;
        }
        // Quoted strings cannot span lines.
        if matches!(self, JsLexer::Quoted(_)) {
            *self = JsLexer::Code;
        }
        None
    }
}

#[derive(Default)]
struct Section {
    first_line: u32,
    separator_name: Option<String>,
    comment_name: Option<String>,
    directives: Directives,
    request_line: Option<u32>,
    method: String,
    url: String,
    http_version: Option<String>,
    headers: Vec<Header>,
    body_lines: Vec<String>,
    pre_scripts: Vec<Script>,
    handlers: Vec<Script>,
    redirect: Option<Redirect>,
}

#[derive(Default)]
struct Parser {
    document: Document,
    section: Section,
    state: State,
    script: Option<OpenScript>,
    last_line: u32,
}

impl Parser {
    fn error(&mut self, line: u32, message: impl Into<String>) {
        self.document.errors.push(ParseError {
            line,
            message: message.into(),
        });
    }

    fn line(&mut self, index: u32, line: &str) {
        self.last_line = index;
        // `###` and directives inside `{% %}` belong to the script.
        if let Some(script) = self.script.as_mut() {
            match script.lexer.find_close(line) {
                Some(end) => {
                    script.source.push_str(&line[..end]);
                    let script = self.script.take().expect("script is open");
                    self.close_script(script);
                }
                None => {
                    script.source.push_str(line);
                    script.source.push('\n');
                }
            }
            return;
        }

        if let Some(name) = line.strip_prefix("###") {
            self.finish_section(index.saturating_sub(1));
            let name = name.trim();
            self.section = Section {
                first_line: index,
                separator_name: (!name.is_empty()).then(|| name.to_owned()),
                ..Section::default()
            };
            self.state = State::Preamble;
            return;
        }

        let trimmed = line.trim();
        if self.state == State::Url {
            // Indented `?a=1` / `&b=2` lines continue the URL.
            let indented = line.starts_with([' ', '\t']);
            if indented && trimmed.starts_with(['?', '&', '/']) {
                self.section.url.push_str(trimmed);
                return;
            }
            self.state = State::Headers;
        }

        match self.state {
            State::Preamble => self.preamble_line(index, trimmed),
            State::Url => unreachable!("URL continuation is handled above"),
            State::Headers => {
                if trimmed.is_empty() {
                    self.state = State::Body;
                } else if is_handler_line(line) {
                    self.state = State::Handlers;
                    self.handler_line(index, line);
                } else if !is_comment(trimmed) {
                    match parse_header(trimmed) {
                        Some(header) => self.section.headers.push(header),
                        None => self.error(index, format!("invalid header {trimmed:?}")),
                    }
                }
            }
            State::Body => {
                if is_handler_line(line) {
                    self.state = State::Handlers;
                    self.handler_line(index, line);
                } else {
                    self.section.body_lines.push(line.to_owned());
                }
            }
            State::Handlers => self.handler_line(index, line),
        }
    }

    fn preamble_line(&mut self, index: u32, line: &str) {
        if line.is_empty() {
            return;
        }
        if let Some(comment) = comment_text(line) {
            if let Some(directive) = comment.strip_prefix('@') {
                self.directive(index, directive);
            }
            return;
        }
        if let Some(rest) = line.strip_prefix('<') {
            let rest = rest.trim();
            if let Some(source) = rest.strip_prefix("{%") {
                self.open_script(ScriptKind::PreRequest, index, source);
            } else if rest.is_empty() {
                self.error(index, "pre-request script is missing its path");
            } else {
                self.section.pre_scripts.push(Script::File {
                    line: index,
                    path: rest.to_owned(),
                });
            }
            return;
        }
        if let Some(variable) = line.strip_prefix('@') {
            match variable.split_once('=') {
                Some((name, value)) if is_identifier(name.trim()) => {
                    self.document.file_vars.push(FileVariable {
                        line: index,
                        name: name.trim().to_owned(),
                        value: value.trim().to_owned(),
                    });
                }
                _ => self.error(index, format!("invalid variable definition {line:?}")),
            }
            return;
        }
        if let Some(path) = keyword(line, "import") {
            self.document.imports.push(Import {
                line: index,
                path: path.to_owned(),
            });
            return;
        }
        if let Some(target) = keyword(line, "run") {
            match parse_run(index, target) {
                Some(run) => self.document.runs.push(run),
                None => self.error(index, format!("invalid run command {line:?}")),
            }
            return;
        }
        self.request_line(index, line);
    }

    fn directive(&mut self, index: u32, directive: &str) {
        let (key, value) = match directive.find(|c: char| c.is_whitespace() || c == '=') {
            Some(split) => (
                &directive[..split],
                directive[split..]
                    .trim_start()
                    .trim_start_matches('=')
                    .trim(),
            ),
            None => (directive, ""),
        };
        let directives = &mut self.section.directives;
        match key {
            "name" if !value.is_empty() => self.section.comment_name = Some(value.to_owned()),
            "no-redirect" => directives.no_redirect = true,
            "no-cookie-jar" => directives.no_cookie_jar = true,
            "no-log" => directives.no_log = true,
            "timeout" | "connection-timeout" => match parse_duration(value) {
                Some(duration) if key == "timeout" => directives.timeout = Some(duration),
                Some(duration) => directives.connection_timeout = Some(duration),
                None => self.error(index, format!("invalid @{key} value {value:?}")),
            },
            // Other `@` comments (such as editor hints) are not directives.
            _ => {}
        }
    }

    fn request_line(&mut self, index: u32, line: &str) {
        let mut tokens: Vec<&str> = line.split_whitespace().collect();
        let http_version = match tokens.last() {
            Some(last) if tokens.len() > 1 && last.starts_with("HTTP/") => {
                let version = (*last).to_owned();
                tokens.pop();
                Some(version)
            }
            _ => None,
        };
        let method = match tokens.first() {
            Some(first) if tokens.len() > 1 && is_method(first) => {
                let method = (*first).to_owned();
                tokens.remove(0);
                method
            }
            _ => "GET".to_owned(),
        };
        let section = &mut self.section;
        section.request_line = Some(index);
        section.method = method;
        section.url = tokens.join(" ");
        section.http_version = http_version;
        self.state = State::Url;
    }

    fn handler_line(&mut self, index: u32, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() || is_comment(trimmed) {
            return;
        }
        let redirect = match line.strip_prefix(">>!") {
            Some(path) => Some((path, true)),
            None => line.strip_prefix(">>").map(|path| (path, false)),
        };
        if let Some((path, overwrite)) = redirect {
            let path = path.trim();
            if path.is_empty() {
                self.error(index, "response redirect is missing its path");
            } else if self.section.redirect.is_some() {
                self.error(index, "only one response redirect is allowed per request");
            } else {
                self.section.redirect = Some(Redirect {
                    path: path.to_owned(),
                    overwrite,
                });
            }
            return;
        }
        if let Some(rest) = line.strip_prefix('>') {
            let rest = rest.trim();
            if let Some(source) = rest.strip_prefix("{%") {
                self.open_script(ScriptKind::Handler, index, source);
            } else {
                self.section.handlers.push(Script::File {
                    line: index,
                    path: rest.to_owned(),
                });
            }
            return;
        }
        self.error(
            index,
            format!("unexpected {trimmed:?} after the response handler"),
        );
    }

    fn open_script(&mut self, kind: ScriptKind, line: u32, rest: &str) {
        let mut lexer = JsLexer::default();
        let script = match lexer.find_close(rest) {
            Some(end) => OpenScript {
                kind,
                line,
                source: rest[..end].to_owned(),
                lexer,
            },
            None => {
                self.script = Some(OpenScript {
                    kind,
                    line,
                    source: format!("{rest}\n"),
                    lexer,
                });
                return;
            }
        };
        self.close_script(script);
    }

    fn close_script(&mut self, script: OpenScript) {
        let parsed = Script::Inline {
            line: script.line,
            source: script.source.trim().to_owned(),
        };
        match script.kind {
            ScriptKind::PreRequest => self.section.pre_scripts.push(parsed),
            ScriptKind::Handler => self.section.handlers.push(parsed),
        }
    }

    fn finish_section(&mut self, last_line: u32) {
        let section = std::mem::take(&mut self.section);
        let Some(start_line) = section.request_line else {
            if let Some(script) = section.pre_scripts.first() {
                let line = match script {
                    Script::Inline { line, .. } | Script::File { line, .. } => *line,
                };
                self.error(line, "pre-request script is not followed by a request");
            }
            return;
        };
        if section.url.is_empty() {
            self.error(start_line, "request line is missing its URL");
            return;
        }
        let body = parse_body(&section.headers, section.body_lines);
        self.document.blocks.push(RequestBlock {
            start_line,
            first_line: section.first_line,
            last_line,
            name: section.comment_name.or(section.separator_name),
            directives: section.directives,
            method: section.method,
            url: section.url,
            http_version: section.http_version,
            headers: section.headers,
            body,
            pre_scripts: section.pre_scripts,
            handlers: section.handlers,
            redirect: section.redirect,
        });
    }

    fn finish(mut self) -> Document {
        if let Some(script) = self.script.take() {
            self.error(script.line, "script is missing its closing %}");
            self.close_script(script);
        }
        let last_line = self.last_line;
        self.finish_section(last_line);
        self.document
    }
}

fn is_comment(line: &str) -> bool {
    line.starts_with('#') || line.starts_with("//")
}

fn comment_text(line: &str) -> Option<&str> {
    line.strip_prefix("//")
        .or_else(|| line.strip_prefix('#'))
        .map(str::trim)
}

fn is_handler_line(line: &str) -> bool {
    if line.starts_with(">> ") || line.starts_with(">>! ") {
        return true;
    }
    let Some(rest) = line.strip_prefix("> ") else {
        return false;
    };
    let rest = rest.trim();
    rest.starts_with("{%") || rest.ends_with(".js")
}

fn is_method(token: &str) -> bool {
    token.len() >= 3 && token.bytes().all(|byte| byte.is_ascii_uppercase())
}

fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '$'))
}

/// Returns the argument when `line` is `keyword <argument>`.
fn keyword<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    rest.starts_with([' ', '\t'])
        .then(|| rest.trim())
        .filter(|rest| !rest.is_empty())
}

fn parse_run(line: u32, argument: &str) -> Option<RunCommand> {
    let (target, overrides) = match argument.split_once('(') {
        Some((target, overrides)) => (target.trim(), overrides.trim().strip_suffix(')')?),
        None => (argument, ""),
    };
    let overrides = overrides
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            let (name, value) = item.strip_prefix('@')?.split_once('=')?;
            Some((name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect::<Option<Vec<_>>>()?;
    let target = match target.strip_prefix('#') {
        Some(name) if !name.trim().is_empty() => RunTarget::Request(name.trim().to_owned()),
        Some(_) => return None,
        None if !target.is_empty() => RunTarget::File(target.to_owned()),
        None => return None,
    };
    Some(RunCommand {
        line,
        target,
        overrides,
    })
}

fn parse_header(line: &str) -> Option<Header> {
    let (name, value) = line.split_once(':')?;
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    Some(Header {
        name: name.to_owned(),
        value: value.trim().to_owned(),
    })
}

/// Accepts bare seconds or a `ms`, `s` or `m` suffix, optionally separated by a space.
fn parse_duration(value: &str) -> Option<Duration> {
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let amount: u64 = value[..split].parse().ok()?;
    match value[split..].trim() {
        "" | "s" => Some(Duration::from_secs(amount)),
        "ms" => Some(Duration::from_millis(amount)),
        "m" => Some(Duration::from_secs(amount.checked_mul(60)?)),
        _ => None,
    }
}

fn parse_body(headers: &[Header], mut lines: Vec<String>) -> Body {
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let leading = lines
        .iter()
        .take_while(|line| line.trim().is_empty())
        .count();
    lines.drain(..leading);

    if let Some(boundary) = find_header(headers, "content-type").and_then(multipart_boundary) {
        return Body::Multipart {
            parts: parse_multipart(&boundary, &lines),
            boundary,
        };
    }
    Body::Parts(parse_parts(&lines))
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    let mut parameters = content_type.split(';');
    let mime = parameters.next()?.trim();
    if !mime.to_ascii_lowercase().starts_with("multipart/") {
        return None;
    }
    parameters.find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}

fn parse_multipart(boundary: &str, lines: &[String]) -> Vec<MultipartPart> {
    let delimiter = format!("--{boundary}");
    let closing = format!("--{boundary}--");
    let mut parts = Vec::new();
    let mut current: Option<Vec<&String>> = None;
    for line in lines {
        let trimmed = line.trim_end();
        if trimmed == delimiter || trimmed == closing {
            if let Some(part) = current.take() {
                parts.push(parse_multipart_part(&part));
            }
            if trimmed == delimiter {
                current = Some(Vec::new());
            }
        } else if let Some(part) = current.as_mut() {
            part.push(line);
        }
    }
    if let Some(part) = current {
        parts.push(parse_multipart_part(&part));
    }
    parts
}

fn parse_multipart_part(lines: &[&String]) -> MultipartPart {
    let header_count = lines
        .iter()
        .position(|line| line.trim().is_empty())
        .unwrap_or(lines.len());
    let headers = lines[..header_count]
        .iter()
        .filter_map(|line| parse_header(line.trim()))
        .collect();
    let mut body: Vec<String> = lines[header_count.saturating_add(1).min(lines.len())..]
        .iter()
        .map(|line| (*line).clone())
        .collect();
    while body.last().is_some_and(|line| line.trim().is_empty()) {
        body.pop();
    }
    MultipartPart {
        headers,
        body: parse_parts(&body),
    }
}

/// A `<` starts a file include only at the start of a line and followed by a space, so XML and
/// other bodies containing `<` stay text.
fn parse_parts(lines: &[String]) -> Vec<BodyPart> {
    let mut parts = Vec::new();
    let mut text: Vec<&str> = Vec::new();
    let flush = |text: &mut Vec<&str>, parts: &mut Vec<BodyPart>| {
        if !text.is_empty() {
            parts.push(BodyPart::Text(text.join("\n")));
            text.clear();
        }
    };
    for line in lines {
        // Whitespace must follow, so `<@mention>` and `<tag>` stay text.
        let include = match line.strip_prefix("<@") {
            Some(path) if path.starts_with([' ', '\t']) => Some((path.trim(), true)),
            Some(_) => None,
            None => line
                .strip_prefix('<')
                .filter(|path| path.starts_with([' ', '\t']))
                .map(|path| (path.trim(), false)),
        };
        match include {
            Some((path, substitute)) if !path.is_empty() => {
                flush(&mut text, &mut parts);
                parts.push(BodyPart::File {
                    path: path.to_owned(),
                    substitute,
                });
            }
            _ => text.push(line),
        }
    }
    flush(&mut text, &mut parts);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(body: &Body) -> &str {
        match body {
            Body::Parts(parts) => match parts.as_slice() {
                [BodyPart::Text(text)] => text,
                parts => panic!("expected one text part, got {parts:?}"),
            },
            body => panic!("expected text, got {body:?}"),
        }
    }

    #[test]
    fn parses_separators_names_and_ranges() {
        let document = parse(include_str!("../tests/fixtures/separators.http"));
        assert!(document.errors.is_empty(), "{:?}", document.errors);
        let blocks = &document.blocks;
        assert_eq!(blocks.len(), 4);

        assert_eq!(blocks[0].name, None);
        assert_eq!(blocks[0].method, "GET");
        assert_eq!(blocks[0].url, "https://example.test/first");
        assert_eq!((blocks[0].first_line, blocks[0].start_line), (0, 1));
        assert_eq!(blocks[0].last_line, 2);

        assert_eq!(blocks[1].name.as_deref(), Some("Separator name"));
        assert_eq!(blocks[1].method, "POST");
        assert_eq!(blocks[1].http_version.as_deref(), Some("HTTP/1.1"));
        assert_eq!(blocks[1].first_line, 3);
        assert_eq!(blocks[1].header("content-type"), Some("application/json"));
        assert_eq!(text(&blocks[1].body), "{\n  \"a\": 1\n}");

        // The comment name wins over the separator name.
        assert_eq!(blocks[2].name.as_deref(), Some("commentName"));
        assert_eq!(blocks[3].name.as_deref(), Some("equalsName"));
        assert_eq!(blocks[3].method, "PURGE");
        assert_eq!(blocks[3].last_line, 23);
        assert!(blocks[3].contains(blocks[3].first_line));
        assert_eq!(
            document.block_at(17).map(|block| block.start_line),
            Some(blocks[3].start_line)
        );
    }

    #[test]
    fn parses_directives_and_multiline_urls() {
        let document = parse(include_str!("../tests/fixtures/directives.http"));
        assert!(document.errors.is_empty(), "{:?}", document.errors);
        let block = &document.blocks[0];
        assert_eq!(
            block.directives,
            Directives {
                no_redirect: true,
                no_cookie_jar: true,
                no_log: true,
                timeout: Some(Duration::from_secs(120)),
                connection_timeout: Some(Duration::from_millis(500)),
            }
        );
        assert_eq!(block.url, "https://example.test/search?q=1&page=2");
        assert_eq!(block.header("Accept"), Some("*/*"));
        assert_eq!(document.blocks[1].url, "https://example.test/bare");
        assert_eq!(document.blocks[1].method, "GET");
        assert_eq!(
            document.blocks[1].directives.timeout,
            Some(Duration::from_secs(180))
        );
    }

    #[test]
    fn parses_bodies_includes_and_multipart() {
        let document = parse(include_str!("../tests/fixtures/bodies.http"));
        assert!(document.errors.is_empty(), "{:?}", document.errors);
        let xml = &document.blocks[0];
        assert!(text(&xml.body).starts_with("<?xml"));
        assert!(text(&xml.body).contains("<user>"));

        assert_eq!(
            document.blocks[1].body,
            Body::Parts(vec![
                BodyPart::Text("prefix".to_owned()),
                BodyPart::File {
                    path: "./raw.json".to_owned(),
                    substitute: false
                },
                BodyPart::File {
                    path: "./template.json".to_owned(),
                    substitute: true
                },
            ])
        );

        let Body::Multipart { boundary, parts } = &document.blocks[2].body else {
            panic!("expected multipart: {:?}", document.blocks[2].body);
        };
        assert_eq!(boundary, "WebBoundary");
        assert_eq!(parts.len(), 2);
        assert_eq!(
            find_header(&parts[0].headers, "content-disposition"),
            Some("form-data; name=\"field\"")
        );
        assert_eq!(parts[0].body, vec![BodyPart::Text("value".to_owned())]);
        assert_eq!(
            parts[1].body,
            vec![BodyPart::File {
                path: "./upload.txt".to_owned(),
                substitute: false
            }]
        );

        // `<@mention>` and `<tag>` are text; `<@ path` needs the space.
        assert_eq!(
            document.blocks[3].body,
            Body::Parts(vec![
                BodyPart::Text("<@mention> hello\n<tag attr=\"1\"/>".to_owned()),
                BodyPart::File {
                    path: "template.json".to_owned(),
                    substitute: true
                },
            ])
        );
    }

    #[test]
    fn parses_scripts_handlers_and_redirects() {
        let document = parse(include_str!("../tests/fixtures/scripts.http"));
        assert!(document.errors.is_empty(), "{:?}", document.errors);
        assert_eq!(document.blocks.len(), 3);
        let block = &document.blocks[0];
        assert_eq!(block.pre_scripts.len(), 2);
        let Script::Inline { source, line } = &block.pre_scripts[0] else {
            panic!("expected an inline script");
        };
        assert_eq!(*line, 1);
        // `###` inside a script does not start a new block.
        assert!(source.contains("### not a separator"));
        assert!(source.contains("request.variables.set"));
        assert_eq!(
            block.pre_scripts[1],
            Script::File {
                line: 5,
                path: "./pre.js".to_owned()
            }
        );
        assert_eq!(text(&block.body), "{\"ok\": true}");
        assert_eq!(block.handlers.len(), 2);
        assert!(
            matches!(&block.handlers[0], Script::Inline { source, .. } if source == "client.log(1)")
        );
        assert!(
            matches!(&block.handlers[1], Script::File { path, .. } if path == "handlers/after.js")
        );
        assert_eq!(
            block.redirect,
            Some(Redirect {
                path: "./out.json".to_owned(),
                overwrite: true
            })
        );
        let second = &document.blocks[1];
        assert_eq!(second.handlers.len(), 1);
        assert!(second.body.is_empty());
        assert_eq!(
            second.redirect,
            Some(Redirect {
                path: "./second.json".to_owned(),
                overwrite: false
            })
        );
    }

    #[test]
    fn ignores_script_delimiters_in_strings_and_comments() {
        let document = parse(include_str!("../tests/fixtures/scripts.http"));
        let block = &document.blocks[2];
        assert_eq!(block.handlers.len(), 1);
        let Script::Inline { source, .. } = &block.handlers[0] else {
            panic!("expected an inline handler");
        };
        assert!(source.starts_with("client.log(\"%}\");"), "{source}");
        assert!(source.ends_with("client.log(\"done\")"), "{source}");
        assert!(source.contains("`a\n%}`"), "{source}");
    }

    #[test]
    fn last_block_extends_to_the_final_empty_line() {
        for text in [
            "GET http://x\n",
            "GET http://x\r\n",
            "###\nGET http://x\n\n",
        ] {
            let document = parse(text);
            let last = text.matches('\n').count() as u32;
            assert_eq!(document.blocks[0].last_line, last, "{text:?}");
            assert!(document.block_at(last).is_some(), "{text:?}");
        }
    }

    #[test]
    fn parses_file_level_syntax() {
        let document = parse(include_str!("../tests/fixtures/file_level.http"));
        assert!(document.errors.is_empty(), "{:?}", document.errors);
        let variables: Vec<_> = document
            .file_vars
            .iter()
            .map(|variable| (variable.name.as_str(), variable.value.as_str()))
            .collect();
        assert_eq!(
            variables,
            vec![
                ("host", "https://example.test"),
                ("token", "{{login.response.body.$.token}}"),
            ]
        );
        assert_eq!(document.imports[0].path, "./other.http");
        assert_eq!(
            document.runs,
            vec![
                RunCommand {
                    line: 4,
                    target: RunTarget::Request("Login".to_owned()),
                    overrides: vec![("user".to_owned(), "admin".to_owned())],
                },
                RunCommand {
                    line: 5,
                    target: RunTarget::File("./setup.http".to_owned()),
                    overrides: Vec::new(),
                },
            ]
        );
        assert_eq!(document.blocks.len(), 1);
        assert_eq!(document.blocks[0].method, "GRAPHQL");
    }

    #[test]
    fn handles_bom_crlf_and_reports_errors() {
        let document = parse("\u{feff}GET https://example.test\r\nX-Ok: 1\r\nnot a header\r\n");
        assert_eq!(document.blocks.len(), 1);
        assert_eq!(document.blocks[0].url, "https://example.test");
        assert_eq!(document.blocks[0].header("x-ok"), Some("1"));
        assert_eq!(document.errors.len(), 1);
        assert_eq!(document.errors[0].line, 2);
        assert!(document.error_summary().unwrap().starts_with("line 3:"));

        let unterminated = parse("< {%\nclient.log(1)\nGET https://example.test\n");
        assert!(unterminated
            .errors
            .iter()
            .any(|error| error.message.contains("%}")));
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("5"), Some(Duration::from_secs(5)));
        assert_eq!(parse_duration("5 s"), Some(Duration::from_secs(5)));
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("2 m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("2h"), None);
        assert_eq!(parse_duration(""), None);
    }
}
