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

#[tokio::test]
async fn asynchronous_test_callbacks_fail_the_run() {
    let workspace = Workspace::new(start_server().await);
    workspace.write(
        "requests with spaces.http",
        "GET {{baseUrl}}/json\n> {%\nclient.test('must fail', async () => { client.assert(false, 'intentional failure'); });\n%}\n",
    );
    let result = run(&workspace, &[]).await;
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(!result.status.success(), "{result:?}");
    assert!(output.contains("✗ must fail"), "{output}");
    assert!(!output.contains("✓ must fail"), "{output}");
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

#[cfg(unix)]
fn launcher_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        _ => "unknown-target",
    }
}

#[cfg(unix)]
fn record_key(root: &str) -> String {
    let hash = root
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    format!("{hash:016x}.path")
}

#[cfg(unix)]
struct LauncherHome {
    root: std::path::PathBuf,
    work: std::path::PathBuf,
}

#[cfg(unix)]
fn launcher_home(workspace: &Workspace) -> LauncherHome {
    let home = workspace.root.join("launcher-home");
    let zed = home.join("Library/Application Support/Zed");
    let installed = zed.join("extensions/installed/http");
    let languages = installed.join("languages/http");
    std::fs::create_dir_all(&languages).unwrap();
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    std::fs::copy(
        repository.join("languages/http/run_http.sh"),
        languages.join("run_http.sh"),
    )
    .unwrap();
    std::fs::write(installed.join("extension.toml"), "version = \"9.9.9\"\n").unwrap();
    let work = zed.join("extensions/work/http");
    std::fs::create_dir_all(&work).unwrap();
    LauncherHome { root: home, work }
}

#[cfg(unix)]
impl LauncherHome {
    fn write_adapter(&self, name: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let directory = self.work.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        let binary = directory.join("zed-http-lsp");
        std::fs::write(&binary, format!("#!/bin/sh\necho 'adapter {name}'\n")).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        binary
    }

    fn write_record(&self, root: &str, adapter: &str) {
        let records = self.work.join("active-servers");
        std::fs::create_dir_all(&records).unwrap();
        std::fs::write(
            records.join(record_key(root)),
            format!("{root}\n{adapter}\n"),
        )
        .unwrap();
    }
}

#[cfg(unix)]
async fn run_launcher_task(
    workspace: &Workspace,
    home: &LauncherHome,
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let path = workspace.write("requests with spaces.http", "GET {{baseUrl}}/json\n");
    let templates: serde_json::Value =
        serde_json::from_str(include_str!("../../languages/http/tasks.json")).unwrap();
    let task = &templates[0];
    let mut command = Command::new(task["shell"]["program"].as_str().unwrap());
    command
        .arg("-c")
        .arg(task["command"].as_str().unwrap())
        .env("HTTP_REQUEST_FILE", &path)
        .env("HTTP_REQUEST_ROW", "1")
        .env("HOME", &home.root)
        .env("ZED_HTTP_ENV", "test")
        .env_remove("ZED_HTTP_LSP")
        .current_dir(&workspace.root);
    for (name, value) in extra_env {
        command.env(name, value);
    }
    command.output().await.unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn launcher_falls_back_to_newest_numeric_install() {
    let workspace = Workspace::new(start_server().await);
    let home = launcher_home(&workspace);
    home.write_adapter(&format!("zed-http-lsp-{}-9.9.8", launcher_target()));
    let output = run_launcher_task(&workspace, &home, &[]).await;
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(text.contains("adapter zed-http-lsp"), "{text}");
    assert!(text.contains("9.9.8"), "{text}");

    home.write_adapter(&format!("zed-http-lsp-{}-9.10.0", launcher_target()));
    home.write_adapter(&format!("zed-http-lsp-{}-other", launcher_target()));
    home.write_adapter(&format!(
        "zed-http-lsp-{}-99.0.0",
        if launcher_target() == "aarch64-apple-darwin" {
            "x86_64-apple-darwin"
        } else {
            "aarch64-apple-darwin"
        }
    ));
    let output = run_launcher_task(&workspace, &home, &[]).await;
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(text.contains("9.10.0"), "{text}");
}

#[cfg(unix)]
#[tokio::test]
async fn launcher_prefers_the_recorded_adapter_for_this_workspace() {
    let workspace = Workspace::new(start_server().await);
    let home = launcher_home(&workspace);
    let spaced = home.work.join("adapters/with spaces");
    std::fs::create_dir_all(&spaced).unwrap();
    let recorded = spaced.join("zed-http-lsp");
    std::fs::write(&recorded, "#!/bin/sh\necho 'recorded adapter'\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&recorded, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let root = workspace.root.to_string_lossy().into_owned();
    home.write_record(&root, &recorded.to_string_lossy());
    home.write_record("/nonexistent/other-workspace", "/nonexistent/adapter");
    let records = home.work.join("active-servers");
    std::fs::write(records.join("deadbeefdeadbeef.path"), "\n").unwrap();
    std::fs::write(records.join("aaaaaaaaaaaaaaaa.tmp"), "tmp\n").unwrap();

    let output = run_launcher_task(&workspace, &home, &[]).await;
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(text.contains("recorded adapter"), "{text}");
}

#[cfg(unix)]
#[tokio::test]
async fn launcher_ignores_a_stale_record_and_uses_the_extension_version() {
    let workspace = Workspace::new(start_server().await);
    let home = launcher_home(&workspace);
    let root = workspace.root.to_string_lossy().into_owned();
    home.write_record(&root, &format!("{}/gone", home.work.display()));
    home.write_adapter(&format!("zed-http-lsp-{}-9.9.9", launcher_target()));
    let output = run_launcher_task(&workspace, &home, &[]).await;
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(text.contains("9.9.9"), "{text}");

    let relative = home.write_adapter(&format!("zed-http-lsp-{}-9.9.7", launcher_target()));
    let relative_name = relative
        .strip_prefix(&home.work)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    home.write_record(&root, &relative_name);
    let output = run_launcher_task(&workspace, &home, &[]).await;
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(text.contains("9.9.7"), "{text}");
}

#[cfg(unix)]
#[tokio::test]
async fn launcher_explicit_override_wins_and_bad_override_fails() {
    let workspace = Workspace::new(start_server().await);
    let home = launcher_home(&workspace);
    let explicit = home.write_adapter("explicit/zed-http-lsp");
    home.write_adapter(&format!("zed-http-lsp-{}-9.9.9", launcher_target()));

    let output = run_launcher_task(
        &workspace,
        &home,
        &[("ZED_HTTP_LSP", &explicit.to_string_lossy())],
    )
    .await;
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(text.contains("adapter explicit"), "{text}");

    let output =
        run_launcher_task(&workspace, &home, &[("ZED_HTTP_LSP", "/missing/adapter")]).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("adapter not found"));
}

#[cfg(unix)]
#[tokio::test]
async fn launcher_record_wins_over_a_dev_checkout() {
    let workspace = Workspace::new(start_server().await);
    let home = launcher_home(&workspace);
    let installed = home
        .root
        .join("Library/Application Support/Zed/extensions/installed/http");
    std::fs::remove_dir_all(&installed).unwrap();
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    std::os::unix::fs::symlink(repository, &installed).unwrap();
    let recorded = home.write_adapter("recorded/zed-http-lsp");
    let root = workspace.root.to_string_lossy().into_owned();
    home.write_record(&root, &recorded.to_string_lossy());
    let output = run_launcher_task(&workspace, &home, &[]).await;
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(text.contains("adapter recorded"), "{text}");
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
