#[allow(dead_code)]
mod common;

use std::time::{Duration, Instant};

use common::{script_engine, start_server, Workspace};
use zed_http_lsp::{
    report::OutputView,
    script::{Limits, ScriptInput},
};

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

#[tokio::test]
async fn kills_scripts_past_the_wall_clock_and_keeps_working() {
    // Generous enough for the trivial script below on slow machines; the nested loops run for
    // far longer than this.
    let engine = script_engine(Limits {
        wall_clock: Duration::from_secs(5),
        ..Limits::default()
    });
    // The loop limit is per call frame, so only killing the worker stops this.
    let nested = "function f() { for (let j = 0; j < 4e6; j++) {} } \
                  for (let i = 0; i < 4e6; i++) f();";
    let input = ScriptInput::default();
    let started = Instant::now();
    let (first, second) = tokio::join!(
        engine.run(nested.to_owned(), &input),
        engine.run(nested.to_owned(), &input),
    );
    for effects in [first, second] {
        assert!(effects.error.unwrap().contains("time limit"));
    }
    assert!(started.elapsed() < Duration::from_secs(15));

    // Both worker slots are free again.
    let effects = engine
        .run("client.log('still alive')".to_owned(), &input)
        .await;
    assert_eq!(effects.error, None);
    assert_eq!(effects.logs[0].message, "still alive");
}

#[tokio::test]
async fn decodes_declared_charsets_for_reports_and_scripts() {
    let address = start_server().await;
    let workspace = Workspace::new(address);
    let runner = workspace.runner();
    let text = "GET {{baseUrl}}/latin1\n> {% client.test(\"decoded\", () => client.assert(response.body === \"café\")); %}\n";
    let report = workspace.run(&runner, text, None).await;
    let output = report.render(OutputView::Full);
    assert!(output.ends_with("\ncafé\n"), "{output}");
    assert!(output.contains("# ✓ decoded"), "{output}");
}

#[tokio::test]
async fn bounds_script_results() {
    // Building 24 MiB of strings is slow in debug builds; this test is about size, not time.
    let engine = script_engine(Limits {
        wall_clock: Duration::from_secs(120),
        ..Limits::default()
    });
    let input = ScriptInput::default();

    let thrown = engine
        .run(
            "let s = 'x'; for (let i = 0; i < 18; i++) s += s; throw new Error(s);".to_owned(),
            &input,
        )
        .await;
    let error = thrown.error.unwrap();
    assert!(error.len() < 100 * 1024, "{}", error.len());
    assert!(
        error.ends_with("[truncated]"),
        "{}",
        &error[..200.min(error.len())]
    );

    let huge = engine
        .run(
            "let s = 'x'; for (let i = 0; i < 23; i++) s += s; \
             client.global.set('a', s); client.global.set('b', s); client.global.set('c', s);"
                .to_owned(),
            &input,
        )
        .await;
    assert!(huge.error.unwrap().contains("larger than 16 MiB"));
    assert!(huge.globals.is_empty());
}

/// A worker that floods stdout is killed as soon as it passes the limit instead of blocking
/// until the wall clock runs out. `sh -c 'exec yes'` floods without creating an executable,
/// which sibling tests forking concurrently could hold open (ETXTBSY).
#[cfg(unix)]
#[tokio::test]
async fn kills_workers_that_flood_stdout() {
    let engine = zed_http_lsp::script::ScriptEngine::with_command(
        Limits {
            wall_clock: Duration::from_secs(30),
            ..Limits::default()
        },
        std::path::PathBuf::from("sh"),
        vec!["-c".into(), "exec yes".into()],
    );
    let started = Instant::now();
    let effects = engine.run(String::new(), &ScriptInput::default()).await;
    let error = effects.error.unwrap();
    assert!(error.contains("larger than 16 MiB"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn explains_a_missing_worker() {
    let engine = zed_http_lsp::script::ScriptEngine::new(
        Limits::default(),
        std::path::PathBuf::from("/nonexistent/zed-http-lsp"),
    );
    let effects = engine
        .run("client.log(1)".to_owned(), &ScriptInput::default())
        .await;
    let error = effects.error.unwrap();
    assert!(
        error.contains("restart the zed-http-lsp language server"),
        "{error}"
    );
}
