//! codex-bridge — a passthrough proxy to the ChatGPT Codex backend.
//!
//! Every request is forwarded verbatim (method, path, query, body) to
//! CODEX_UPSTREAM with the subscription bearer token and the headers
//! the backend uses to classify traffic as the Codex CLI. Whatever the
//! upstream answers — success, SSE stream, or error — is returned as-is.
//! The only added behaviour is OAuth token refresh: proactively before
//! expiry, and once more on an upstream 401, plus an optional API key
//! on the client side (BRIDGE_API_KEY).

mod device_flow;

// Shared with the lib crate's auth.rs tests; this binary's own tests
// only exercise a subset of these fixtures.
#[cfg(test)]
#[path = "__tests__/support.rs"]
#[allow(dead_code)]
mod support;

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

use codex_bridge::auth::{self, DevicePoll, TokenStore};
use codex_bridge::{api_keys_from_env, env_or, presented_key, secret_eq, Error};
use device_flow::{DeviceFlowStore, Phase};

// Paths reachable without BRIDGE_API_KEY. `/health` so a load balancer
// (which cannot attach a key) can still probe liveness; `/auth` so the
// device-code sign-in page itself can be opened in a bare browser tab —
// the page is static and touches no credentials. The two endpoints it
// calls (`/auth/device/start`, `/auth/device/poll`), which do, stay
// behind the guard like everything else.
const OPEN_PATHS: &[&str] = &["/health", "/auth"];

struct App {
    client: reqwest::Client,
    upstream: String,
    usage_url: String,
    user_agent: String,
    tokens: TokenStore,
    /// Keys a caller may present. Empty = the bridge is unauthenticated.
    api_keys: Vec<String>,
    device_flows: DeviceFlowStore,
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

fn json_error(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "error": { "message": message, "type": "bridge_error" } });
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn json_ok(body: serde_json::Value) -> Response {
    (
        StatusCode::OK,
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
// "unused" there. `api_keys_from_env`/`secret_eq`/`presented_key` live in
// the lib crate so codex-mcp's HTTP transport can reuse the same gate.
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
// account details from unauthenticated callers. `/auth` (the device-code
// sign-in page) is open for the same reason a login page has to be: it
// is what gets a caller their first key. See `OPEN_PATHS`.
async fn guard(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    if OPEN_PATHS.contains(&req.uri().path()) || authorized(&app, req.headers()) {
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

// --- device-code sign-in (`GET /auth`) ----------------------------------

async fn auth_page() -> Html<&'static str> {
    Html(include_str!("auth_page.html"))
}

async fn device_start(State(app): State<Arc<App>>) -> Response {
    let code = match app.tokens.request_device_code().await {
        Ok(code) => code,
        Err(err) => return json_error(StatusCode::BAD_GATEWAY, &err.to_string()),
    };
    let (flow_id, expires_at) = app.device_flows.create(&code).await;
    let ttl = expires_at.saturating_duration_since(tokio::time::Instant::now());
    json_ok(serde_json::json!({
        "flowId": flow_id,
        "userCode": code.user_code,
        "verificationUri": code.verification_uri,
        "expiresAt": unix_secs(std::time::SystemTime::now()) + ttl.as_secs(),
        "intervalSeconds": code.interval.as_secs(),
    }))
}

#[derive(serde::Deserialize)]
struct DevicePollRequest {
    #[serde(rename = "flowId")]
    flow_id: String,
}

// Mirrors the shape of the flow the WebUI drives: the browser polls on
// its own timer, but only a fraction of those calls actually reach
// auth.openai.com — `DeviceFlowStore` throttles by the vendor's own
// `interval` regardless of how eagerly the client asks.
async fn device_poll(State(app): State<Arc<App>>, body: Bytes) -> Response {
    let Ok(req) = serde_json::from_slice::<DevicePollRequest>(&body) else {
        return json_error(StatusCode::BAD_REQUEST, "missing flowId");
    };
    let Some(flow) = app.device_flows.get(&req.flow_id).await else {
        return json_ok(serde_json::json!({ "status": "expired" }));
    };
    match flow.phase {
        Phase::Connected => return json_ok(serde_json::json!({ "status": "connected" })),
        Phase::Completing => return json_ok(serde_json::json!({ "status": "pending" })),
        Phase::Polling => {}
    }
    let now = tokio::time::Instant::now();
    if now >= flow.expires_at {
        app.device_flows.delete(&req.flow_id).await;
        return json_ok(serde_json::json!({ "status": "expired" }));
    }
    if now < flow.next_poll_at {
        return json_ok(serde_json::json!({ "status": "pending" }));
    }

    // Claimed before the upstream call, not after: a poll arriving while
    // this one is still waiting on auth.openai.com must answer `pending`
    // from memory, or both could reach upstream and the single-use code
    // would be exchanged twice.
    app.device_flows.mark_polled(&req.flow_id).await;
    match app
        .tokens
        .poll_device_code(&flow.device_auth_id, &flow.user_code)
        .await
    {
        Ok(DevicePoll::Pending) => json_ok(serde_json::json!({ "status": "pending" })),
        Ok(DevicePoll::Authorized { code, code_verifier }) => {
            app.device_flows.set_phase(&req.flow_id, Phase::Completing).await;
            match app.tokens.install_from_device_code(&code, &code_verifier).await {
                Ok(_) => {
                    app.device_flows.set_phase(&req.flow_id, Phase::Connected).await;
                    json_ok(serde_json::json!({ "status": "connected" }))
                }
                Err(err) => {
                    // The grant's code is single-use and already spent;
                    // there is nothing left to retry on this flow.
                    app.device_flows.delete(&req.flow_id).await;
                    json_error(StatusCode::BAD_GATEWAY, &err.to_string())
                }
            }
        }
        Err(err) => {
            app.device_flows.delete(&req.flow_id).await;
            json_error(StatusCode::BAD_GATEWAY, &err.to_string())
        }
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
        .route("/auth", get(auth_page))
        .route("/auth/device/start", post(device_start))
        .route("/auth/device/poll", post(device_poll))
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
    let api_keys = api_keys_from_env("BRIDGE_API_KEY");

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
        device_flows: DeviceFlowStore::new(),
    });
    let router = router(app);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
#[path = "__tests__/main.rs"]
mod tests;
