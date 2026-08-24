const METHODS: &[&str] = &[
    "GET",
    "HEAD",
    "POST",
    "PUT",
    "PATCH",
    "DELETE",
    "OPTIONS",
    "CONNECT",
    "TRACE",
    "GRAPHQL",
    "WEBSOCKET",
    "WS",
    "GRPC",
    "SSE",
    "MQTT",
    "AMQP",
    "PROPFIND",
    "PROPPATCH",
    "COPY",
    "MOVE",
    "LOCK",
    "UNLOCK",
    "CHECKOUT",
    "REPORT",
    "MERGE",
    "MKACTIVITY",
    "MKWORKSPACE",
    "VERSION-CONTROL",
    "LIST",
];

#[derive(Debug, Clone)]
pub struct Request {
    /// Zero-indexed line containing the request method and URL.
    pub line: u32,
    pub region_start: u32,
    pub region_end: u32,
    pub method: String,
    pub url: String,
}

/// Scans request sections in a `.http` file.
///
/// Everything after a request line (headers, body) belongs to that request
/// until the next `###` separator, so method-like text inside a body is not
/// re-scanned as a new request. Regions span from the preceding separator
/// (or file start) to the line before the next one, which is what
/// [`request_at_line`] maps hover/lens positions against.
pub fn scan(text: &str) -> Vec<Request> {
    let mut requests: Vec<Request> = Vec::new();
    let mut in_body = false;
    let mut current_separator = None;
    let mut total_lines = 0;

    for (index, raw_line) in text.lines().enumerate() {
        total_lines = index as u32 + 1;
        let line = raw_line.trim_start();

        if line.starts_with("###") {
            if let Some(previous) = requests.last_mut() {
                previous.region_end = (index as u32).saturating_sub(1);
            }
            in_body = false;
            current_separator = Some(index as u32);
            continue;
        }

        if in_body {
            continue;
        }

        if let Some((method, remainder)) = line.split_once(char::is_whitespace) {
            if METHODS
                .iter()
                .any(|candidate| method.eq_ignore_ascii_case(candidate))
            {
                let url = remainder.split_whitespace().next().unwrap_or_default();
                if !url.is_empty() {
                    let request_line = index as u32;
                    requests.push(Request {
                        line: request_line,
                        region_start: current_separator.unwrap_or(request_line),
                        region_end: request_line,
                        method: method.to_ascii_uppercase(),
                        url: url.to_owned(),
                    });
                    current_separator = None;
                    in_body = true;
                }
            }
        }
    }

    if let Some(last) = requests.last_mut() {
        last.region_end = total_lines.saturating_sub(1);
    }

    requests
}

pub fn request_at_line(requests: &[Request], line: u32) -> Option<&Request> {
    requests
        .iter()
        .find(|request| request.region_start <= line && line <= request.region_end)
}

#[cfg(test)]
mod tests {
    use super::{request_at_line, scan};

    #[test]
    fn indexes_named_request_sections() {
        let source = "\
@host = http://localhost
### first
GET http://example.com/a

### second
POST http://example.com/b
Content-Type: application/json

{\"key\":true}
";

        let requests = scan(source);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].line, 2);
        assert_eq!(requests[0].region_start, 1);
        assert_eq!(requests[0].region_end, 3);
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].line, 5);
        assert_eq!(requests[1].region_start, 4);
        assert_eq!(requests[1].region_end, 8);
    }

    #[test]
    fn ignores_method_like_text_in_request_bodies() {
        let source = "\
### first
POST http://example.com/a
Content-Type: text/plain

GET this is body text
DELETE this is also body text

### second
GET http://example.com/b
";

        let requests = scan(source);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[1].method, "GET");
    }

    #[test]
    fn maps_separator_and_body_lines_to_the_enclosing_request() {
        let requests = scan(
            "\
file-level text
### first
GET http://example.com/a

### second
POST http://example.com/b
body
",
        );

        assert!(request_at_line(&requests, 0).is_none());
        assert_eq!(request_at_line(&requests, 1).unwrap().line, 2);
        assert_eq!(request_at_line(&requests, 3).unwrap().line, 2);
        assert_eq!(request_at_line(&requests, 4).unwrap().line, 5);
        assert_eq!(request_at_line(&requests, 6).unwrap().line, 5);
    }
}
