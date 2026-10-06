#[allow(dead_code)]
mod common;

use common::{start_server, Workspace};
use zed_http_lsp::report::OutputView;

#[tokio::test]
async fn scripts_prepare_requests_and_carry_globals_between_them() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    workspace.write(
        "check.js",
        r#"client.test("echoed the global", () => client.assert(response.body.token === "world"));"#,
    );
    let runner = workspace.runner();
    let text = r#"
### Login
< {%
  request.variables.set("path", "json");
  client.log("preparing", request.url);
%}
GET {{baseUrl}}/{{path}}

> {%
  client.global.set("auth", response.body.hello);
  client.test("status is 200", () => client.assert(response.status === 200, "status"));
%}

### Use the token
POST {{baseUrl}}/echo
X-Token: {{auth}}

> ./check.js
> {% client.test("fails", () => client.assert(false, "expected failure")); %}
"#;
    let report = workspace.run(&runner, text, None).await;
    let output = report.render(OutputView::Full);
    assert_eq!(report.executions[0].status, Some(200), "{output}");
    assert!(
        output.contains(&format!(
            "# [script:log] preparing http://{address}/{{{{path}}}}"
        )),
        "{output}"
    );
    assert!(output.contains("# ✓ status is 200"), "{output}");
    assert!(output.contains("# ✓ echoed the global"), "{output}");
    assert!(output.contains("# ✗ fails: expected failure"), "{output}");
    assert_eq!(report.summary().failed, 1);

    // Globals persist in the session for later runs.
    let report = workspace
        .run(&runner, "POST {{baseUrl}}/echo\nX-Token: {{auth}}\n", None)
        .await;
    let echo = report.executions[0]
        .body
        .as_ref()
        .unwrap()
        .content
        .clone()
        .unwrap();
    assert_eq!(echo["token"], "world");
}

#[tokio::test]
async fn failing_scripts_are_reported() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = "\
< {% client.global.set(\"kept\", 1); throw new Error(\"no way\"); %}
GET {{baseUrl}}/json

###
< ./missing.js
GET {{baseUrl}}/json

###
GET {{baseUrl}}/json
> {% response.body.missing.field %}
";
    let report = workspace.run(&runner, text, None).await;
    let output = report.render(OutputView::Full);
    assert!(
        output.contains("error: pre-request script failed: Error: no way"),
        "{output}"
    );
    assert!(output.contains("failed to read script"), "{output}");
    assert!(report.executions[0].status.is_none());
    assert_eq!(report.executions[2].status, Some(200));
    assert!(output.contains("# [script:error] TypeError"), "{output}");
    assert_eq!(report.summary().failed, 3);

    // Effects made before the exception are kept.
    let report = workspace
        .run(&runner, "POST {{baseUrl}}/echo\nX-Token: {{kept}}\n", None)
        .await;
    let echo = report.executions[0]
        .body
        .as_ref()
        .unwrap()
        .content
        .clone()
        .unwrap();
    assert_eq!(echo["token"], "1");
}
