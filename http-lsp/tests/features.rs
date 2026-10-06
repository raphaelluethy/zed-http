#[allow(dead_code)]
mod common;

use std::fs;

use common::{start_server, Workspace};
use serde_json::{json, Value};
use zed_http_lsp::report::{OutputView, Report};

fn output(report: &Report) -> String {
    report.render(OutputView::Full)
}

fn content(report: &Report, index: usize) -> Value {
    report.executions[index]
        .body
        .as_ref()
        .and_then(|body| body.content.clone())
        .unwrap_or_else(|| panic!("no JSON body\n{}", output(report)))
}

#[tokio::test]
async fn sends_multipart_bodies_with_file_includes() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    workspace.write("upload.txt", "uploaded {{token}}");
    let runner = workspace.runner();
    let text = "\
POST {{baseUrl}}/multipart
Content-Type: multipart/form-data; boundary=Boundary

--Boundary
Content-Disposition: form-data; name=\"field\"

value {{token}}
--Boundary
Content-Disposition: form-data; name=\"file\"; filename=\"upload.txt\"
Content-Type: text/plain

< ./upload.txt
--Boundary--
";
    let report = workspace.run(&runner, text, None).await;
    assert_eq!(
        content(&report, 0),
        json!([
            { "name": "field", "fileName": null, "contentType": null, "text": "value env-token" },
            { "name": "file", "fileName": "upload.txt", "contentType": "text/plain", "text": "uploaded {{token}}" },
        ])
    );
}

#[tokio::test]
async fn includes_files_with_and_without_substitution() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    workspace.write("raw.json", "{\"raw\": \"{{token}}\"}");
    workspace.write("template.json", "{\"template\": \"{{token}}\"}");
    let runner = workspace.runner();
    let text = "POST {{baseUrl}}/echo\n\n< ./raw.json\n<@ ./template.json\n";
    let report = workspace.run(&runner, text, None).await;
    assert_eq!(
        content(&report, 0)["body"],
        "{\"raw\": \"{{token}}\"}\n{\"template\": \"env-token\"}"
    );
}

#[tokio::test]
async fn sends_graphql_queries_with_variables_and_operation_name() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = "\
GRAPHQL {{baseUrl}}/graphql

query Viewer($id: ID!) {

  node(id: $id) { id }
}

{
  \"id\": \"{{token}}\"
}
";
    let report = workspace.run(&runner, text, None).await;
    let rendered = output(&report);
    assert!(
        rendered.contains(&format!("# GRAPHQL http://{address}/graphql")),
        "{rendered}"
    );
    assert_eq!(
        content(&report, 0)["data"],
        json!({
            "query": "query Viewer($id: ID!) {\n\n  node(id: $id) { id }\n}",
            "variables": { "id": "env-token" },
            "operationName": "Viewer",
        })
    );
}

#[tokio::test]
async fn keeps_cookies_set_during_redirects() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let report = workspace
        .run(&runner, "GET {{baseUrl}}/login\n", None)
        .await;
    assert_eq!(report.executions[0].url, format!("http://{address}/cookie"));
    assert!(
        output(&report).ends_with("login=yes\n"),
        "{}",
        output(&report)
    );
}

#[tokio::test]
async fn applies_timeouts_and_no_log() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = "# @timeout 200ms\nGET {{baseUrl}}/slow\n\n###\n# @no-log\nGET {{baseUrl}}/json\n";
    let report = workspace.run(&runner, text, None).await;
    let rendered = output(&report);
    assert!(report.executions[0].error.is_some(), "{rendered}");
    assert!(
        rendered.contains("<response body not logged (@no-log)>"),
        "{rendered}"
    );
    assert!(!rendered.contains("\"hello\""), "{rendered}");
}

#[tokio::test]
async fn redirects_binary_responses_to_files() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = "GET {{baseUrl}}/binary\n>> ./out/body.bin\n\n###\nGET {{baseUrl}}/json\n>>! ./out/body.json\n";
    workspace.run(&runner, text, None).await;
    let report = workspace.run(&runner, text, None).await;
    let rendered = output(&report);
    assert!(rendered.contains("# saved response body to "), "{rendered}");
    assert_eq!(
        fs::read(workspace.root.join("out/body.bin")).unwrap(),
        vec![0u8, 159, 146, 150]
    );
    // `>>` picks a fresh name the second time; `>>!` overwrites.
    assert!(workspace.root.join("out/body-1.bin").exists());
    assert!(!workspace.root.join("out/body-1.json").exists());
    let saved: Value =
        serde_json::from_slice(&fs::read(workspace.root.join("out/body.json")).unwrap()).unwrap();
    assert_eq!(saved["hello"], "world");
}

#[tokio::test]
async fn runs_named_requests_from_imports_and_resolves_response_references() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    workspace.write(
        "auth.http",
        "import ./requests.http\n\n### Login\n# @name login\nGET {{baseUrl}}/json?user={{user}}\n",
    );
    let runner = workspace.runner();
    let text = "\
import ./auth.http
@user = nobody

run #login (@user=admin)

### Use it
POST {{baseUrl}}/echo
X-Token: {{login.response.body.$.hello}}-{{login.response.headers.CONTENT-TYPE}}

###
run #missing
";
    let report = workspace.run(&runner, text, None).await;
    let rendered = output(&report);
    assert_eq!(report.executions.len(), 3, "{rendered}");
    assert_eq!(
        report.executions[0].request.as_ref().unwrap().url,
        format!("http://{address}/json?user=admin")
    );
    assert_eq!(content(&report, 1)["token"], "world-application/json");
    // The import cycle back to requests.http ends the search instead of looping.
    assert!(
        rendered.contains("error: no request named \"missing\" in this file or its imports"),
        "{rendered}"
    );

    // Send on a `run` line executes just that command.
    let report = workspace.run(&runner, text, Some(3)).await;
    assert_eq!(report.executions.len(), 1);
    assert_eq!(report.executions[0].block_name, "login");
}

#[tokio::test]
async fn runs_other_files_and_stops_run_cycles() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    workspace.write(
        "setup.http",
        "@kind = setup\nGET {{baseUrl}}/json?from={{kind}}\n\n###\nrun ./requests.http\n",
    );
    let runner = workspace.runner();
    let text = "run ./setup.http\n\n###\nGET {{baseUrl}}/status/204\n";
    let report = workspace.run(&runner, text, Some(0)).await;
    let rendered = output(&report);
    assert_eq!(
        report.executions[0].request.as_ref().unwrap().url,
        format!("http://{address}/json?from=setup"),
        "{rendered}"
    );
    assert!(
        rendered.contains("run cycles are not allowed"),
        "{rendered}"
    );
    assert_eq!(report.executions.len(), 2, "{rendered}");
}

#[tokio::test]
async fn scopes_run_overrides_to_nested_commands() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    workspace.write(
        "child.http",
        "### Req\nGET {{baseUrl}}/json?token={{token}}\n\n###\nrun #Req\n\n###\nrun #Req (@token=inner)\n\n###\nrun #Req\n",
    );
    let runner = workspace.runner();
    let report = workspace
        .run(
            &runner,
            "run ./child.http (@token=outer)\n\n###\nGET {{baseUrl}}/json?token={{token}}\n",
            None,
        )
        .await;
    let tokens: Vec<_> = report
        .executions
        .iter()
        .map(|execution| {
            let url = &execution.request.as_ref().unwrap().url;
            url.rsplit_once("token=").unwrap().1.to_owned()
        })
        .collect();
    assert_eq!(
        tokens,
        vec!["outer", "outer", "inner", "outer", "env-token"]
    );
}
