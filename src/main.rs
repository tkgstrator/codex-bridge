//! codex-bridge — a passthrough proxy to the ChatGPT Codex backend.
//!
//! Every request is forwarded verbatim (method, path, query, body) to
//! CODEX_UPSTREAM with the subscription bearer token and the headers
//! the backend uses to classify traffic as the Codex CLI. Whatever the
//! upstream answers — success, SSE stream, or error — is returned as-is.
//! The only added behaviour is OAuth token refresh: proactively before
//! expiry, and once more on an upstream 401, plus an optional API key
//! on the client side (BRIDGE_API_KEY).

mod auth;
#[cfg(test)]
#[path = "__tests__/support.rs"]
mod support;

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

use auth::TokenStore;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

struct App {
    client: reqwest::Client,
    upstream: String,
    usage_url: String,
    user_agent: String,
    tokens: TokenStore,
    /// Keys a caller may present. Empty = the bridge is unauthenticated.
    api_keys: Vec<String>,
}

// Hop-by-hop and connection-specific headers that must not be relayed
// in either direction. `accept-encoding` is dropped so the upstream
// answers uncompressed and the body can be streamed through untouched.
// `authorization` and `x-api-key` are the bridge's own credentials: they
// are consumed here and never reach the upstream, which authenticates
// with the subscription token instead.
const STRIP_REQUEST_HEADERS: &[&str] = &[
    "host",
    "authorization",
    "x-api-key",
    "connection",
    "content-length",
    "transfer-encoding",
    "accept-encoding",
    "keep-alive",
    "proxy-authorization",
    "te",
    "trailer",
    "upgrade",
];
const STRIP_RESPONSE_HEADERS: &[&str] = &["content-length", "transfer-encoding", "connection"];

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.into())
}

fn json_error(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "error": { "message": message, "type": "bridge_error" } });
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

// --- client-side authentication ----------------------------------------
//
// Opt-in: with BRIDGE_API_KEY unset the bridge answers anyone who can
// reach it, so it must stay behind a firewall. Since every OpenAI SDK
// requires an `api_key` and sends it as `Authorization: Bearer <key>`,
// turning this on costs the caller nothing — they just stop writing
// "unused" there. Comma-separated values let a key be rotated without
// downtime: publish the new one, drop the old one once clients moved.
fn api_keys_from_env() -> Vec<String> {
    env_or("BRIDGE_API_KEY", "")
        .split(',')
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .collect()
}

// Compare without an early exit, so a wrong key cannot be guessed byte
// by byte from the response time. The lengths are not hidden, but the
// presented one is the attacker's own input anyway.
fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

// `Authorization: Bearer <key>` is what the OpenAI SDKs send; `x-api-key`
// is accepted too because Anthropic-shaped clients and a few gateways
// only know that one.
fn presented_key(headers: &HeaderMap) -> Option<&str> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .and_then(|(scheme, token)| scheme.eq_ignore_ascii_case("bearer").then_some(token.trim()));
    bearer.or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
}

fn authorized(app: &App, headers: &HeaderMap) -> bool {
    if app.api_keys.is_empty() {
        return true;
    }
    let Some(presented) = presented_key(headers) else {
        return false;
    };
    app.api_keys.iter().any(|key| secret_eq(key, presented))
}

// `/health` is left open so a load balancer — which cannot attach a key
// to its health check — can still probe it; the handler withholds the
// account details from unauthenticated callers.
async fn guard(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    if req.uri().path() == "/health" || authorized(&app, req.headers()) {
        return next.run(req).await;
    }
    println!("[proxy] {} {} -> 401 (bridge)", req.method(), req.uri().path());
    (
        [(header::WWW_AUTHENTICATE, "Bearer")],
        json_error(StatusCode::UNAUTHORIZED, "missing or invalid API key"),
    )
        .into_response()
}

fn upstream_headers(app: &App, inbound: &HeaderMap, creds: &auth::Credentials) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in inbound {
        if !STRIP_REQUEST_HEADERS.contains(&name.as_str()) {
            headers.append(name.clone(), value.clone());
        }
    }
    let set = |headers: &mut HeaderMap, name: &'static str, value: &str| {
        if let Ok(v) = HeaderValue::from_str(value) {
            headers.insert(HeaderName::from_static(name), v);
        }
    };
    set(
        &mut headers,
        "authorization",
        &format!("Bearer {}", creds.access_token),
    );
    // The backend bills a request against the subscription only when it
    // looks like the official CLI (originator + user-agent).
    set(&mut headers, "originator", "codex_cli");
    set(&mut headers, "user-agent", &app.user_agent);
    if let Some(id) = &creds.account_id {
        set(&mut headers, "chatgpt-account-id", id);
    }
    if !headers.contains_key("session_id") {
        set(&mut headers, "session_id", &uuid::Uuid::new_v4().to_string());
    }
    headers
}

async fn send(
    app: &App,
    method: &axum::http::Method,
    target: &str,
    inbound: &HeaderMap,
    body: &Bytes,
    stale: Option<&str>,
) -> Result<(reqwest::Response, String), Error> {
    let creds = app.tokens.get(stale).await?;
    let res = app
        .client
        .request(method.clone(), target)
        .headers(upstream_headers(app, inbound, &creds))
        .body(body.clone())
        .send()
        .await?;
    Ok((res, creds.access_token))
}

// OpenAI SDKs default to a base_url ending in `/v1` and LiteLLM & co.
// hard-code `<base>/v1/responses`; the Codex backend has no such prefix.
fn strip_v1(path_and_query: &str) -> &str {
    match path_and_query.strip_prefix("/v1") {
        Some(rest) if rest.starts_with('/') => rest,
        _ => path_and_query,
    }
}

async fn proxy(app: Arc<App>, req: Request) -> Result<Response, Error> {
    let (parts, body) = req.into_parts();
    let path_and_query = strip_v1(parts.uri.path_and_query().map_or("/", |p| p.as_str()));
    let path = path_and_query.split('?').next().unwrap_or_default();
    // `/usage` is the one path that lives outside the codex root
    // (backend-api/wham/usage); everything else maps 1:1 onto CODEX_UPSTREAM.
    let target = if parts.method == axum::http::Method::GET && path == "/usage" {
        app.usage_url.clone()
    } else {
        format!("{}{}", app.upstream, path_and_query)
    };
    // Buffer the body so it can be replayed on the 401 retry.
    let body = axum::body::to_bytes(body, usize::MAX).await?;

    let (mut upstream, used) = send(&app, &parts.method, &target, &parts.headers, &body, None).await?;
    if upstream.status() == StatusCode::UNAUTHORIZED {
        println!("[proxy] upstream 401, refreshing token and retrying once");
        (upstream, _) = send(&app, &parts.method, &target, &parts.headers, &body, Some(&used)).await?;
    }
    println!(
        "[proxy] {} {} -> {}",
        parts.method,
        parts.uri.path(),
        upstream.status()
    );

    let mut response = Response::builder().status(upstream.status());
    if let Some(headers) = response.headers_mut() {
        for (name, value) in upstream.headers() {
            if !STRIP_RESPONSE_HEADERS.contains(&name.as_str()) {
                headers.append(name.clone(), value.clone());
            }
        }
    }
    Ok(response.body(Body::from_stream(upstream.bytes_stream()))?)
}

async fn handle(State(app): State<Arc<App>>, req: Request) -> Response {
    match proxy(app, req).await {
        Ok(res) => res,
        Err(err) => {
            eprintln!("[proxy] {err}");
            json_error(StatusCode::BAD_GATEWAY, &err.to_string())
        }
    }
}

fn unix_secs(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

// Liveness plus a non-secret view of the stored grant, so a monitor can
// tell "process up" from "refresh_token is dead" without spending a
// request against the subscription. An unauthenticated caller still
// gets the liveness verdict (and the 503 when the grant is broken), but
// not the identity behind it.
async fn health_status(app: &App, detailed: bool) -> Response {
    match app.tokens.status().await {
        Ok(_) if !detailed => {
            ([(header::CONTENT_TYPE, "application/json")], r#"{"ok":true}"#).into_response()
        }
        Err(_) if !detailed => json_error(StatusCode::SERVICE_UNAVAILABLE, "credentials unavailable"),
        Ok(s) => {
            let now = std::time::SystemTime::now();
            let expires_at = s.expires_at.map(unix_secs);
            let expires_in = s
                .expires_at
                .map(|t| t.duration_since(now).map_or(0, |d| d.as_secs()));
            let body = serde_json::json!({
                "ok": true,
                "account_id": s.account_id,
                "email": s.email,
                "plan_type": s.plan_type,
                "token": {
                    "expires_at": expires_at,
                    "expires_in_seconds": expires_in,
                    "last_refresh": s.last_refresh,
                    "has_refresh_token": s.has_refresh_token,
                },
            });
            ([(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
        }
        Err(err) => json_error(StatusCode::SERVICE_UNAVAILABLE, &err.to_string()),
    }
}

async fn health(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let detailed = authorized(&app, &headers);
    health_status(&app, detailed).await
}

// Reachable only through the guard, so the answer is always detailed.
async fn refresh(State(app): State<Arc<App>>) -> Response {
    match app.tokens.refresh_now().await {
        Ok(_) => health_status(&app, true).await,
        Err(err) => json_error(StatusCode::BAD_GATEWAY, &err.to_string()),
    }
}

// `codex-bridge --health`: probe the running server and exit 0/1, so a
// Docker HEALTHCHECK works inside distroless where there is no curl.
async fn health_probe(port: u16) -> Result<(), Error> {
    let res = reqwest::get(format!("http://127.0.0.1:{port}/health")).await?;
    if res.status().is_success() {
        Ok(())
    } else {
        Err(format!("health probe returned {}", res.status()).into())
    }
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/refresh", post(refresh))
        .fallback(handle)
        // `layer` (not `route_layer`) so the fallback — i.e. everything
        // that gets proxied — is guarded too.
        .layer(axum::middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let port: u16 = env_or("PORT", "3000").parse()?;
    if std::env::args().nth(1).as_deref() == Some("--health") {
        return health_probe(port).await;
    }
    let upstream = env_or("CODEX_UPSTREAM", "https://chatgpt.com/backend-api/codex")
        .trim_end_matches('/')
        .to_string();
    let usage_url = env_or("CODEX_USAGE_URL", "https://chatgpt.com/backend-api/wham/usage");
    let cli_version = env_or("CODEX_CLI_VERSION", "0.0.0");
    let user_agent = format!(
        "codex_cli/{cli_version} ({} {}; {})",
        std::env::consts::OS,
        std::env::consts::FAMILY,
        std::env::consts::ARCH
    );
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let tokens = TokenStore::new(auth::default_auth_path(), client.clone());
    let api_keys = api_keys_from_env();

    println!("codex-bridge listening on http://localhost:{port}");
    println!("  upstream:    {upstream}");
    println!("  credentials: {}", tokens.path().display());
    match api_keys.len() {
        0 => {
            println!("  auth:        DISABLED — set BRIDGE_API_KEY, or restrict access at the network level")
        }
        n => println!("  auth:        BRIDGE_API_KEY ({n} key(s))"),
    }

    let app = Arc::new(App {
        client,
        upstream,
        usage_url,
        user_agent,
        tokens,
        api_keys,
    });
    let router = router(app);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
#[path = "__tests__/main.rs"]
mod tests;
