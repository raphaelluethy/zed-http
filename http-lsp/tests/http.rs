#[allow(dead_code)]
mod common;

use common::{start_server, Workspace};
use zed_http_lsp::report::{OutputView, Report};

fn output(report: &Report) -> String {
    report.render(OutputView::Full)
}

#[tokio::test]
async fn sends_requests_with_environment_and_file_variables() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = "\
@path = json
@id = {{$random.integer(1, 2)}}

### Get JSON
GET {{baseUrl}}/{{path}}?id={{id}}

### Echo
POST {{baseUrl}}/echo
X-Token: {{token}}
Content-Type: application/json

{\"name\": \"{{path}}\"}
";
    let report = workspace.run(&runner, text, None).await;
    assert_eq!(report.summary().executed, 2);
    assert_eq!(report.summary().failed, 0, "{}", output(&report));

    let get = &report.executions[0];
    assert_eq!(get.block_name, "Get JSON");
    assert_eq!(get.status, Some(200));
    assert_eq!(
        get.request.as_ref().unwrap().url,
        format!("http://{address}/json?id=1")
    );
    let rendered = output(&report);
    assert!(rendered.contains("HTTP/1.1 200 OK"), "{rendered}");
    assert!(
        rendered.contains("{\n  \"hello\": \"world\","),
        "{rendered}"
    );

    let echo = report.executions[1]
        .body
        .as_ref()
        .unwrap()
        .content
        .clone()
        .unwrap();
    assert_eq!(echo["method"], "POST");
    assert_eq!(echo["token"], "env-token");
    assert_eq!(echo["contentType"], "application/json");
    assert_eq!(echo["body"], "{\"name\": \"json\"}");
}

#[tokio::test]
async fn selects_the_block_containing_the_line() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = "GET {{baseUrl}}/status/201\n\n###\n# comment\nGET {{baseUrl}}/status/404\n";
    let report = workspace.run(&runner, text, Some(3)).await;
    assert_eq!(report.executions.len(), 1);
    assert_eq!(report.executions[0].status, Some(404));
    assert_eq!(report.summary().failed, 1);

    let path = workspace.path();
    let error = runner
        .run(&path, "\n\n", Some(1), Some("test"))
        .await
        .unwrap_err();
    assert!(error.contains("no request"), "{error}");
}

#[tokio::test]
async fn follows_redirects_unless_disabled() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = "GET {{baseUrl}}/redirect\n\n###\n# @no-redirect\nGET {{baseUrl}}/redirect\n";
    let report = workspace.run(&runner, text, None).await;
    assert_eq!(report.executions[0].status, Some(200));
    assert_eq!(report.executions[0].url, format!("http://{address}/json"));
    assert_eq!(report.executions[1].status, Some(303));
}

#[tokio::test]
async fn keeps_cookies_in_the_session_jar() {
    let address = start_server().await;
    let workspace = Workspace::new(address);

    let runner = workspace.runner();
    let text = "GET {{baseUrl}}/set-cookie\n\n###\nGET {{baseUrl}}/cookie\n";
    let report = workspace.run(&runner, text, None).await;
    assert!(
        output(&report).ends_with("session=abc\n"),
        "{}",
        output(&report)
    );

    // `@no-cookie-jar` does not save received cookies.
    let runner = workspace.runner();
    let text = "# @no-cookie-jar\nGET {{baseUrl}}/set-cookie\n\n###\nGET {{baseUrl}}/cookie\n";
    let report = workspace.run(&runner, text, None).await;
    assert!(output(&report).ends_with("none\n"), "{}", output(&report));
}

#[tokio::test]
async fn decompresses_and_summarises_bodies() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = "GET {{baseUrl}}/gzip\n\n###\nGET {{baseUrl}}/binary\n";
    let report = workspace.run(&runner, text, None).await;
    let rendered = output(&report);
    assert!(rendered.contains(&"compressed ".repeat(200)), "{rendered}");
    assert!(
        rendered.contains("<binary 4 bytes, application/octet-stream>"),
        "{rendered}"
    );
}

#[tokio::test]
async fn reports_failures_and_unresolved_variables_per_block() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = "\
GET {{baseUrl}}/json?missing={{missing}}

###
WEBSOCKET ws://{{baseUrl}}/socket

###
GET http://127.0.0.1:1/unreachable
";
    let report = workspace.run(&runner, text, None).await;
    assert_eq!(report.executions.len(), 3);
    let rendered = output(&report);
    assert!(
        rendered.contains("# warning: unresolved variable {{missing}}"),
        "{rendered}"
    );
    assert!(
        rendered.contains("error: WebSocket requests are not yet supported"),
        "{rendered}"
    );
    assert!(report.executions[2].error.is_some());
    assert_eq!(report.summary().failed, 2);
}
