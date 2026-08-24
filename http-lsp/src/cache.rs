use std::sync::Arc;

use chrono::{DateTime, Local};
use dashmap::DashMap;
use tower_lsp::lsp_types::Url;

use crate::httpyac::Exchange;

#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub exchange: Arc<Exchange>,
    pub received_at: DateTime<Local>,
}

#[derive(Debug, Default)]
pub struct ResponseCache {
    entries: DashMap<(Url, u32), CachedResponse>,
}

impl ResponseCache {
    pub fn insert(&self, uri: Url, line: u32, exchange: Arc<Exchange>) {
        self.entries.insert(
            (uri, line),
            CachedResponse {
                exchange,
                received_at: Local::now(),
            },
        );
    }

    pub fn get(&self, uri: &Url, line: u32) -> Option<CachedResponse> {
        self.entries
            .get(&(uri.clone(), line))
            .map(|entry| entry.clone())
    }

    pub fn remove_document(&self, uri: &Url) {
        self.entries.retain(|(cached_uri, _), _| cached_uri != uri);
    }
}
