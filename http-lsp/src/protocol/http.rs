//! HTTP over reqwest with rustls. Redirect policy, timeouts and cookie saving are client-level
//! settings in reqwest, so one client is cached per combination; all of them share the session's
//! cookie jar. Idle connections are pooled briefly and sparingly, and the cache is bounded, so
//! requests to many hosts cannot exhaust file descriptors (macOS allows 256 by default).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use reqwest::{
    cookie::{CookieStore, Jar},
    header::{HeaderName, HeaderValue},
    redirect, Client, Method, Url, Version,
};

use super::{
    error_chain, install_crypto_provider, Context, PreparedBody, PreparedRequest, Response,
    DEFAULT_TIMEOUT, MAX_BODY_BYTES,
};

const MAX_REDIRECTS: usize = 10;
/// Distinct `@timeout` values can create many clients; past this the cache starts over.
const MAX_CACHED_CLIENTS: usize = 16;
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const POOL_MAX_IDLE_PER_HOST: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum VersionPreference {
    Any,
    Http1,
    Http2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ClientKey {
    follow_redirects: bool,
    save_cookies: bool,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    version: VersionPreference,
}

pub struct Clients {
    cookies: Arc<Jar>,
    cache: Mutex<HashMap<ClientKey, Client>>,
}

impl Clients {
    pub fn new(cookies: Arc<Jar>) -> Self {
        install_crypto_provider();
        Self {
            cookies,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn client(&self, key: ClientKey) -> Result<Client, String> {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(client) = cache.get(&key) {
            return Ok(client.clone());
        }
        let mut builder = Client::builder()
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
            .redirect(if key.follow_redirects {
                redirect::Policy::limited(MAX_REDIRECTS)
            } else {
                redirect::Policy::none()
            });
        builder = if key.save_cookies {
            builder.cookie_provider(Arc::clone(&self.cookies))
        } else {
            builder.cookie_provider(Arc::new(ReadOnlyCookies(Arc::clone(&self.cookies))))
        };
        if let Some(timeout) = key.connect_timeout {
            builder = builder.connect_timeout(timeout);
        }
        if let Some(timeout) = key.read_timeout {
            builder = builder.read_timeout(timeout);
        }
        builder = match key.version {
            VersionPreference::Any => builder,
            VersionPreference::Http1 => builder.http1_only(),
            VersionPreference::Http2 => builder.http2_prior_knowledge(),
        };
        let client = builder
            .build()
            .map_err(|error| format!("failed to create an HTTP client: {}", error_chain(&error)))?;
        if cache.len() >= MAX_CACHED_CLIENTS {
            // Dropping a client closes its idle connections; in-flight requests keep theirs.
            cache.clear();
        }
        cache.insert(key, client.clone());
        Ok(client)
    }
}

/// `@no-cookie-jar`: send the cookies the session already has, but do not save new ones.
struct ReadOnlyCookies(Arc<Jar>);

impl CookieStore for ReadOnlyCookies {
    fn set_cookies(&self, _: &mut dyn Iterator<Item = &HeaderValue>, _: &Url) {}

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        self.0.cookies(url)
    }
}

pub async fn send(request: &PreparedRequest, context: &Context<'_>) -> Result<Response, String> {
    let url = Url::parse(&request.url)
        .map_err(|error| format!("invalid URL {:?}: {error}", request.url))?;
    let method = Method::from_bytes(request.method.as_bytes())
        .map_err(|_| format!("invalid HTTP method {:?}", request.method))?;
    let version = match request.http_version.as_deref() {
        None => VersionPreference::Any,
        Some("HTTP/1.0" | "HTTP/1.1") => VersionPreference::Http1,
        Some("HTTP/2" | "HTTP/2.0") => VersionPreference::Http2,
        Some(other) => return Err(format!("unsupported HTTP version {other:?}")),
    };
    let directives = &request.directives;
    let client = context.http.client(ClientKey {
        follow_redirects: !directives.no_redirect,
        save_cookies: !directives.no_cookie_jar,
        connect_timeout: directives.connection_timeout,
        read_timeout: directives.timeout,
        version,
    })?;

    // `@timeout` bounds inactivity; the overall deadline only guards against runaway transfers.
    let deadline = directives
        .timeout
        .map_or(DEFAULT_TIMEOUT, |timeout| timeout.max(DEFAULT_TIMEOUT));
    let mut builder = client.request(method, url).timeout(deadline);
    if version == VersionPreference::Http1 && request.http_version.as_deref() == Some("HTTP/1.0") {
        builder = builder.version(Version::HTTP_10);
    }
    for header in &request.headers {
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| format!("invalid header name {:?}", header.name))?;
        let value = HeaderValue::from_str(&header.value)
            .map_err(|_| format!("invalid value for header {}", header.name))?;
        builder = builder.header(name, value);
    }
    if let PreparedBody::Bytes(body) = &request.body {
        builder = builder.body(body.clone());
    }

    let started = Instant::now();
    let mut response = builder.send().await.map_err(|error| error_chain(&error))?;
    let status = response.status().as_u16();
    let status_line = format!("{:?}", response.version());
    let final_url = response.url().to_string();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let mut body = Vec::new();
    let mut error = None;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > MAX_BODY_BYTES {
                    body.extend_from_slice(&chunk[..MAX_BODY_BYTES - body.len()]);
                    error = Some(format!(
                        "response body exceeded the {} MiB limit and was truncated",
                        MAX_BODY_BYTES / 1024 / 1024
                    ));
                    break;
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(failure) => {
                error = Some(format!(
                    "failed to read the response body: {}",
                    error_chain(&failure)
                ));
                break;
            }
        }
    }

    Ok(Response {
        success: error.is_none(),
        status: Some(status),
        status_line,
        url: final_url,
        headers,
        content_type,
        body,
        formatted: None,
        elapsed: started.elapsed(),
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_the_client_cache() {
        let clients = Clients::new(Arc::new(Jar::default()));
        for seconds in 0..(MAX_CACHED_CLIENTS as u64 * 2 + 3) {
            clients
                .client(ClientKey {
                    follow_redirects: true,
                    save_cookies: true,
                    connect_timeout: None,
                    read_timeout: Some(Duration::from_secs(seconds + 1)),
                    version: VersionPreference::Any,
                })
                .unwrap();
            let cached = clients.cache.lock().unwrap().len();
            assert!(cached <= MAX_CACHED_CLIENTS, "{cached}");
        }
    }
}
