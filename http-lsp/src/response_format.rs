use crate::httpyac::{Exchange, RequestResult, Response};

#[derive(Debug, Clone, Copy)]
pub enum View {
    Full,
    HeadersOnly,
}

pub fn format(exchange: &Exchange, view: View) -> String {
    exchange
        .requests
        .first()
        .map(|request| format_request(request, view))
        .unwrap_or_else(|| "(no request executed)\n".to_owned())
}

fn format_request(request: &RequestResult, view: View) -> String {
    let mut output = String::new();

    if let Some(name) = request.name.as_deref().or(request.title.as_deref()) {
        output.push_str(&format!("# {name}\n\n"));
    }

    match request.response.as_ref() {
        Some(response) => write_response(&mut output, response, view),
        None => output.push_str("(no response)\n"),
    }

    output
}

fn write_response(output: &mut String, response: &Response, view: View) {
    let protocol = response.protocol.as_deref().unwrap_or("HTTP/1.1");
    let status_message = response.status_message.as_deref().unwrap_or_default();
    output.push_str(&format!(
        "{protocol} {} {status_message}\n",
        response.status_code
    ));

    if let Some(duration) = response.timings.as_ref().and_then(|timings| timings.total) {
        let size = response
            .meta
            .as_ref()
            .and_then(|meta| meta.size.as_deref())
            .unwrap_or_default();
        if size.is_empty() {
            output.push_str(&format!("# {duration:.0} ms\n"));
        } else {
            output.push_str(&format!("# {duration:.0} ms · {size}\n"));
        }
    }
    output.push('\n');

    let mut headers: Vec<_> = response.headers.iter().collect();
    headers.sort_by_key(|(name, _)| *name);
    for (name, value) in headers {
        output.push_str(name);
        output.push_str(": ");
        match value.as_str() {
            Some(text) => output.push_str(text),
            None => output.push_str(&value.to_string()),
        }
        output.push('\n');
    }

    if matches!(view, View::Full) && !response.body.is_empty() {
        output.push('\n');
        output.push_str(&response.body);
        if !response.body.ends_with('\n') {
            output.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{format, View};

    #[test]
    fn formats_status_headers_timing_and_body() {
        let exchange = serde_json::from_str(
            r#"{
                "requests": [{
                    "name": "GET example",
                    "response": {
                        "protocol": "HTTP/2",
                        "statusCode": 201,
                        "statusMessage": "Created",
                        "headers": {"content-type": "application/json"},
                        "body": "{\"id\":1}",
                        "timings": {"total": 8.4},
                        "meta": {"size": "8 B"}
                    }
                }]
            }"#,
        )
        .expect("fixture should deserialize");

        assert_eq!(
            format(&exchange, View::Full),
            "# GET example\n\nHTTP/2 201 Created\n# 8 ms · 8 B\n\ncontent-type: application/json\n\n{\"id\":1}\n"
        );
        assert_eq!(
            format(&exchange, View::HeadersOnly),
            "# GET example\n\nHTTP/2 201 Created\n# 8 ms · 8 B\n\ncontent-type: application/json\n"
        );
    }
}
