//! Environment files, variable layers, dynamic variables and `{{ }}` substitution.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};
use serde_json_path::JsonPath;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{report::value_text, session::NamedResponse};

pub const PUBLIC_ENV_FILE: &str = "http-client.env.json";
pub const PRIVATE_ENV_FILE: &str = "http-client.private.env.json";
const SHARED: &str = "$shared";
const MAX_DEPTH: usize = 16;
const MAX_ENV_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// The selected environment, with `$shared` and private values already merged in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Environment {
    pub name: Option<String>,
    pub variables: BTreeMap<String, String>,
}

/// Loads the environment for `http_file`. Each env file is taken from the nearest directory
/// that has one, searching from the file's directory up to the enclosing workspace root. Files
/// outside every workspace root only use env files next to them.
pub fn load_environment(
    http_file: &Path,
    workspace_roots: &[PathBuf],
    configured: Option<&str>,
) -> Result<Environment, String> {
    let directories = search_directories(http_file, workspace_roots);
    let public = find_env_file(&directories, PUBLIC_ENV_FILE)?;
    let private = find_env_file(&directories, PRIVATE_ENV_FILE)?;

    let names = public.iter().chain(&private).flat_map(|file| file.keys());
    let name = match configured {
        Some(name) => {
            let defined = names.clone().any(|candidate| candidate == name);
            if !defined && (public.is_some() || private.is_some()) {
                return Err(format!(
                    "environment {name:?} is not defined in {PUBLIC_ENV_FILE} or {PRIVATE_ENV_FILE}"
                ));
            }
            Some(name.to_owned())
        }
        None => select_environment(names.cloned()),
    };

    // Lowest precedence first: public $shared, private $shared, public env, private env.
    let mut variables = BTreeMap::new();
    let mut merge = |file: &Option<Map<String, Value>>, key: &str| {
        if let Some(Value::Object(values)) = file.as_ref().and_then(|file| file.get(key)) {
            for (name, value) in values {
                variables.insert(name.clone(), value_text(value));
            }
        }
    };
    merge(&public, SHARED);
    merge(&private, SHARED);
    if let Some(name) = &name {
        merge(&public, name);
        merge(&private, name);
    }
    Ok(Environment { name, variables })
}

fn search_directories(http_file: &Path, workspace_roots: &[PathBuf]) -> Vec<PathBuf> {
    let Some(directory) = http_file.parent() else {
        return Vec::new();
    };
    let root = workspace_roots
        .iter()
        .filter(|root| directory.starts_with(root))
        .max_by_key(|root| root.components().count());
    let Some(root) = root else {
        return vec![directory.to_path_buf()];
    };
    directory
        .ancestors()
        .take_while(|ancestor| ancestor.starts_with(root))
        .map(Path::to_path_buf)
        .collect()
}

fn find_env_file(
    directories: &[PathBuf],
    name: &str,
) -> Result<Option<Map<String, Value>>, String> {
    let Some(path) = directories
        .iter()
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
    else {
        return Ok(None);
    };
    let size = fs::metadata(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?
        .len();
    if size > MAX_ENV_FILE_BYTES {
        return Err(format!("{} is larger than 4 MiB", path.display()));
    }
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    match serde_json::from_str(&text) {
        Ok(Value::Object(file)) => Ok(Some(file)),
        Ok(_) => Err(format!("{} must contain a JSON object", path.display())),
        Err(error) => Err(format!("{} is not valid JSON: {error}", path.display())),
    }
}

/// Prefers `default`, otherwise the alphabetically first environment. Names starting with `$`
/// (such as `$shared`) hold variables shared by all environments and are never selectable.
pub fn select_environment(names: impl IntoIterator<Item = String>) -> Option<String> {
    let names: BTreeSet<String> = names
        .into_iter()
        .filter(|name| !name.starts_with('$'))
        .collect();
    if names.contains("default") {
        return Some("default".to_owned());
    }
    names.into_iter().next()
}

/// Variable layers for one request, highest precedence first.
#[derive(Clone, Debug, Default)]
pub struct Variables {
    /// Set by `request.variables.set` in pre-request scripts.
    pub request: BTreeMap<String, String>,
    /// `client.global` values.
    pub globals: BTreeMap<String, Value>,
    /// File `@name = value` definitions.
    pub file: BTreeMap<String, String>,
    pub environment: BTreeMap<String, String>,
    /// Last responses of named requests, for `{{name.response.…}}` references.
    pub responses: HashMap<String, Arc<NamedResponse>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Substitution {
    pub text: String,
    /// Names left verbatim because they did not resolve, in first-seen order.
    pub unresolved: Vec<String>,
}

impl Variables {
    pub fn lookup(&self, name: &str) -> Option<String> {
        self.request
            .get(name)
            .cloned()
            .or_else(|| self.globals.get(name).map(value_text))
            .or_else(|| self.file.get(name).cloned())
            .or_else(|| self.environment.get(name).cloned())
            .or_else(|| self.response_reference(name))
    }

    /// `name.response.body.<JSONPath>` (`$` or `*` for the whole body) and
    /// `name.response.headers.<Header>` (case-insensitive).
    fn response_reference(&self, reference: &str) -> Option<String> {
        let (name, rest) = reference.split_once(".response.")?;
        let response = self.responses.get(name)?;
        if let Some(header) = rest.strip_prefix("headers.") {
            return response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(header))
                .map(|(_, value)| value.clone());
        }
        let path = rest.strip_prefix("body")?;
        let path = match path.strip_prefix('.') {
            Some(path) => path,
            None if path.is_empty() => "$",
            None => return None,
        };
        // Bodies that were not served as JSON may still be JSON.
        let parsed;
        let body = match &response.body {
            Value::String(text) if path != "$" && path != "*" => {
                parsed = serde_json::from_str::<Value>(text).ok()?;
                &parsed
            }
            body => body,
        };
        if path == "$" || path == "*" {
            return Some(value_text(body));
        }
        let path = JsonPath::parse(path).ok()?;
        match path.query(body).all().as_slice() {
            [] => None,
            [value] => Some(value_text(value)),
            values => serde_json::to_string(values).ok(),
        }
    }

    /// Replaces every `{{name}}`. Resolved values are substituted recursively; dynamic variables
    /// are evaluated per occurrence.
    pub fn substitute(&self, text: &str) -> Substitution {
        let mut unresolved = Vec::new();
        let text = self.expand(text, &mut Vec::new(), &mut unresolved);
        Substitution { text, unresolved }
    }

    fn expand(&self, text: &str, stack: &mut Vec<String>, unresolved: &mut Vec<String>) -> String {
        let mut output = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("{{") {
            let Some(length) = rest[start + 2..].find("}}") else {
                break;
            };
            output.push_str(&rest[..start]);
            let raw = &rest[start..start + 2 + length + 2];
            let name = rest[start + 2..start + 2 + length].trim();
            rest = &rest[start + 2 + length + 2..];

            let value = if name.starts_with('$') {
                dynamic_variable(name)
            } else if stack.iter().any(|seen| seen == name) || stack.len() >= MAX_DEPTH {
                None
            } else {
                self.lookup(name).map(|value| {
                    stack.push(name.to_owned());
                    let value = self.expand(&value, stack, unresolved);
                    stack.pop();
                    value
                })
            };
            match value {
                Some(value) => output.push_str(&value),
                None => {
                    if !unresolved.iter().any(|seen| seen == name) {
                        unresolved.push(name.to_owned());
                    }
                    output.push_str(raw);
                }
            }
        }
        output.push_str(rest);
        output
    }
}

/// Evaluates `$uuid`, `$timestamp`, `$random.*`, `$env.NAME` and friends.
pub fn dynamic_variable(name: &str) -> Option<String> {
    if let Some(variable) = name.strip_prefix("$env.") {
        return env::var(variable).ok();
    }
    if let Some(variable) = name.strip_prefix("$processEnv") {
        return env::var(variable.trim()).ok();
    }
    let (function, arguments) = match name.split_once('(') {
        Some((function, arguments)) => (function.trim(), arguments.strip_suffix(')')?),
        None => (name, ""),
    };
    let arguments: Vec<&str> = arguments
        .split(',')
        .map(str::trim)
        .filter(|argument| !argument.is_empty())
        .collect();
    let count = || -> Option<usize> {
        match arguments.as_slice() {
            [] => Some(10),
            [count] => count.parse().ok().filter(|count| *count <= 10_000),
            _ => None,
        }
    };

    match function {
        "$uuid" | "$random.uuid" => Some(uuid::Uuid::new_v4().to_string()),
        "$timestamp" => Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .to_string(),
        ),
        "$isoTimestamp" => {
            let now = OffsetDateTime::now_utc();
            let now = now.replace_millisecond(now.millisecond()).unwrap_or(now);
            now.format(&Rfc3339).ok()
        }
        "$randomInt" | "$random.integer" => {
            let (low, high) = match arguments.as_slice() {
                [] => (0, 1000),
                [low, high] => (low.parse::<i64>().ok()?, high.parse::<i64>().ok()?),
                _ => return None,
            };
            (low < high).then(|| fastrand::i64(low..high).to_string())
        }
        "$random.float" => {
            let (low, high) = match arguments.as_slice() {
                [] => (0.0, 1000.0),
                [low, high] => (low.parse::<f64>().ok()?, high.parse::<f64>().ok()?),
                _ => return None,
            };
            (low < high).then(|| (low + fastrand::f64() * (high - low)).to_string())
        }
        "$random.alphabetic" => Some(random_string(count()?, |_| fastrand::alphabetic())),
        "$random.alphanumeric" => Some(random_string(count()?, |_| fastrand::alphanumeric())),
        "$random.hexadecimal" => Some(random_string(count()?, |_| {
            char::from_digit(fastrand::u32(0..16), 16).unwrap_or('0')
        })),
        "$random.email" => Some(format!(
            "{}@{}.com",
            random_string(10, |_| fastrand::lowercase()),
            random_string(8, |_| fastrand::lowercase())
        )),
        _ => None,
    }
}

fn random_string(length: usize, next: impl FnMut(usize) -> char) -> String {
    (0..length).map(next).collect()
}

#[cfg(test)]
mod tests {
    use std::process;

    use serde_json::json;

    use super::*;

    fn temporary_directory(name: &str) -> PathBuf {
        let directory = env::temp_dir().join(format!(
            "zed-http-variables-{name}-{}-{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn selects_default_or_first_named_environment() {
        let names = |names: &[&str]| {
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            select_environment(names(&["$shared", "dev", "default"])).as_deref(),
            Some("default")
        );
        assert_eq!(
            select_environment(names(&["$shared", "prod", "dev"])).as_deref(),
            Some("dev")
        );
        assert_eq!(select_environment(names(&["$shared"])), None);
    }

    #[test]
    fn layers_resolve_in_documented_precedence() {
        let variables = Variables {
            request: BTreeMap::from([("a".to_owned(), "request".to_owned())]),
            globals: BTreeMap::from([
                ("a".to_owned(), json!("global")),
                ("b".to_owned(), json!(42)),
            ]),
            file: BTreeMap::from([
                ("a".to_owned(), "file".to_owned()),
                ("b".to_owned(), "file".to_owned()),
                ("c".to_owned(), "file".to_owned()),
            ]),
            environment: BTreeMap::from([
                ("a".to_owned(), "env".to_owned()),
                ("c".to_owned(), "env".to_owned()),
                ("d".to_owned(), "env".to_owned()),
            ]),
            ..Variables::default()
        };
        assert_eq!(
            variables.substitute("{{a}} {{ b }} {{c}} {{d}}").text,
            "request 42 file env"
        );
    }

    #[test]
    fn environment_files_merge_in_documented_precedence() {
        let root = temporary_directory("precedence");
        let nested = root.join("api").join("v1");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            root.join(PUBLIC_ENV_FILE),
            json!({
                "$shared": { "a": "public-shared", "b": "public-shared", "c": "public-shared", "d": "public-shared" },
                "dev": { "a": "public-dev", "b": "public-dev", "port": 8080 },
                "default": { "a": "public-default" }
            })
            .to_string(),
        )
        .unwrap();
        // The nearest private file wins over one further up.
        fs::write(
            root.join(PRIVATE_ENV_FILE),
            json!({ "dev": { "a": "ignored" } }).to_string(),
        )
        .unwrap();
        fs::write(
            nested.join(PRIVATE_ENV_FILE),
            json!({
                "$shared": { "a": "private-shared", "b": "private-shared", "c": "private-shared" },
                "dev": { "a": "private-dev" }
            })
            .to_string(),
        )
        .unwrap();
        let http_file = nested.join("requests.http");

        let environment =
            load_environment(&http_file, std::slice::from_ref(&root), Some("dev")).unwrap();
        assert_eq!(environment.name.as_deref(), Some("dev"));
        let get = |name: &str| environment.variables.get(name).map(String::as_str);
        assert_eq!(get("a"), Some("private-dev"));
        assert_eq!(get("b"), Some("public-dev"));
        assert_eq!(get("c"), Some("private-shared"));
        assert_eq!(get("d"), Some("public-shared"));
        assert_eq!(get("port"), Some("8080"));

        let selected = load_environment(&http_file, std::slice::from_ref(&root), None).unwrap();
        assert_eq!(selected.name.as_deref(), Some("default"));
        assert!(
            load_environment(&http_file, std::slice::from_ref(&root), Some("missing")).is_err()
        );

        // Without a workspace root, only env files next to the request file are used.
        let outside = load_environment(&http_file, &[], Some("dev")).unwrap();
        assert_eq!(
            outside.variables.get("b").map(String::as_str),
            Some("private-shared")
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn substitutes_recursively_and_reports_unresolved_names() {
        let variables = Variables {
            file: BTreeMap::from([
                ("host".to_owned(), "example.test".to_owned()),
                ("base".to_owned(), "https://{{host}}".to_owned()),
                ("loop".to_owned(), "{{loop}}".to_owned()),
            ]),
            ..Variables::default()
        };
        let result = variables.substitute("{{base}}/{{missing}}/{{missing}}/{{loop}} {{ unclosed");
        assert_eq!(
            result.text,
            "https://example.test/{{missing}}/{{missing}}/{{loop}} {{ unclosed"
        );
        assert_eq!(result.unresolved, vec!["missing", "loop"]);
    }

    #[test]
    fn resolves_named_response_references() {
        let response = |body: Value| {
            Arc::new(NamedResponse {
                status: Some(200),
                headers: vec![("x-token".to_owned(), "header-token".to_owned())],
                body,
            })
        };
        let variables = Variables {
            responses: HashMap::from([
                (
                    "login".to_owned(),
                    response(json!({ "token": "abc", "items": [{ "id": 1 }, { "id": 2 }] })),
                ),
                (
                    "text".to_owned(),
                    response(json!("{\"served\": \"as text\"}")),
                ),
            ]),
            file: BTreeMap::from([(
                "auth".to_owned(),
                "Bearer {{login.response.body.$.token}}".to_owned(),
            )]),
            ..Variables::default()
        };
        let result = variables.substitute(
            "{{auth}} {{login.response.headers.X-Token}} {{login.response.body.$.items[*].id}} \
             {{text.response.body.$.served}} {{login.response.body.$.missing}} {{other.response.body.$}}",
        );
        assert_eq!(
            result.text,
            "Bearer abc header-token [1,2] as text {{login.response.body.$.missing}} \
             {{other.response.body.$}}"
        );
        assert_eq!(
            variables.lookup("text.response.body").as_deref(),
            Some("{\"served\": \"as text\"}")
        );
        assert_eq!(
            variables.lookup("login.response.body.*").as_deref(),
            Some(r#"{"token":"abc","items":[{"id":1},{"id":2}]}"#)
        );
    }

    #[test]
    fn evaluates_dynamic_variables_per_occurrence() {
        let variables = Variables::default();
        let result = variables.substitute("{{$uuid}} {{$uuid}}");
        let ids: Vec<_> = result.text.split(' ').collect();
        assert_eq!(ids[0].len(), 36);
        assert_ne!(ids[0], ids[1]);

        let number: i64 = dynamic_variable("$random.integer(5, 7)")
            .unwrap()
            .parse()
            .unwrap();
        assert!((5..7).contains(&number));
        let float: f64 = dynamic_variable("$random.float(1, 2)")
            .unwrap()
            .parse()
            .unwrap();
        assert!((1.0..2.0).contains(&float));
        assert!(dynamic_variable("$randomInt")
            .unwrap()
            .parse::<i64>()
            .is_ok());
        assert_eq!(dynamic_variable("$random.alphabetic(4)").unwrap().len(), 4);
        assert!(dynamic_variable("$random.alphanumeric")
            .unwrap()
            .chars()
            .all(|c| c.is_ascii_alphanumeric()));
        assert!(dynamic_variable("$random.hexadecimal(8)")
            .unwrap()
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
        assert!(dynamic_variable("$random.email").unwrap().contains('@'));
        assert!(dynamic_variable("$timestamp")
            .unwrap()
            .parse::<u64>()
            .is_ok());
        assert!(dynamic_variable("$isoTimestamp").unwrap().ends_with('Z'));
        let path = env::var("PATH").ok();
        assert_eq!(dynamic_variable("$env.PATH"), path);
        assert_eq!(dynamic_variable("$processEnv PATH"), path);
        assert_eq!(dynamic_variable("$unknown"), None);
    }
}
