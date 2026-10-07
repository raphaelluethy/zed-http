#[allow(dead_code)]
mod common;

use axum::{
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use common::Workspace;
use serde_json::{json, Value};

/// The same seven-request workflow as spring-test/api.http, against an isolated server.
/// A fresh random token ensures the bearer value actually comes from the JSON response.
#[tokio::test]
async fn sign_in_token_works_for_run_all_and_separate_runs() {
    let token = uuid::Uuid::new_v4().to_string();
    let login_token = token.clone();
    let app = Router::new()
        .route(
            "/api/login",
            post(move |Json(body): Json<Value>| {
                let token = login_token.clone();
                async move {
                    if body == json!({"username": "user", "password": "password"}) {
                        (StatusCode::OK, Json(json!({"token": token})))
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid credentials"})),
                        )
                    }
                }
            }),
        )
        .route(
            "/api/editor-test",
            get(move |headers: HeaderMap| {
                let bearer = format!("Bearer {token}");
                async move {
                    let authorization = headers
                        .get("authorization")
                        .and_then(|header| header.to_str().ok());
                    if authorization == Some(bearer.as_str())
                        || authorization == Some("Basic dXNlcjpwYXNzd29yZA==")
                    {
                        StatusCode::OK
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let workspace = Workspace::new(address);
    let text = include_str!("fixtures/authentication.http")
        .replace("http://localhost:8080", &format!("http://{address}"));

    let all = workspace.run(&workspace.runner(), &text, None).await;
    let statuses: Vec<_> = all
        .executions
        .iter()
        .map(|execution| execution.status)
        .collect();
    assert_eq!(
        statuses,
        [
            Some(200),
            Some(200),
            Some(401),
            Some(401),
            Some(401),
            Some(200),
            Some(401)
        ]
    );
    assert!(all.executions[0].success, "{:?}", all.executions[0]);
    assert!(all
        .executions
        .iter()
        .all(|execution| execution.warnings.is_empty()));

    let runner = workspace.runner();
    let document = zed_http_lsp::syntax::parse(&text);
    assert!(document.errors.is_empty());
    let login = workspace
        .run(&runner, &text, Some(document.blocks[0].start_line))
        .await;
    assert!(login.executions[0].success, "{:?}", login.executions[0]);
    let bearer = workspace
        .run(&runner, &text, Some(document.blocks[1].start_line))
        .await;
    assert_eq!(bearer.executions[0].status, Some(200));
    assert!(bearer.executions[0].warnings.is_empty());
    server.abort();
}
