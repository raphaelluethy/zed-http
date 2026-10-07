#[allow(dead_code)]
mod common;

use common::{start_server, Workspace};
use tokio::process::Command;

async fn run(workspace: &Workspace, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_zed-http-lsp"))
        .arg("--run")
        .arg(workspace.root.join("requests with spaces.http"))
        .args(args)
        .env("ZED_HTTP_ENV", "test")
        .env_remove("FORCE_COLOR")
        .env_remove("NO_COLOR")
        .current_dir(&workspace.root)
        .output()
        .await
        .unwrap()
}

#[tokio::test]
async fn gutter_row_selects_one_request_and_all_runs_every_request() {
    let workspace = Workspace::new(start_server().await);
    workspace.write(
        "requests with spaces.http",
        "GET {{baseUrl}}/status/404\n\n### Success\nGET {{baseUrl}}/json\n",
    );
    let selected = run(&workspace, &["--line", "4"]).await;
    let output = String::from_utf8_lossy(&selected.stdout);
    assert!(selected.status.success(), "{:?}", selected);
    assert!(output.contains("200 OK"), "{output}");
    assert!(!output.contains("404 Not Found"), "{output}");

    let all = run(&workspace, &[]).await;
    let output = String::from_utf8_lossy(&all.stdout);
    assert!(!all.status.success());
    assert!(output.contains("404 Not Found"), "{output}");
    assert!(output.contains("200 OK"), "{output}");
}

#[tokio::test]
async fn task_mode_runs_script_workers_and_rejects_invalid_rows() {
    let workspace = Workspace::new(start_server().await);
    workspace.write(
        "requests with spaces.http",
        "< {% request.variables.set('route', 'json'); %}\nGET {{baseUrl}}/{{route}}\n",
    );
    let scripted = run(&workspace, &["--line", "2"]).await;
    assert!(scripted.status.success(), "{:?}", scripted);
    assert!(String::from_utf8_lossy(&scripted.stdout).contains("200 OK"));

    let invalid = run(&workspace, &["--line", "0"]).await;
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("positive row"));
}

#[tokio::test]
async fn terminal_json_is_pretty_and_color_can_be_enabled_or_disabled() {
    let workspace = Workspace::new(start_server().await);
    let path = workspace.write(
        "requests with spaces.http",
        "### JSON response\nGET {{baseUrl}}/json\n",
    );
    let plain = run(&workspace, &[]).await;
    let text = String::from_utf8_lossy(&plain.stdout);
    assert!(plain.status.success());
    assert!(text.contains("✓ JSON response"), "{text}");
    assert!(text.contains("\n  \"hello\": \"world\""), "{text}");
    assert!(!text.contains('\x1b'));

    for no_color in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_zed-http-lsp"));
        command
            .arg("--run")
            .arg(&path)
            .env("ZED_HTTP_ENV", "test")
            .env("FORCE_COLOR", "1")
            .current_dir(&workspace.root);
        if no_color {
            command.env("NO_COLOR", "1");
        } else {
            command.env_remove("NO_COLOR");
        }
        let output = command.output().await.unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(text.contains('\x1b'), !no_color, "{text}");
        if !no_color {
            assert!(text.contains("\x1b[36m\"hello\"\x1b[0m"), "{text}");
            assert!(text.contains("\x1b[32m\"world\"\x1b[0m"), "{text}");
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn packaged_task_finds_the_installed_dev_adapter_without_path_setup() {
    let workspace = Workspace::new(start_server().await);
    let path = workspace.write("requests with spaces.http", "GET {{baseUrl}}/json\n");
    let installed = workspace
        .root
        .join("Library/Application Support/Zed/extensions/installed");
    std::fs::create_dir_all(&installed).unwrap();
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    std::os::unix::fs::symlink(repository, installed.join("http")).unwrap();
    let templates: serde_json::Value =
        serde_json::from_str(include_str!("../../languages/http/tasks.json")).unwrap();
    let task = &templates[0];
    // Zed joins command/args into shell text. Exercise that exact boundary,
    // rather than quoting each argument as a direct process invocation would.
    let shell = task["shell"]["program"].as_str().unwrap();
    assert_eq!(shell, "/bin/sh");
    let output = Command::new(shell)
        .arg("-c")
        .arg(task["command"].as_str().unwrap())
        .env("HTTP_REQUEST_FILE", &path)
        .env("HTTP_REQUEST_ROW", "1")
        .env("HOME", &workspace.root)
        .env("ZED_HTTP_ENV", "test")
        .env_remove("ZED_HTTP_LSP")
        .current_dir(&workspace.root)
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("200 OK"));
}

/// Starts the language server for `root` and waits until it has answered `initialize` and
/// received `initialized`, after which it accepts forwarded task requests.
#[cfg(unix)]
async fn start_language_server(root: &std::path::Path) -> tokio::process::Child {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let mut child = Command::new(env!("CARGO_BIN_EXE_zed-http-lsp"))
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let send = |message: serde_json::Value| {
        let body = message.to_string();
        format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    };
    let root_uri = format!("file://{}", root.display());
    stdin
        .write_all(&send(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "processId": null, "rootUri": root_uri, "capabilities": {} }
        })))
        .await
        .unwrap();
    // Read the initialize response.
    let mut length = 0;
    loop {
        let mut header = String::new();
        stdout.read_line(&mut header).await.unwrap();
        if let Some(value) = header.strip_prefix("Content-Length: ") {
            length = value.trim().parse().unwrap();
        }
        if header == "\r\n" {
            break;
        }
    }
    let mut body = vec![0; length];
    stdout.read_exact(&mut body).await.unwrap();
    stdin
        .write_all(&send(serde_json::json!({
            "jsonrpc": "2.0", "method": "initialized", "params": {}
        })))
        .await
        .unwrap();
    // Keep the pipes open for the server's lifetime.
    tokio::spawn(async move {
        let _stdin = stdin;
        let mut sink = Vec::new();
        stdout.read_to_end(&mut sink).await.ok();
    });
    child
}

#[cfg(unix)]
#[tokio::test]
async fn gutter_runs_share_the_language_servers_session_until_it_exits() {
    let workspace = Workspace::new(start_server().await);
    workspace.write(
        "requests with spaces.http",
        "### Login\nGET {{baseUrl}}/set-cookie\n> {% client.global.set(\"shared\", \"from-login\"); %}\n\n\
         ### Use\nPOST {{baseUrl}}/echo\nX-Token: {{shared}}\n\n\
         ### Cookie\nGET {{baseUrl}}/cookie\n",
    );
    let mut server = start_language_server(&workspace.root).await;

    // The server binds its socket after `initialized`; retry until the run is forwarded.
    let mut forwarded = false;
    for _ in 0..50 {
        let login = run(&workspace, &["--line", "2"]).await;
        assert!(login.status.success(), "{login:?}");
        if login.stderr.is_empty() {
            forwarded = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        forwarded,
        "the language server never accepted a forwarded run"
    );

    let used = run(&workspace, &["--line", "6"]).await;
    let output = String::from_utf8_lossy(&used.stdout);
    assert!(used.status.success(), "{used:?}");
    assert!(output.contains("\"token\": \"from-login\""), "{output}");

    let cookie = run(&workspace, &["--line", "10"]).await;
    assert!(String::from_utf8_lossy(&cookie.stdout).contains("session=abc"));

    // Like a session cookie, everything is gone once the language server exits.
    server.kill().await.unwrap();
    server.wait().await.unwrap();
    let fresh = run(&workspace, &["--line", "10"]).await;
    assert!(String::from_utf8_lossy(&fresh.stdout).contains("none"));
    assert!(String::from_utf8_lossy(&fresh.stderr).contains("not running"));
}
