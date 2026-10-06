//! Pre-request scripts and response handlers on the embedded Boa JavaScript engine, with the
//! IntelliJ `client` / `request` / `response` API.
//!
//! Every script runs in a fresh Boa context inside a short-lived worker process: this same
//! executable started with [`WORKER_FLAG`], with JSON on stdin and [`ScriptEffects`] JSON on
//! stdout. Boa has no interrupt hook and its loop limit is per call frame, so nested loops can
//! outlast any in-process guard; a worker past the wall clock is killed instead. Boa's runtime
//! limits (loop iterations, recursion) still end most runaway scripts early. Scripts get no
//! filesystem, network or environment access.

use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    io::{self, Read as _, Write as _},
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use base64::Engine as _;
use boa_engine::{
    js_string, property::Attribute, Context, JsArgs, JsError, JsNativeError, JsResult, JsString,
    JsValue, NativeFunction, Source,
};
use md5::Md5;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json_path::JsonPath;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Semaphore,
    time::{timeout_at, Instant},
};

use crate::{session::GlobalChange, variables::dynamic_variable};

const PRELUDE: &str = include_str!("script_prelude.js");
pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_EFFECTS_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONCURRENT_SCRIPTS: usize = 2;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const MAX_WORKER_INPUT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_WORKER_DIAGNOSTICS: u64 = 64 * 1024;
/// The first argument that turns the executable into a script worker.
pub const WORKER_FLAG: &str = "--script-worker";

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub wall_clock: Duration,
    pub loop_iterations: u64,
    pub recursion: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            wall_clock: Duration::from_secs(10),
            loop_iterations: 5_000_000,
            recursion: 256,
        }
    }
}

/// What a script can see. The response is only present for response handlers.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptInput {
    pub globals: BTreeMap<String, Value>,
    pub request_variables: BTreeMap<String, String>,
    pub environment: BTreeMap<String, String>,
    pub request: ScriptRequest,
    pub response: Option<ScriptResponse>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScriptRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptResponse {
    pub status: Option<u16>,
    pub headers: Vec<(String, String)>,
    /// Parsed JSON for JSON responses, otherwise the body text.
    pub body: Value,
    pub content_type: Option<String>,
}

/// Everything a script did, for the runner to apply. Effects made before an exception are kept
/// and `error` is set.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScriptEffects {
    /// `client.global` mutations in call order.
    pub globals: Vec<GlobalChange>,
    pub request_variables: BTreeMap<String, String>,
    pub logs: Vec<ScriptLog>,
    pub tests: Vec<ScriptTest>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptLog {
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptTest {
    pub name: String,
    pub passed: bool,
    #[serde(default)]
    pub message: Option<String>,
}

impl ScriptEffects {
    fn failed(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::default()
        }
    }
}

pub struct ScriptEngine {
    limits: Limits,
    /// The worker executable, or why it could not be located.
    worker: Result<PathBuf, String>,
    worker_args: Vec<OsString>,
    workers: Arc<Semaphore>,
}

impl Default for ScriptEngine {
    /// Uses the running executable as the worker, resolved once to its real path so a symlinked
    /// install keeps working. It is never looked up on `PATH`.
    fn default() -> Self {
        let worker = env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|error| {
                format!("cannot locate the zed-http-lsp executable to run scripts: {error}")
            });
        Self {
            limits: Limits::default(),
            worker,
            worker_args: vec![OsString::from(WORKER_FLAG)],
            workers: Arc::new(Semaphore::new(MAX_CONCURRENT_SCRIPTS)),
        }
    }
}

/// What the parent sends a worker on stdin.
#[derive(Serialize, Deserialize)]
struct WorkerRequest {
    source: String,
    /// The JSON-encoded [`ScriptInput`].
    input: String,
    limits: Limits,
}

impl ScriptEngine {
    /// `worker` is an executable that runs [`worker_main`] when given [`WORKER_FLAG`].
    pub fn new(limits: Limits, worker: PathBuf) -> Self {
        Self::with_command(limits, worker, vec![OsString::from(WORKER_FLAG)])
    }

    /// Runs workers as `program args…` instead of `worker --script-worker`.
    pub fn with_command(limits: Limits, program: PathBuf, args: Vec<OsString>) -> Self {
        Self {
            limits,
            worker: Ok(program),
            worker_args: args,
            workers: Arc::new(Semaphore::new(MAX_CONCURRENT_SCRIPTS)),
        }
    }

    pub async fn run(&self, source: String, input: &ScriptInput) -> ScriptEffects {
        if source.len() > MAX_SOURCE_BYTES {
            return ScriptEffects::failed(format!(
                "script is larger than {} KiB",
                MAX_SOURCE_BYTES / 1024
            ));
        }
        let request = serde_json::to_string(input).and_then(|input| {
            serde_json::to_vec(&WorkerRequest {
                source,
                input,
                limits: self.limits,
            })
        });
        let request = match request {
            Ok(request) => request,
            Err(error) => return ScriptEffects::failed(format!("failed to encode input: {error}")),
        };
        let Ok(_permit) = self.workers.acquire().await else {
            return ScriptEffects::failed("script workers are shut down");
        };
        let deadline = Instant::now() + self.limits.wall_clock;
        match self.run_worker(request, deadline).await {
            Ok(effects) => effects,
            Err(error) => ScriptEffects::failed(error),
        }
    }

    async fn run_worker(
        &self,
        request: Vec<u8>,
        deadline: Instant,
    ) -> Result<ScriptEffects, String> {
        let worker = self.worker.as_ref().map_err(Clone::clone)?;
        let mut child = Command::new(worker)
            .args(&self.worker_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                // The worker is the adapter binary itself, which an extension update can
                // replace or remove while the language server keeps running.
                format!(
                    "failed to start the script worker {}: {error}. The adapter binary may have \
                     been replaced by an extension update; restart the zed-http-lsp language \
                     server",
                    worker.display()
                )
            })?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or("failed to open the worker's stdin")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("failed to read the worker's stdout")?;
        let stderr = child
            .stderr
            .take()
            .ok_or("failed to read the worker's stderr")?;

        // Writing and stderr run on their own tasks so a worker that stops reading or floods
        // stderr cannot stall the stdout read; killing the worker ends both.
        let write = tokio::spawn(async move {
            // A worker that exits early explains itself through its status and stderr.
            let _ = stdin.write_all(&request).await;
            let _ = stdin.shutdown().await;
        });
        let diagnostics = tokio::spawn(async move {
            let mut diagnostics = Vec::new();
            let mut stderr = stderr;
            let _ = (&mut stderr)
                .take(MAX_WORKER_DIAGNOSTICS)
                .read_to_end(&mut diagnostics)
                .await;
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
            diagnostics
        });
        let exchange = async {
            let mut output = Vec::new();
            match stdout
                .take(MAX_EFFECTS_BYTES as u64 + 1)
                .read_to_end(&mut output)
                .await
            {
                Err(error) => return Err(format!("failed to read script results: {error}")),
                Ok(_) if output.len() > MAX_EFFECTS_BYTES => {
                    return Err("script results are larger than 16 MiB".to_owned())
                }
                Ok(_) => {}
            }
            let status = child
                .wait()
                .await
                .map_err(|error| format!("failed to wait for the script worker: {error}"))?;
            Ok((output, status))
        };
        let outcome = match timeout_at(deadline, exchange).await {
            Ok(outcome) => outcome,
            Err(_) => Err(format!(
                "script exceeded the {} second time limit and was stopped; its effects were \
                 discarded",
                self.limits.wall_clock.as_secs_f64()
            )),
        };
        let (output, status) = match outcome {
            Ok(result) => result,
            Err(error) => {
                // Kill and reap the worker before reporting, so no process outlives the run.
                let _ = child.kill().await;
                write.abort();
                diagnostics.abort();
                return Err(error);
            }
        };
        let _ = write.await;
        let diagnostics = diagnostics.await.unwrap_or_default();

        if !status.success() {
            let diagnostics = String::from_utf8_lossy(&diagnostics);
            let diagnostics = diagnostics.trim();
            return Err(if diagnostics.is_empty() {
                format!("script worker exited with {status}")
            } else {
                format!("script worker exited with {status}: {diagnostics}")
            });
        }
        serde_json::from_slice(&output)
            .map_err(|error| format!("script worker returned invalid results: {error}"))
    }
}

/// Entry point of a worker process: reads a request from stdin, writes effects to stdout and
/// returns the exit code.
pub fn worker_main() -> i32 {
    let mut request = Vec::new();
    if let Err(error) = io::stdin()
        .take(MAX_WORKER_INPUT_BYTES)
        .read_to_end(&mut request)
    {
        eprintln!("failed to read the script request: {error}");
        return 2;
    }
    let request: WorkerRequest = match serde_json::from_slice(&request) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("invalid script request: {error}");
            return 2;
        }
    };
    let effects = evaluate(&request.source, request.input, request.limits);
    let mut output = serde_json::to_vec(&effects).unwrap_or_default();
    if output.len() > MAX_EFFECTS_BYTES {
        output = serde_json::to_vec(&ScriptEffects::failed(
            "script results are larger than 16 MiB; its effects were discarded",
        ))
        .unwrap_or_default();
    }
    let mut stdout = io::stdout().lock();
    let written = stdout.write_all(&output).and_then(|()| stdout.flush());
    match written {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("failed to write script results: {error}");
            2
        }
    }
}

#[derive(Deserialize)]
struct RawEffects {
    globals: Vec<RawGlobalChange>,
    variables: BTreeMap<String, String>,
    logs: Vec<ScriptLog>,
    tests: Vec<ScriptTest>,
    exited: bool,
    truncated: bool,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum RawGlobalChange {
    Set { name: String, value: Value },
    Clear { name: String },
    ClearAll,
}

/// Evaluates one script in a fresh context. Runs in a worker process.
pub fn evaluate(source: &str, input: String, limits: Limits) -> ScriptEffects {
    let mut context = Context::default();
    let runtime_limits = context.runtime_limits_mut();
    runtime_limits.set_loop_iteration_limit(limits.loop_iterations);
    runtime_limits.set_recursion_limit(limits.recursion);

    if let Err(error) = install_api(&mut context, input) {
        return ScriptEffects::failed(format!(
            "failed to set up the script API: {}",
            describe(error, &mut context)
        ));
    }
    let error = context
        .eval(Source::from_bytes(source))
        .err()
        .map(|error| describe(error, &mut context));

    // Queued `client.test` callbacks run after the script body, then the effects are collected.
    let effects = match context.eval(Source::from_bytes("__zedHttpFinish()")) {
        Ok(effects) => effects,
        Err(failure) => {
            return ScriptEffects::failed(format!(
                "failed to collect script results: {}",
                describe(failure, &mut context)
            ))
        }
    };
    let effects = match effects.as_string() {
        Some(effects) => effects.to_std_string_escaped(),
        None => return ScriptEffects::failed("script results were not a string"),
    };
    if effects.len() > MAX_EFFECTS_BYTES {
        return ScriptEffects::failed("script results are larger than 16 MiB");
    }
    let raw: RawEffects = match serde_json::from_str(&effects) {
        Ok(raw) => raw,
        Err(failure) => {
            return ScriptEffects::failed(format!("script results are invalid: {failure}"))
        }
    };

    let mut logs = raw.logs;
    if raw.truncated {
        logs.push(ScriptLog {
            level: "warn".to_owned(),
            message: "further script output was dropped".to_owned(),
        });
    }
    ScriptEffects {
        globals: raw
            .globals
            .into_iter()
            .map(|change| match change {
                RawGlobalChange::Set { name, value } => GlobalChange::Set(name, value),
                RawGlobalChange::Clear { name } => GlobalChange::Clear(name),
                RawGlobalChange::ClearAll => GlobalChange::ClearAll,
            })
            .collect(),
        request_variables: raw.variables,
        logs,
        tests: raw.tests,
        // `client.exit()` throws to unwind the script, which is not an error.
        error: error.filter(|_| !raw.exited).map(truncate_error),
    }
}

/// Thrown values can be arbitrarily large; the report only needs the start.
fn truncate_error(mut error: String) -> String {
    if error.len() > MAX_ERROR_BYTES {
        let mut end = MAX_ERROR_BYTES;
        while !error.is_char_boundary(end) {
            end -= 1;
        }
        error.truncate(end);
        error.push_str("… [truncated]");
    }
    error
}

fn install_api(context: &mut Context, input: String) -> JsResult<()> {
    context.register_global_property(
        js_string!("__zedHttpInput"),
        JsString::from(input.as_str()),
        Attribute::all(),
    )?;
    let natives: [(&str, usize, NativeFunction); 3] = [
        ("__zedHttpDigest", 3, NativeFunction::from_fn_ptr(digest)),
        (
            "__zedHttpJsonPath",
            2,
            NativeFunction::from_fn_ptr(json_path),
        ),
        ("__zedHttpDynamic", 1, NativeFunction::from_fn_ptr(dynamic)),
    ];
    for (name, length, function) in natives {
        context.register_global_builtin_callable(JsString::from(name), length, function)?;
    }
    context.eval(Source::from_bytes(PRELUDE))?;
    Ok(())
}

fn describe(error: JsError, context: &mut Context) -> String {
    match error.try_native(context) {
        Ok(native) => native.to_string(),
        Err(_) => error.to_string(),
    }
}

fn string_argument(arguments: &[JsValue], index: usize, context: &mut Context) -> JsResult<String> {
    Ok(arguments
        .get_or_undefined(index)
        .to_string(context)?
        .to_std_string_escaped())
}

/// `__zedHttpDigest(algorithm, text, encoding)` → hex or base64 digest.
fn digest(_: &JsValue, arguments: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let algorithm = string_argument(arguments, 0, context)?;
    let text = string_argument(arguments, 1, context)?;
    let encoding = string_argument(arguments, 2, context)?;
    let bytes = match algorithm.as_str() {
        "md5" => Md5::digest(text.as_bytes()).to_vec(),
        "sha1" => Sha1::digest(text.as_bytes()).to_vec(),
        "sha256" => Sha256::digest(text.as_bytes()).to_vec(),
        "sha512" => Sha512::digest(text.as_bytes()).to_vec(),
        other => {
            return Err(JsNativeError::typ()
                .with_message(format!("unsupported digest {other}"))
                .into())
        }
    };
    let encoded = if encoding == "base64" {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    } else {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    };
    Ok(JsString::from(encoded.as_str()).into())
}

/// `__zedHttpJsonPath(jsonText, expression)` → JSON array of every match.
fn json_path(_: &JsValue, arguments: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let document = string_argument(arguments, 0, context)?;
    let expression = string_argument(arguments, 1, context)?;
    let document: Value = serde_json::from_str(&document).map_err(|error| {
        JsNativeError::typ().with_message(format!("jsonPath input is not JSON: {error}"))
    })?;
    let path = JsonPath::parse(&expression).map_err(|error| {
        JsNativeError::syntax().with_message(format!("invalid JSONPath {expression:?}: {error}"))
    })?;
    let matches: Vec<&Value> = path.query(&document).all();
    let matches = serde_json::to_string(&matches).unwrap_or_else(|_| "[]".to_owned());
    Ok(JsString::from(matches.as_str()).into())
}

/// `__zedHttpDynamic(name)` → the value of a `$random.*` dynamic variable, or `undefined`.
/// Environment lookups (`$env`, `$processEnv`) are deliberately not reachable from scripts.
fn dynamic(_: &JsValue, arguments: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = string_argument(arguments, 0, context)?;
    if !name.starts_with("$random.") {
        return Ok(JsValue::undefined());
    }
    Ok(dynamic_variable(&name)
        .map(|value| JsString::from(value.as_str()).into())
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn run(source: &str, input: ScriptInput) -> ScriptEffects {
        evaluate(
            source,
            serde_json::to_string(&input).unwrap(),
            Limits::default(),
        )
    }

    fn handler_input(body: Value) -> ScriptInput {
        ScriptInput {
            request: ScriptRequest {
                method: "GET".to_owned(),
                url: "https://example.test/users".to_owned(),
                headers: vec![("Accept".to_owned(), "application/json".to_owned())],
            },
            response: Some(ScriptResponse {
                status: Some(200),
                headers: vec![
                    (
                        "content-type".to_owned(),
                        "application/json; charset=utf-8".to_owned(),
                    ),
                    ("set-cookie".to_owned(), "a=1".to_owned()),
                    ("set-cookie".to_owned(), "b=2".to_owned()),
                ],
                body,
                content_type: Some("application/json; charset=utf-8".to_owned()),
            }),
            ..ScriptInput::default()
        }
    }

    #[test]
    fn records_ordered_global_changes_and_request_variables() {
        let input = ScriptInput {
            globals: BTreeMap::from([("old".to_owned(), json!("1"))]),
            environment: BTreeMap::from([("host".to_owned(), "example.test".to_owned())]),
            ..ScriptInput::default()
        };
        let effects = run(
            r#"
            client.global.set("a", { nested: [1] });
            client.global.clearAll();
            client.global.set("token", "abc");
            client.global.set("gone", 1);
            client.global.clear("gone");
            client.log("empty:", client.global.isEmpty(), "token:", client.global.get("token"));
            request.variables.set("id", 7);
            client.log(request.variables.get("id"), request.environment.get("host"));
            "#,
            input,
        );
        assert_eq!(effects.error, None);
        assert_eq!(
            effects.globals,
            vec![
                GlobalChange::Set("a".to_owned(), json!({ "nested": [1] })),
                GlobalChange::ClearAll,
                GlobalChange::Set("token".to_owned(), json!("abc")),
                GlobalChange::Set("gone".to_owned(), json!(1)),
                GlobalChange::Clear("gone".to_owned()),
            ]
        );
        assert_eq!(
            effects.request_variables,
            BTreeMap::from([("id".to_owned(), "7".to_owned())])
        );
        let messages: Vec<_> = effects
            .logs
            .iter()
            .map(|log| log.message.as_str())
            .collect();
        assert_eq!(messages, vec!["empty: false token: abc", "7 example.test"]);
    }

    #[test]
    fn exposes_the_response_and_runs_queued_tests_after_the_body() {
        let effects = run(
            r#"
            client.test("runs after the body", () => client.assert(globalThis.seen === true));
            client.test("status", () => client.assert(response.status === 200, "status"));
            client.test("fails", () => client.assert(response.body.items.length === 3, "three items"));
            client.log(response.headers.valueOf("Content-Type"), response.headers.valuesOf("set-cookie").join(","));
            client.log(response.contentType.mimeType, response.contentType.charset);
            client.log(request.method, request.url, request.headers.findByName("accept"));
            client.log(jsonPath(response.body, "$.items[*].id"), jsonPath(response.body, "$.name"));
            globalThis.seen = true;
            "#,
            handler_input(json!({ "name": "users", "items": [{ "id": 1 }, { "id": 2 }] })),
        );
        assert_eq!(effects.error, None);
        let tests: Vec<_> = effects
            .tests
            .iter()
            .map(|test| (test.name.as_str(), test.passed, test.message.as_deref()))
            .collect();
        assert_eq!(
            tests,
            vec![
                ("runs after the body", true, None),
                ("status", true, None),
                ("fails", false, Some("three items")),
            ]
        );
        let messages: Vec<_> = effects
            .logs
            .iter()
            .map(|log| log.message.as_str())
            .collect();
        assert_eq!(
            messages,
            vec![
                "application/json; charset=utf-8 a=1,b=2",
                "application/json utf-8",
                "GET https://example.test/users application/json",
                "[1,2] users",
            ]
        );
    }

    #[test]
    fn exit_ends_only_the_script_and_keeps_effects() {
        let effects = run(
            r#"
            client.global.set("before", 1);
            client.test("queued", () => {});
            client.exit();
            client.global.set("after", 1);
            "#,
            ScriptInput::default(),
        );
        assert_eq!(effects.error, None);
        assert_eq!(
            effects.globals,
            vec![GlobalChange::Set("before".to_owned(), json!(1))]
        );
        assert_eq!(effects.tests.len(), 1);
    }

    #[test]
    fn keeps_effects_before_an_exception() {
        let effects = run(
            r#"
            client.global.set("kept", true);
            console.warn("careful");
            client.assert(false, "boom");
            "#,
            ScriptInput::default(),
        );
        assert!(effects.error.as_deref().unwrap().contains("boom"));
        assert_eq!(
            effects.globals,
            vec![GlobalChange::Set("kept".to_owned(), json!(true))]
        );
        assert_eq!(effects.logs[0].level, "warn");

        let syntax = run("this is not javascript", ScriptInput::default());
        assert!(syntax.error.unwrap().contains("SyntaxError"));
    }

    #[test]
    fn scripts_cannot_read_the_environment() {
        let effects = run(
            r#"client.log(String(__zedHttpDynamic("$env.PATH")), String(__zedHttpDynamic("$processEnv PATH")));"#,
            ScriptInput::default(),
        );
        assert_eq!(effects.logs[0].message, "undefined undefined");
    }

    #[test]
    fn provides_crypto_and_random_helpers() {
        let effects = run(
            r#"
            client.log(crypto.sha256().updateWithText("ab").updateWithText("c").digest().toHex());
            client.log(crypto.md5().updateWithText("abc").digest().toBase64());
            client.log(crypto.sha1().updateWithText("abc").digest().toHex());
            client.log($random.uuid.length, typeof $random.integer(1, 5), $random.alphabetic(4).length);
            "#,
            ScriptInput::default(),
        );
        assert_eq!(effects.error, None);
        let messages: Vec<_> = effects
            .logs
            .iter()
            .map(|log| log.message.as_str())
            .collect();
        assert_eq!(
            messages,
            vec![
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                "kAFQmDzST7DWlj99KOF/cg==",
                "a9993e364706816aba3e25717850c26c9cd0d89d",
                "36 number 4",
            ]
        );
    }

    #[test]
    fn stops_runaway_scripts() {
        let looping = run("while (true) {}", ScriptInput::default());
        assert!(looping.error.unwrap().contains("loop"), "loop limit");
        let recursing = run("function f() { return f(); } f();", ScriptInput::default());
        assert!(recursing.error.is_some());
    }
}
