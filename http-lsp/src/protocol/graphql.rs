//! GraphQL over HTTP: the body is the query, optionally followed by a JSON variables object, and
//! is sent as `POST {query, variables, operationName}`.

use serde_json::{json, Map, Value};

use super::{PreparedBody, PreparedRequest};
use crate::syntax::{find_header, Header};

/// Turns a `GRAPHQL` request into the equivalent JSON `POST`.
pub fn into_http(mut request: PreparedRequest) -> Result<PreparedRequest, String> {
    let text = match &request.body {
        PreparedBody::Empty => return Err("GraphQL request has no query".to_owned()),
        PreparedBody::Bytes(bytes) => String::from_utf8(bytes.clone())
            .map_err(|_| "GraphQL query is not UTF-8 text".to_owned())?,
    };
    let (query, variables) = split_variables(&text);
    if query.is_empty() {
        return Err("GraphQL request has no query".to_owned());
    }
    let mut payload = Map::new();
    payload.insert("query".to_owned(), json!(query));
    if let Some(variables) = variables {
        payload.insert("variables".to_owned(), variables);
    }
    if let Some(operation) = operation_name(query) {
        payload.insert("operationName".to_owned(), json!(operation));
    }

    request.method = "POST".to_owned();
    request.body = PreparedBody::Bytes(Value::Object(payload).to_string().into_bytes());
    if find_header(&request.headers, "content-type").is_none() {
        request.headers.push(Header {
            name: "Content-Type".to_owned(),
            value: "application/json".to_owned(),
        });
    }
    Ok(request)
}

/// Finds a trailing JSON object by structure: the earliest line starting with `{` from which
/// the rest of the body parses as a JSON object. Query selection sets never parse as JSON.
pub fn split_variables(text: &str) -> (&str, Option<Value>) {
    let text = text.trim();
    if !text.ends_with('}') {
        return (text, None);
    }
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let indent = line.len() - line.trim_start().len();
        let start = offset + indent;
        offset += line.len();
        if start == 0 || !text[start..].starts_with('{') {
            continue;
        }
        if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(&text[start..]) {
            return (text[..start].trim_end(), Some(value));
        }
    }
    (text, None)
}

/// The first named operation (`query Name`, `mutation Name` or `subscription Name`) at the top
/// level, skipping strings, comments and fragment definitions.
pub fn operation_name(query: &str) -> Option<String> {
    let mut chars = query.char_indices().peekable();
    let mut depth = 0usize;
    let mut expect_name = false;
    // Inside `fragment Name on Type { … }`, whose name and type may look like keywords.
    let mut in_fragment = false;
    while let Some((index, c)) = chars.next() {
        match c {
            '#' => {
                for (_, c) in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '"' => {
                let block = query[index..].starts_with("\"\"\"");
                if block {
                    chars.next();
                    chars.next();
                    let rest = &query[index + 3..];
                    let end = rest
                        .find("\"\"\"")
                        .map_or(query.len(), |end| index + 3 + end + 3);
                    while chars.peek().is_some_and(|(next, _)| *next < end) {
                        chars.next();
                    }
                } else {
                    let mut escaped = false;
                    for (_, c) in chars.by_ref() {
                        match c {
                            '\\' if !escaped => escaped = true,
                            '"' if !escaped => break,
                            _ => escaped = false,
                        }
                    }
                }
                expect_name = false;
            }
            '{' | '(' => {
                depth += 1;
                expect_name = false;
            }
            '}' | ')' => {
                depth = depth.saturating_sub(1);
                if c == '}' && depth == 0 {
                    in_fragment = false;
                }
            }
            c if c == '_' || c.is_ascii_alphabetic() => {
                let mut end = index + c.len_utf8();
                while let Some((next, c)) = chars.peek() {
                    if *c == '_' || c.is_ascii_alphanumeric() {
                        end = next + c.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }
                let word = &query[index..end];
                if depth == 0 && !in_fragment {
                    if expect_name {
                        return Some(word.to_owned());
                    }
                    in_fragment = word == "fragment";
                    expect_name = matches!(word, "query" | "mutation" | "subscription");
                }
            }
            c if c.is_whitespace() || c == ',' => {}
            _ => expect_name = false,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::Directives;

    #[test]
    fn splits_trailing_variables_structurally() {
        let (query, variables) = split_variables("query { viewer { id } }\n\n{\"id\": 1}\n");
        assert_eq!(query, "query { viewer { id } }");
        assert_eq!(variables, Some(json!({ "id": 1 })));

        // Blank lines inside the query and the variables do not matter.
        let text = "query Q($id: ID!) {\n\n  node(id: $id) {\n    id\n  }\n}\n{\n\n  \"id\": {\n    \"x\": 1\n  }\n}";
        let (query, variables) = split_variables(text);
        assert!(query.ends_with("  }\n}"), "{query}");
        assert_eq!(variables, Some(json!({ "id": { "x": 1 } })));

        let (query, variables) = split_variables("{\n  viewer {\n    id\n  }\n}");
        assert_eq!(query, "{\n  viewer {\n    id\n  }\n}");
        assert_eq!(variables, None);
    }

    #[test]
    fn finds_the_operation_name() {
        assert_eq!(
            operation_name("# query Commented\nquery GetUser($id: ID!) { user(id: $id) { name } }")
                .as_deref(),
            Some("GetUser")
        );
        assert_eq!(
            operation_name("fragment F on User { id }\nmutation Save { save(note: \"query X\") }")
                .as_deref(),
            Some("Save")
        );
        assert_eq!(
            operation_name(
                "fragment query on User @include(if: true) { id }\nquery Real { me { ...query } }"
            )
            .as_deref(),
            Some("Real")
        );
        assert_eq!(operation_name("query { viewer { id } }"), None);
        assert_eq!(operation_name("{ viewer { id } }"), None);
    }

    #[test]
    fn converts_to_a_json_post() {
        let request = PreparedRequest {
            method: "GRAPHQL".to_owned(),
            url: "http://example.test/graphql".to_owned(),
            http_version: None,
            headers: Vec::new(),
            body: PreparedBody::Bytes(b"query Me { me { id } }\n\n{\"a\": true}".to_vec()),
            directives: Directives::default(),
        };
        let request = into_http(request).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(
            find_header(&request.headers, "content-type"),
            Some("application/json")
        );
        let PreparedBody::Bytes(body) = request.body else {
            panic!("expected a body");
        };
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body,
            json!({ "query": "query Me { me { id } }", "variables": { "a": true }, "operationName": "Me" })
        );
    }
}
