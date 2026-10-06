//! Pre-request scripts and response handlers on the embedded Boa JavaScript engine, with the
//! IntelliJ `client` / `request` / `response` API.
//!
//! Boa contexts are `!Send`, so every script gets a fresh context on a blocking thread and only
//! JSON crosses the boundary. Boa's runtime limits (loop iterations per call frame, recursion)
//! stop runaway scripts. Boa has no interrupt hook, so the wall-clock guard can only abandon a
//! script's result; its thread keeps its worker permit until the runtime limits end it.
//! Scripts get no filesystem or network access.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

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
use tokio::{sync::Semaphore, time::timeout};

use crate::{session::GlobalChange, variables::dynamic_variable};

const PRELUDE: &str = include_str!("script_prelude.js");
pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_EFFECTS_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONCURRENT_SCRIPTS: usize = 2;

#[derive(Clone, Copy, Debug)]
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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScriptEffects {
    /// `client.global` mutations in call order.
    pub globals: Vec<GlobalChange>,
    pub request_variables: BTreeMap<String, String>,
    pub logs: Vec<ScriptLog>,
    pub tests: Vec<ScriptTest>,
    /// `client.exit()` ended the script early.
    pub exited: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ScriptLog {
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
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
    workers: Arc<Semaphore>,
}

impl Default for ScriptEngine {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl ScriptEngine {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
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
        let input = match serde_json::to_string(input) {
            Ok(input) => input,
            Err(error) => return ScriptEffects::failed(format!("failed to encode input: {error}")),
        };
        let limits = self.limits;
        let workers = Arc::clone(&self.workers);
        let task = async move {
            let permit = workers
                .acquire_owned()
                .await
                .map_err(|_| "script workers are shut down".to_owned())?;
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                evaluate(&source, input, limits)
            })
            .await
            .map_err(|error| format!("script worker failed: {error}"))
        };
        match timeout(limits.wall_clock, task).await {
            Ok(Ok(effects)) => effects,
            Ok(Err(error)) => ScriptEffects::failed(error),
            Err(_) => ScriptEffects::failed(format!(
                "script exceeded the {} second time limit; its effects were discarded",
                limits.wall_clock.as_secs_f64()
            )),
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

/// Runs on a blocking thread; every Boa object is created and dropped here.
fn evaluate(source: &str, input: String, limits: Limits) -> ScriptEffects {
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
        error: error.filter(|_| !raw.exited),
        exited: raw.exited,
    }
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

/// `__zedHttpDynamic(name)` → the value of a `{{$…}}` dynamic variable, or `undefined`.
fn dynamic(_: &JsValue, arguments: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = string_argument(arguments, 0, context)?;
    Ok(dynamic_variable(&name)
        .map(|value| JsString::from(value.as_str()).into())
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    async fn run(source: &str, input: ScriptInput) -> ScriptEffects {
        ScriptEngine::default().run(source.to_owned(), &input).await
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

    #[tokio::test]
    async fn records_ordered_global_changes_and_request_variables() {
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
        )
        .await;
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

    #[tokio::test]
    async fn exposes_the_response_and_runs_queued_tests_after_the_body() {
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
        )
        .await;
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

    #[tokio::test]
    async fn exit_ends_only_the_script_and_keeps_effects() {
        let effects = run(
            r#"
            client.global.set("before", 1);
            client.test("queued", () => {});
            client.exit();
            client.global.set("after", 1);
            "#,
            ScriptInput::default(),
        )
        .await;
        assert_eq!(effects.error, None);
        assert!(effects.exited);
        assert_eq!(
            effects.globals,
            vec![GlobalChange::Set("before".to_owned(), json!(1))]
        );
        assert_eq!(effects.tests.len(), 1);
    }

    #[tokio::test]
    async fn keeps_effects_before_an_exception() {
        let effects = run(
            r#"
            client.global.set("kept", true);
            console.warn("careful");
            client.assert(false, "boom");
            "#,
            ScriptInput::default(),
        )
        .await;
        assert!(effects.error.as_deref().unwrap().contains("boom"));
        assert_eq!(
            effects.globals,
            vec![GlobalChange::Set("kept".to_owned(), json!(true))]
        );
        assert_eq!(effects.logs[0].level, "warn");

        let syntax = run("this is not javascript", ScriptInput::default()).await;
        assert!(syntax.error.unwrap().contains("SyntaxError"));
    }

    #[tokio::test]
    async fn provides_crypto_and_random_helpers() {
        let effects = run(
            r#"
            client.log(crypto.sha256().updateWithText("ab").updateWithText("c").digest().toHex());
            client.log(crypto.md5().updateWithText("abc").digest().toBase64());
            client.log(crypto.sha1().updateWithText("abc").digest().toHex());
            client.log($random.uuid.length, typeof $random.integer(1, 5), $random.alphabetic(4).length);
            "#,
            ScriptInput::default(),
        )
        .await;
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

    #[tokio::test]
    async fn stops_runaway_scripts() {
        let looping = run("while (true) {}", ScriptInput::default()).await;
        assert!(looping.error.unwrap().contains("loop"), "loop limit");
        let recursing = run("function f() { return f(); } f();", ScriptInput::default()).await;
        assert!(recursing.error.is_some());
    }

    #[test]
    fn abandons_scripts_past_the_wall_clock_limit() {
        // The abandoned thread spins until the process exits, so the runtime must not wait for it.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            .build()
            .unwrap();
        let engine = ScriptEngine::new(Limits {
            wall_clock: Duration::from_millis(100),
            loop_iterations: u64::MAX,
            recursion: 256,
        });
        let slow = runtime.block_on(engine.run(
            "let x = 0; for (let i = 0; i < 1e12; i++) { x += i; }".to_owned(),
            &ScriptInput::default(),
        ));
        runtime.shutdown_background();
        assert!(slow.error.unwrap().contains("time limit"));
    }
}
