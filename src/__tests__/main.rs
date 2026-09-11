use super::*;
use crate::support::*;
use serde_json::{json, Value};

const USER_AGENT: &str = "codex_cli/test (linux unix; x86_64)";

/// A running bridge wired to mock upstream, usage and token servers.
struct Bridge {
    addr: std::net::SocketAddr,
    upstream: MockServer,
    usage: MockServer,
    token_endpoint: MockServer,
    file: TempFile,
    http: reqwest::Client,
}

impl Bridge {
    async fn start(
        file: TempFile,
        respond: impl Fn(usize, &Recorded) -> Response + Send + Sync + 'static,
    ) -> Self {
        Self::guarded(file, Vec::new(), respond).await
    }

    /// Same, but with BRIDGE_API_KEY-style keys configured.
    async fn guarded(
        file: TempFile,
        api_keys: Vec<String>,
        respond: impl Fn(usize, &Recorded) -> Response + Send + Sync + 'static,
    ) -> Self {
        let upstream = MockServer::start(respond).await;
        let usage = MockServer::start(|_, _| json_response(StatusCode::OK, json!({ "usage": true }))).await;
        let token_endpoint = MockServer::token_endpoint("rt-2").await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let tokens = auth::TokenStore::with_token_url(
            file.path.clone(),
            token_endpoint.url("/oauth/token"),
            client.clone(),
        );
        let app = Arc::new(App {
            client,
            upstream: upstream.url("/backend-api/codex"),
            usage_url: usage.url("/backend-api/wham/usage"),
            user_agent: USER_AGENT.into(),
            tokens,
            api_keys,
        });
        Self {
            addr: spawn(router(app)).await,
            upstream,
            usage,
            token_endpoint,
            file,
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn current_access_token(&self) -> String {
        self.file.read()["tokens"]["access_token"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

fn ok_text(body: &'static str) -> impl Fn(usize, &Recorded) -> Response + Send + Sync + 'static {
    move |_, _| (StatusCode::OK, body).into_response()
}

async fn json_body(res: reqwest::Response) -> Value {
    serde_json::from_slice(&res.bytes().await.unwrap()).unwrap()
}

// --- pure helpers -------------------------------------------------------

#[test]
fn env_or_treats_empty_as_unset() {
    std::env::remove_var("CODEX_BRIDGE_TEST_UNSET");
    assert_eq!(env_or("CODEX_BRIDGE_TEST_UNSET", "dflt"), "dflt");
    std::env::set_var("CODEX_BRIDGE_TEST_EMPTY", "");
    assert_eq!(env_or("CODEX_BRIDGE_TEST_EMPTY", "dflt"), "dflt");
    std::env::set_var("CODEX_BRIDGE_TEST_SET", "value");
    assert_eq!(env_or("CODEX_BRIDGE_TEST_SET", "dflt"), "value");
}

#[tokio::test]
async fn json_error_has_the_bridge_error_shape() {
    let res = json_error(StatusCode::BAD_GATEWAY, "boom");
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "application/json");
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        body,
        json!({ "error": { "message": "boom", "type": "bridge_error" } })
    );
}

#[test]
fn strip_v1_only_removes_a_leading_path_segment() {
    assert_eq!(strip_v1("/v1/responses"), "/responses");
    assert_eq!(strip_v1("/v1/responses?stream=true"), "/responses?stream=true");
    assert_eq!(strip_v1("/responses"), "/responses");
    assert_eq!(strip_v1("/v1beta/responses"), "/v1beta/responses");
    assert_eq!(strip_v1("/v1"), "/v1");
    assert_eq!(strip_v1("/"), "/");
}

// --- proxying -----------------------------------------------------------

#[tokio::test]
async fn forwards_request_verbatim_and_impersonates_the_cli() {
    let bridge = Bridge::start(TempFile::json(&fresh_auth()), |_, _| {
        (
            StatusCode::CREATED,
            [("content-type", "text/event-stream"), ("x-upstream", "yes")],
            "data: hi\n\n",
        )
            .into_response()
    })
    .await;

    let res = bridge
        .http
        .post(bridge.url("/responses?stream=true"))
        .header("content-type", "application/json")
        .header("x-custom", "kept")
        .header("accept-encoding", "gzip, br")
        .header("authorization", "Bearer client-supplied")
        .body(r#"{"model":"gpt-5-codex"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 201);
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    assert_eq!(res.headers()["x-upstream"], "yes");
    assert_eq!(res.text().await.unwrap(), "data: hi\n\n");

    let sent = &bridge.upstream.requests()[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.uri, "/backend-api/codex/responses?stream=true");
    assert_eq!(sent.body_json(), json!({ "model": "gpt-5-codex" }));
    assert_eq!(sent.header("content-type"), Some("application/json"));
    assert_eq!(sent.header("x-custom"), Some("kept"));
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {}", bridge.current_access_token()).as_str())
    );
    assert_eq!(sent.headers.get_all("authorization").iter().count(), 1);
    assert_eq!(sent.header("originator"), Some("codex_cli"));
    assert_eq!(sent.header("user-agent"), Some(USER_AGENT));
    assert_eq!(sent.header("chatgpt-account-id"), Some(ACCOUNT_ID));
    assert!(uuid::Uuid::parse_str(sent.header("session_id").unwrap()).is_ok());
    assert!(sent.header("accept-encoding").is_none());
    assert_eq!(bridge.token_endpoint.count(), 0);
}

#[tokio::test]
async fn accepts_the_v1_prefix_openai_sdks_send() {
    let bridge = Bridge::start(TempFile::json(&fresh_auth()), ok_text("ok")).await;
    let res = bridge
        .http
        .post(bridge.url("/v1/responses?stream=true"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(
        bridge.upstream.requests()[0].uri,
        "/backend-api/codex/responses?stream=true"
    );

    let res = bridge.http.get(bridge.url("/v1/usage")).send().await.unwrap();
    assert_eq!(json_body(res).await, json!({ "usage": true }));
    assert_eq!(bridge.usage.count(), 1);
    assert_eq!(bridge.upstream.count(), 1);
}

#[tokio::test]
async fn keeps_a_client_supplied_session_id() {
    let bridge = Bridge::start(TempFile::json(&fresh_auth()), ok_text("ok")).await;
    bridge
        .http
        .get(bridge.url("/models"))
        .header("session_id", "sess-1")
        .send()
        .await
        .unwrap();
    assert_eq!(bridge.upstream.requests()[0].header("session_id"), Some("sess-1"));
}

#[tokio::test]
async fn refreshes_proactively_when_the_stored_token_is_about_to_expire() {
    let bridge = Bridge::start(
        TempFile::json(&auth_json(
            json!({ "access_token": access_token(30), "refresh_token": "rt-1" }),
        )),
        ok_text("ok"),
    )
    .await;
    let res = bridge.http.get(bridge.url("/models")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(bridge.token_endpoint.count(), 1);
    assert_eq!(bridge.upstream.count(), 1);
    let rotated = bridge.current_access_token();
    assert_eq!(
        bridge.upstream.requests()[0].header("authorization"),
        Some(format!("Bearer {rotated}").as_str())
    );
}

#[tokio::test]
async fn retries_once_with_a_rotated_token_after_401() {
    let bridge = Bridge::start(TempFile::json(&fresh_auth()), |i, _| match i {
        0 => (StatusCode::UNAUTHORIZED, "expired").into_response(),
        _ => (StatusCode::OK, "ok").into_response(),
    })
    .await;
    let original = bridge.current_access_token();

    let res = bridge
        .http
        .post(bridge.url("/responses"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "ok");

    let sent = bridge.upstream.requests();
    assert_eq!(sent.len(), 2);
    assert_eq!(bridge.token_endpoint.count(), 1);
    let rotated = bridge.current_access_token();
    assert_ne!(rotated, original);
    assert_eq!(
        sent[0].header("authorization"),
        Some(format!("Bearer {original}").as_str())
    );
    assert_eq!(
        sent[1].header("authorization"),
        Some(format!("Bearer {rotated}").as_str())
    );
    // The body is replayed on the retry.
    assert_eq!(sent[1].body, sent[0].body);
    assert_eq!(bridge.file.read()["tokens"]["refresh_token"], "rt-2");
}

#[tokio::test]
async fn gives_up_after_the_second_401() {
    let bridge = Bridge::start(TempFile::json(&fresh_auth()), |_, _| {
        (StatusCode::UNAUTHORIZED, "still no").into_response()
    })
    .await;
    let res = bridge.http.get(bridge.url("/models")).send().await.unwrap();
    assert_eq!(res.status(), 401);
    assert_eq!(res.text().await.unwrap(), "still no");
    assert_eq!(bridge.upstream.count(), 2);
    assert_eq!(bridge.token_endpoint.count(), 1);
}

#[tokio::test]
async fn relays_upstream_errors_untouched() {
    let bridge = Bridge::start(TempFile::json(&fresh_auth()), |_, _| {
        json_response(StatusCode::TOO_MANY_REQUESTS, json!({ "error": "rate_limited" }))
    })
    .await;
    let res = bridge.http.get(bridge.url("/models")).send().await.unwrap();
    assert_eq!(res.status(), 429);
    assert_eq!(json_body(res).await, json!({ "error": "rate_limited" }));
    assert_eq!(bridge.token_endpoint.count(), 0);
}

#[tokio::test]
async fn get_usage_is_routed_to_the_usage_url() {
    let bridge = Bridge::start(TempFile::json(&fresh_auth()), ok_text("ok")).await;
    let res = bridge.http.get(bridge.url("/usage")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(json_body(res).await, json!({ "usage": true }));
    assert_eq!(bridge.usage.requests()[0].uri, "/backend-api/wham/usage");
    assert_eq!(bridge.upstream.count(), 0);
    // Only GET is special-cased; anything else maps onto the codex root.
    bridge.http.post(bridge.url("/usage")).send().await.unwrap();
    assert_eq!(bridge.upstream.requests()[0].uri, "/backend-api/codex/usage");
    assert_eq!(bridge.usage.count(), 1);
}

#[tokio::test]
async fn missing_credentials_yield_502_without_contacting_upstream() {
    let bridge = Bridge::start(TempFile::missing(), ok_text("ok")).await;
    let res = bridge.http.get(bridge.url("/models")).send().await.unwrap();
    assert_eq!(res.status(), 502);
    let body = json_body(res).await;
    assert_eq!(body["error"]["type"], "bridge_error");
    assert!(body["error"]["message"].as_str().unwrap().contains("codex login"));
    assert_eq!(bridge.upstream.count(), 0);
}

#[tokio::test]
async fn unreachable_upstream_yields_502() {
    let file = TempFile::json(&fresh_auth());
    let client = reqwest::Client::new();
    let app = Arc::new(App {
        client: client.clone(),
        upstream: "http://127.0.0.1:1".into(),
        usage_url: "http://127.0.0.1:1/usage".into(),
        user_agent: USER_AGENT.into(),
        tokens: auth::TokenStore::with_token_url(file.path.clone(), "http://127.0.0.1:1".into(), client),
        api_keys: Vec::new(),
    });
    let addr = spawn(router(app)).await;
    let res = reqwest::get(format!("http://{addr}/models")).await.unwrap();
    assert_eq!(res.status(), 502);
    assert_eq!(json_body(res).await["error"]["type"], "bridge_error");
}

// --- /health and /refresh ----------------------------------------------

#[tokio::test]
async fn health_reports_the_stored_grant_without_secrets() {
    let bridge = Bridge::start(TempFile::json(&fresh_auth()), ok_text("ok")).await;
    let res = bridge.http.get(bridge.url("/health")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "application/json");
    let body = json_body(res).await;
    assert_eq!(body["ok"], true);
    assert_eq!(body["account_id"], ACCOUNT_ID);
    assert_eq!(body["email"], EMAIL);
    assert_eq!(body["plan_type"], PLAN);
    assert_eq!(body["token"]["has_refresh_token"], true);
    assert_eq!(body["token"]["last_refresh"], Value::Null);
    let expires_in = body["token"]["expires_in_seconds"].as_u64().unwrap();
    assert!((3590..=3600).contains(&expires_in), "{expires_in}");
    let expires_at = body["token"]["expires_at"].as_i64().unwrap();
    assert!((expires_at - now_secs() - 3600).abs() <= 10, "{expires_at}");
    let access = bridge.current_access_token();
    assert!(!body.to_string().contains(&access));
    assert!(!body.to_string().contains("rt-1"));
    assert_eq!(bridge.upstream.count(), 0);
    assert_eq!(bridge.token_endpoint.count(), 0);
}

#[tokio::test]
async fn health_without_credentials_is_503() {
    let bridge = Bridge::start(TempFile::missing(), ok_text("ok")).await;
    let res = bridge.http.get(bridge.url("/health")).send().await.unwrap();
    assert_eq!(res.status(), 503);
    assert_eq!(json_body(res).await["error"]["type"], "bridge_error");
}

#[tokio::test]
async fn refresh_rotates_and_answers_with_health() {
    let bridge = Bridge::start(TempFile::json(&fresh_auth()), ok_text("ok")).await;
    let before = bridge.current_access_token();

    let res = bridge.http.post(bridge.url("/refresh")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let body = json_body(res).await;
    assert_eq!(body["ok"], true);
    assert_eq!(body["token"]["last_refresh"].as_str().unwrap().len(), 20);
    assert_eq!(bridge.token_endpoint.count(), 1);
    assert_ne!(bridge.current_access_token(), before);
    assert_eq!(bridge.file.read()["tokens"]["refresh_token"], "rt-2");
}

#[tokio::test]
async fn refresh_failure_is_502() {
    let bridge = Bridge::start(
        TempFile::json(&auth_json(json!({ "access_token": access_token(3600) }))),
        ok_text("ok"),
    )
    .await;
    let res = bridge.http.post(bridge.url("/refresh")).send().await.unwrap();
    assert_eq!(res.status(), 502);
    let body = json_body(res).await;
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no refresh_token"));
}

#[tokio::test]
async fn refresh_only_accepts_post() {
    let bridge = Bridge::start(TempFile::json(&fresh_auth()), ok_text("ok")).await;
    let res = bridge.http.get(bridge.url("/refresh")).send().await.unwrap();
    assert_eq!(res.status(), 405);
    assert_eq!(bridge.token_endpoint.count(), 0);
}

// --- client-side authentication ----------------------------------------

const KEY: &str = "sk-bridge-primary";

fn keys() -> Vec<String> {
    vec![KEY.into(), "sk-bridge-rotating".into()]
}

#[test]
fn api_keys_are_split_on_commas_and_trimmed() {
    std::env::set_var("BRIDGE_API_KEY", " sk-one , sk-two ,, ");
    assert_eq!(api_keys_from_env(), vec!["sk-one".to_string(), "sk-two".into()]);
    std::env::set_var("BRIDGE_API_KEY", "");
    assert!(api_keys_from_env().is_empty());
    std::env::remove_var("BRIDGE_API_KEY");
    assert!(api_keys_from_env().is_empty());
}

#[test]
fn secret_eq_matches_only_identical_strings() {
    assert!(secret_eq("sk-abc", "sk-abc"));
    assert!(secret_eq("", ""));
    assert!(!secret_eq("sk-abc", "sk-abd"));
    assert!(!secret_eq("sk-abc", "sk-abc-with-a-suffix"));
    assert!(!secret_eq("sk-abc", ""));
}

#[test]
fn presented_key_accepts_bearer_or_x_api_key() {
    let mut headers = HeaderMap::new();
    assert_eq!(presented_key(&headers), None);
    headers.insert("x-api-key", "from-x".parse().unwrap());
    assert_eq!(presented_key(&headers), Some("from-x"));
    // A bearer token wins, and the scheme is matched case-insensitively.
    headers.insert(header::AUTHORIZATION, "bearer  from-auth ".parse().unwrap());
    assert_eq!(presented_key(&headers), Some("from-auth"));
    // Any other scheme is not a bearer token; fall back to x-api-key.
    headers.insert(header::AUTHORIZATION, "Basic dXNlcjpwYXNz".parse().unwrap());
    assert_eq!(presented_key(&headers), Some("from-x"));
}

#[tokio::test]
async fn proxying_without_a_key_is_401_and_never_reaches_upstream() {
    let bridge = Bridge::guarded(TempFile::json(&fresh_auth()), keys(), ok_text("ok")).await;
    for res in [
        bridge
            .http
            .post(bridge.url("/responses"))
            .body("{}")
            .send()
            .await
            .unwrap(),
        bridge
            .http
            .post(bridge.url("/responses"))
            .header("authorization", "Bearer sk-wrong")
            .body("{}")
            .send()
            .await
            .unwrap(),
        bridge.http.get(bridge.url("/usage")).send().await.unwrap(),
        bridge.http.post(bridge.url("/refresh")).send().await.unwrap(),
    ] {
        assert_eq!(res.status(), 401);
        assert_eq!(res.headers()[header::WWW_AUTHENTICATE], "Bearer");
        assert_eq!(json_body(res).await["error"]["type"], "bridge_error");
    }
    assert_eq!(bridge.upstream.count(), 0);
    assert_eq!(bridge.usage.count(), 0);
    assert_eq!(bridge.token_endpoint.count(), 0);
}

#[tokio::test]
async fn a_valid_key_passes_through_but_is_not_forwarded_upstream() {
    let bridge = Bridge::guarded(TempFile::json(&fresh_auth()), keys(), ok_text("ok")).await;

    for (i, request) in [
        bridge
            .http
            .post(bridge.url("/responses"))
            .header("authorization", format!("Bearer {KEY}")),
        // Either rotation-era key works, through either header.
        bridge
            .http
            .post(bridge.url("/responses"))
            .header("x-api-key", "sk-bridge-rotating"),
    ]
    .into_iter()
    .enumerate()
    {
        let res = request.body("{}").send().await.unwrap();
        assert_eq!(res.status(), 200);
        let sent = &bridge.upstream.requests()[i];
        assert_eq!(
            sent.header("authorization"),
            Some(format!("Bearer {}", bridge.current_access_token()).as_str())
        );
        assert!(sent.header("x-api-key").is_none());
    }
    assert_eq!(bridge.upstream.count(), 2);
}

#[tokio::test]
async fn health_stays_open_but_hides_the_account_until_authenticated() {
    let bridge = Bridge::guarded(TempFile::json(&fresh_auth()), keys(), ok_text("ok")).await;

    let res = bridge.http.get(bridge.url("/health")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let body = json_body(res).await;
    assert_eq!(body, json!({ "ok": true }));

    let res = bridge
        .http
        .get(bridge.url("/health"))
        .header("authorization", format!("Bearer {KEY}"))
        .send()
        .await
        .unwrap();
    let body = json_body(res).await;
    assert_eq!(body["email"], EMAIL);
    assert_eq!(body["token"]["has_refresh_token"], true);
}

#[tokio::test]
async fn health_without_credentials_is_503_for_anonymous_callers_too() {
    let bridge = Bridge::guarded(TempFile::missing(), keys(), ok_text("ok")).await;
    let res = bridge.http.get(bridge.url("/health")).send().await.unwrap();
    assert_eq!(res.status(), 503);
    let body = json_body(res).await;
    assert_eq!(body["error"]["type"], "bridge_error");
    // The credentials path is an implementation detail, not for strangers.
    assert_eq!(body["error"]["message"], "credentials unavailable");
}

#[tokio::test]
async fn refresh_works_with_a_key() {
    let bridge = Bridge::guarded(TempFile::json(&fresh_auth()), keys(), ok_text("ok")).await;
    let res = bridge
        .http
        .post(bridge.url("/refresh"))
        .header("authorization", format!("Bearer {KEY}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(json_body(res).await["ok"], true);
    assert_eq!(bridge.token_endpoint.count(), 1);
}
