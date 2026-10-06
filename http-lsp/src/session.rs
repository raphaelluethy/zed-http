//! State shared by every run of one language server: `client.global` values, cookies and the
//! last response of each named request. Everything is kept in memory only.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard},
};

use reqwest::cookie::Jar;
use serde_json::Value;

/// One `client.global` mutation, applied in script order.
#[derive(Clone, Debug, PartialEq)]
pub enum GlobalChange {
    Set(String, Value),
    Clear(String),
    ClearAll,
}

/// The last response of a named request, for `{{name.response.…}}` references.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NamedResponse {
    pub status: Option<u16>,
    /// Lower-cased header names, in received order.
    pub headers: Vec<(String, String)>,
    /// Parsed JSON when the response was JSON, otherwise the body text.
    pub body: Value,
}

#[derive(Debug, Default)]
pub struct Session {
    globals: Mutex<BTreeMap<String, Value>>,
    responses: Mutex<HashMap<String, Arc<NamedResponse>>>,
    cookies: Arc<Jar>,
}

impl Session {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn globals(&self) -> BTreeMap<String, Value> {
        lock(&self.globals).clone()
    }

    pub fn apply_globals(&self, changes: &[GlobalChange]) {
        let mut globals = lock(&self.globals);
        for change in changes {
            match change {
                GlobalChange::Set(name, value) => {
                    globals.insert(name.clone(), value.clone());
                }
                GlobalChange::Clear(name) => {
                    globals.remove(name);
                }
                GlobalChange::ClearAll => globals.clear(),
            }
        }
    }

    pub fn cookies(&self) -> Arc<Jar> {
        Arc::clone(&self.cookies)
    }

    pub fn store_response(&self, name: &str, response: NamedResponse) {
        lock(&self.responses).insert(name.to_owned(), Arc::new(response));
    }

    pub fn response(&self, name: &str) -> Option<Arc<NamedResponse>> {
        lock(&self.responses).get(name).cloned()
    }
}

/// The guarded maps stay consistent even if a holder panicked, so poisoning is ignored.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn applies_global_changes_in_order() {
        let session = Session::new();
        session.apply_globals(&[
            GlobalChange::Set("a".to_owned(), json!("1")),
            GlobalChange::Set("b".to_owned(), json!(2)),
            GlobalChange::ClearAll,
            GlobalChange::Set("c".to_owned(), json!(true)),
            GlobalChange::Set("d".to_owned(), json!("x")),
            GlobalChange::Clear("d".to_owned()),
        ]);
        assert_eq!(
            session.globals(),
            BTreeMap::from([("c".to_owned(), json!(true))])
        );
    }

    #[test]
    fn stores_named_responses() {
        let session = Session::new();
        assert!(session.response("login").is_none());
        session.store_response(
            "login",
            NamedResponse {
                status: Some(200),
                headers: vec![("x-token".to_owned(), "abc".to_owned())],
                body: json!({ "token": "abc" }),
            },
        );
        assert_eq!(session.response("login").unwrap().body["token"], "abc");
    }
}
