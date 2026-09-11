//! codex-bridge — a passthrough proxy to the ChatGPT Codex backend.
//!
//! Every request is forwarded verbatim (method, path, query, body) to
//! CODEX_UPSTREAM with the subscription bearer token and the headers
//! the backend uses to classify traffic as the Codex CLI. Whatever the
//! upstream answers — success, SSE stream, or error — is returned as-is.
//! The only added behaviour is OAuth token refresh: proactively before
//! expiry, and once more on an upstream 401.

mod auth;
#[cfg(test)]
#[path = "__tests__/support.rs"]
mod support;

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
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
}

// Hop-by-hop and connection-specific headers that must not be relayed
// in either direction. `accept-encoding` is dropped so the upstream
// answers uncompressed and the body can be streamed through untouched.
const STRIP_REQUEST_HEADERS: &[&str] = &[
    "host",
    "authorization",
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

async fn proxy(app: Arc<App>, req: Request) -> Result<Response, Error> {
    let (parts, body) = req.into_parts();
    let path_and_query = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    // `/usage` is the one path that lives outside the codex root
    // (backend-api/wham/usage); everything else maps 1:1 onto CODEX_UPSTREAM.
    let target = if parts.method == axum::http::Method::GET && parts.uri.path() == "/usage" {
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
// request against the subscription.
async fn health(State(app): State<Arc<App>>) -> Response {
    match app.tokens.status().await {
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

async fn refresh(State(app): State<Arc<App>>) -> Response {
    match app.tokens.refresh_now().await {
        Ok(_) => health(State(app)).await,
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

    println!("codex-bridge listening on http://localhost:{port}");
    println!("  upstream:    {upstream}");
    println!("  credentials: {}", tokens.path().display());

    let app = Arc::new(App {
        client,
        upstream,
        usage_url,
        user_agent,
        tokens,
    });
    let router = router(app);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
#[path = "__tests__/main.rs"]
mod tests;
