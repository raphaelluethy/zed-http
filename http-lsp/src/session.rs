//! State shared by every run of one language server: `client.global` values, cookies and the
//! last response of each named request. Everything is kept in memory only.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
};

use reqwest::cookie::Jar;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::report::value_bytes;

const MAX_GLOBAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_GLOBALS: usize = 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESPONSES: usize = 256;

/// One `client.global` mutation, applied in script order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
struct ResponseStore {
    entries: HashMap<String, Arc<NamedResponse>>,
    order: VecDeque<String>,
    bytes: usize,
}

impl ResponseStore {
    fn remove(&mut self, name: &str) {
        if let Some(entry) = self.entries.remove(name) {
            self.bytes = self.bytes.saturating_sub(response_bytes(name, &entry));
            self.order.retain(|seen| seen != name);
        }
    }

    fn store(&mut self, name: &str, response: NamedResponse, limits: Limits) -> Result<(), String> {
        self.remove(name);
        let response = Arc::new(response);
        let bytes = response_bytes(name, &response);
        if bytes > limits.bytes {
            return Err(format!(
                "response of {name:?} is larger than {}",
                size_limit(limits.bytes)
            ));
        }
        while self.entries.len() >= limits.entries
            || self.bytes.saturating_add(bytes) > limits.bytes
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.remove(&oldest);
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.order.push_back(name.to_owned());
        self.entries.insert(name.to_owned(), response);
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    bytes: usize,
}

fn response_bytes(name: &str, response: &NamedResponse) -> usize {
    response.headers.iter().fold(
        name.len().saturating_add(value_bytes(&response.body)),
        |total, (name, value)| {
            total
                .saturating_add(name.capacity())
                .saturating_add(value.capacity())
        },
    )
}

fn size_limit(bytes: usize) -> String {
    if bytes.is_multiple_of(1024 * 1024) {
        format!("{} MiB", bytes / 1024 / 1024)
    } else {
        format!("{bytes} bytes")
    }
}

#[derive(Debug, Default)]
pub struct Session {
    globals: Mutex<BTreeMap<String, Value>>,
    responses: Mutex<ResponseStore>,
    cookies: Arc<Jar>,
}

impl Session {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn globals(&self) -> BTreeMap<String, Value> {
        lock(&self.globals).clone()
    }

    pub fn apply_globals(&self, changes: &[GlobalChange]) -> Result<(), String> {
        self.apply_globals_within(
            changes,
            Limits {
                entries: MAX_GLOBALS,
                bytes: MAX_GLOBAL_BYTES,
            },
        )
    }

    fn apply_globals_within(&self, changes: &[GlobalChange], limits: Limits) -> Result<(), String> {
        let mut globals = lock(&self.globals);
        let mut merged: BTreeMap<&str, &Value> = globals
            .iter()
            .map(|(name, value)| (name.as_str(), value))
            .collect();
        for change in changes {
            match change {
                GlobalChange::Set(name, value) => {
                    merged.insert(name.as_str(), value);
                }
                GlobalChange::Clear(name) => {
                    merged.remove(name.as_str());
                }
                GlobalChange::ClearAll => merged.clear(),
            }
        }
        if merged.len() > limits.entries {
            return Err(format!(
                "client.global values exceed {} entries",
                limits.entries
            ));
        }
        let bytes = merged.iter().fold(0usize, |total, (name, value)| {
            total
                .saturating_add(name.len())
                .saturating_add(value_bytes(value))
        });
        if bytes > limits.bytes {
            return Err(format!(
                "client.global values exceed {}",
                size_limit(limits.bytes)
            ));
        }
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
        Ok(())
    }

    pub fn cookies(&self) -> Arc<Jar> {
        Arc::clone(&self.cookies)
    }

    pub fn store_response(&self, name: &str, response: NamedResponse) -> Result<(), String> {
        lock(&self.responses).store(
            name,
            response,
            Limits {
                entries: MAX_RESPONSES,
                bytes: MAX_RESPONSE_BYTES,
            },
        )
    }

    #[cfg(test)]
    fn store_response_within(
        &self,
        name: &str,
        response: NamedResponse,
        entries: usize,
        bytes: usize,
    ) -> Result<(), String> {
        lock(&self.responses).store(name, response, Limits { entries, bytes })
    }

    #[cfg(test)]
    pub fn response(&self, name: &str) -> Option<Arc<NamedResponse>> {
        lock(&self.responses).entries.get(name).cloned()
    }

    pub fn responses(&self) -> HashMap<String, Arc<NamedResponse>> {
        lock(&self.responses).entries.clone()
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
        session
            .apply_globals(&[
                GlobalChange::Set("a".to_owned(), json!("1")),
                GlobalChange::Set("b".to_owned(), json!(2)),
                GlobalChange::ClearAll,
                GlobalChange::Set("c".to_owned(), json!(true)),
                GlobalChange::Set("d".to_owned(), json!("x")),
                GlobalChange::Clear("d".to_owned()),
            ])
            .unwrap();
        assert_eq!(
            session.globals(),
            BTreeMap::from([("c".to_owned(), json!(true))])
        );
    }

    #[test]
    fn rejects_oversized_globals_atomically() {
        let session = Session::new();
        let error = session
            .apply_globals_within(
                &[GlobalChange::Set("big".to_owned(), json!("xxxxxxx"))],
                Limits {
                    entries: 10,
                    bytes: 8,
                },
            )
            .unwrap_err();
        assert!(error.contains("exceed"), "{error}");
        assert!(session.globals().is_empty());

        session
            .apply_globals_within(
                &[
                    GlobalChange::Set("a".to_owned(), json!("keep")),
                    GlobalChange::ClearAll,
                    GlobalChange::Set("b".to_owned(), json!("ok")),
                ],
                Limits {
                    entries: 10,
                    bytes: 1024,
                },
            )
            .unwrap();
        assert_eq!(
            session.globals(),
            BTreeMap::from([("b".to_owned(), json!("ok"))])
        );

        let error = session
            .apply_globals_within(
                &[
                    GlobalChange::Set("one".to_owned(), json!(1)),
                    GlobalChange::Set("two".to_owned(), json!(2)),
                ],
                Limits {
                    entries: 1,
                    bytes: 1024,
                },
            )
            .unwrap_err();
        assert!(error.contains("entries"), "{error}");
        assert!(!session.globals().contains_key("one"));
    }

    #[test]
    fn stores_named_responses() {
        let session = Session::new();
        assert!(session.response("login").is_none());
        session
            .store_response(
                "login",
                NamedResponse {
                    status: Some(200),
                    headers: vec![("x-token".to_owned(), "abc".to_owned())],
                    body: json!({ "token": "abc" }),
                },
            )
            .unwrap();
        assert_eq!(session.response("login").unwrap().body["token"], "abc");
    }

    #[test]
    fn response_budget_evicts_oldest_and_never_leaves_stale_entries() {
        let session = Session::new();
        let response = |body: &str| NamedResponse {
            body: json!(body),
            ..NamedResponse::default()
        };
        session
            .store_response_within("a", response("a"), 2, 4096)
            .unwrap();
        session
            .store_response_within("b", response("b"), 2, 4096)
            .unwrap();
        session
            .store_response_within("c", response("c"), 2, 4096)
            .unwrap();
        assert!(session.response("a").is_none());
        assert!(session.response("b").is_some());
        assert!(session.response("c").is_some());

        session
            .store_response_within("c", response("c2"), 2, 4096)
            .unwrap();
        session
            .store_response_within("d", response("d"), 2, 4096)
            .unwrap();
        assert!(session.response("b").is_none());
        assert_eq!(session.response("c").unwrap().body, json!("c2"));
        assert!(session.response("d").is_some());

        let error = session
            .store_response_within("c", response("way too big"), 2, 16)
            .unwrap_err();
        assert!(error.contains("larger than"), "{error}");
        assert!(session.response("c").is_none());
    }
}
