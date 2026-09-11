//! Fixtures shared by the unit tests: throwaway credential files, mock
//! HTTP servers on an ephemeral port, and unsigned JWTs with chosen claims.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};

pub const ACCOUNT_ID: &str = "acct-123";
pub const EMAIL: &str = "user@example.com";
pub const PLAN: &str = "plus";

/// A file under the OS temp dir, removed on drop.
pub struct TempFile {
    pub path: PathBuf,
}

impl TempFile {
    /// A path that does not exist yet.
    pub fn missing() -> Self {
        let name = format!(
            "codex-bridge-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        );
        Self {
            path: std::env::temp_dir().join(name),
        }
    }

    pub fn json(contents: &Value) -> Self {
        let file = Self::missing();
        file.write(&serde_json::to_vec_pretty(contents).unwrap());
        file
    }

    pub fn write(&self, bytes: &[u8]) {
        std::fs::write(&self.path, bytes).unwrap();
    }

    pub fn read(&self) -> Value {
        serde_json::from_slice(&std::fs::read(&self.path).unwrap()).unwrap()
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// `header.payload.signature` with an unverified signature — the proxy
/// only ever reads claims, it never validates.
pub fn jwt(claims: Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    format!("{header}.{payload}.sig")
}

/// Access token expiring `expires_in` seconds from now (negative = past).
/// `jti` keeps two tokens minted in the same second distinguishable.
pub fn access_token(expires_in: i64) -> String {
    jwt(json!({ "exp": now_secs() + expires_in, "sub": "user", "jti": uuid::Uuid::new_v4().to_string() }))
}

/// Access token whose account id lives only in the namespaced claim.
pub fn access_token_with_account(expires_in: i64, account_id: &str) -> String {
    jwt(json!({
        "exp": now_secs() + expires_in,
        "https://api.openai.com/auth": { "chatgpt_account_id": account_id },
    }))
}

pub fn id_token(account_id: &str) -> String {
    jwt(json!({
        "email": EMAIL,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account_id,
            "chatgpt_plan_type": PLAN,
        },
    }))
}

/// The shape `codex login` writes to ~/.codex/auth.json.
pub fn auth_json(tokens: Value) -> Value {
    json!({ "OPENAI_API_KEY": null, "tokens": tokens })
}

/// A complete, fresh grant: the common starting point for most tests.
pub fn fresh_auth() -> Value {
    auth_json(json!({
        "access_token": access_token(3600),
        "refresh_token": "rt-1",
        "id_token": id_token(ACCOUNT_ID),
        "account_id": ACCOUNT_ID,
    }))
}

/// What https://auth.openai.com/oauth/token answers on a successful rotation.
pub fn rotated_grant(refresh_token: &str) -> Value {
    json!({
        "access_token": access_token(3600),
        "refresh_token": refresh_token,
        "id_token": id_token(ACCOUNT_ID),
        "token_type": "Bearer",
        "expires_in": 3600,
    })
}

pub fn json_response(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// One request as seen by a [`MockServer`].
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: Method,
    pub uri: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Decode an `application/x-www-form-urlencoded` body (no percent
    /// decoding — the values the proxy sends are all URL-safe).
    pub fn form(&self, key: &str) -> Option<&str> {
        std::str::from_utf8(&self.body)
            .ok()?
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    pub fn body_json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

type Responder = dyn Fn(usize, &Recorded) -> Response + Send + Sync;

#[derive(Clone)]
struct MockState {
    requests: Arc<Mutex<Vec<Recorded>>>,
    respond: Arc<Responder>,
}

/// HTTP server on 127.0.0.1 that records every request and answers with
/// whatever the responder returns for the (0-based) request index.
#[derive(Clone)]
pub struct MockServer {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockServer {
    pub async fn start(respond: impl Fn(usize, &Recorded) -> Response + Send + Sync + 'static) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = MockState {
            requests: requests.clone(),
            respond: Arc::new(respond),
        };
        let addr = spawn(Router::new().fallback(record).with_state(state)).await;
        Self { addr, requests }
    }

    /// A token endpoint that hands out a fresh grant on every call.
    pub async fn token_endpoint(next_refresh_token: &'static str) -> Self {
        Self::start(move |_, _| json_response(StatusCode::OK, rotated_grant(next_refresh_token))).await
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

async fn record(State(state): State<MockState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let recorded = Recorded {
        method: parts.method,
        uri: parts.uri.to_string(),
        headers: parts.headers,
        body,
    };
    let index = {
        let mut requests = state.requests.lock().unwrap();
        requests.push(recorded.clone());
        requests.len() - 1
    };
    (state.respond)(index, &recorded)
}

/// Serve `router` on an ephemeral port for the rest of the test.
pub async fn spawn(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}
