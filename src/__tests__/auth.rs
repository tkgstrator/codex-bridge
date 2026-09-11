use std::sync::Arc;

use super::*;
use crate::support::*;
use axum::http::StatusCode;

fn store(file: &TempFile, token_url: &str) -> TokenStore {
    TokenStore::with_token_url(file.path.clone(), token_url.into(), reqwest::Client::new())
}

/// A store whose token endpoint is unreachable: any refresh attempt fails loudly.
fn offline_store(file: &TempFile) -> TokenStore {
    store(file, "http://127.0.0.1:1/oauth/token")
}

fn tokens_of(auth: &Value) -> Map<String, Value> {
    auth["tokens"].as_object().cloned().unwrap()
}

fn err_string(err: Error) -> String {
    err.to_string()
}

// --- pure helpers -------------------------------------------------------

#[test]
fn decodes_jwt_payload_claims() {
    let token = jwt(json!({ "exp": 42, "email": EMAIL }));
    let claims = decode_jwt_payload(&token).unwrap();
    assert_eq!(claims["exp"], 42);
    assert_eq!(claims["email"], EMAIL);
}

#[test]
fn rejects_malformed_jwts() {
    assert!(decode_jwt_payload("opaque-token").is_none());
    assert!(decode_jwt_payload("a.!!!.c").is_none());
    // Valid base64 but not a JSON object.
    let array = format!("h.{}.s", URL_SAFE_NO_PAD.encode(b"[1,2]"));
    assert!(decode_jwt_payload(&array).is_none());
}

#[test]
fn reads_expiry_from_exp_claim() {
    let exp = access_token_expiry(&access_token(3600)).unwrap();
    let secs = exp.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    assert!((secs - (now_secs() + 3600)).abs() <= 1);
    assert!(access_token_expiry("opaque").is_none());
    assert!(access_token_expiry(&jwt(json!({ "sub": "x" }))).is_none());
}

#[test]
fn refreshes_only_inside_the_leeway_window() {
    assert!(!needs_refresh(&access_token(3600)));
    assert!(needs_refresh(&access_token(60)));
    assert!(needs_refresh(&access_token(-10)));
    // No `exp` at all: leave it to the upstream 401.
    assert!(!needs_refresh("opaque"));
}

#[test]
fn account_id_prefers_explicit_field_then_id_token_then_access_token() {
    let explicit = tokens_of(&auth_json(json!({
        "access_token": access_token_with_account(3600, "from-access"),
        "id_token": id_token("from-id"),
        "account_id": "explicit",
    })));
    assert_eq!(credentials_of(&explicit).account_id.as_deref(), Some("explicit"));

    let from_id = tokens_of(&auth_json(json!({
        "access_token": access_token_with_account(3600, "from-access"),
        "id_token": id_token("from-id"),
    })));
    assert_eq!(credentials_of(&from_id).account_id.as_deref(), Some("from-id"));

    let from_access = tokens_of(&auth_json(json!({
        "access_token": access_token_with_account(3600, "from-access"),
    })));
    assert_eq!(
        credentials_of(&from_access).account_id.as_deref(),
        Some("from-access")
    );

    let none = tokens_of(&auth_json(
        json!({ "access_token": access_token(3600), "account_id": "" }),
    ));
    assert_eq!(credentials_of(&none).account_id, None);
}

#[test]
fn identity_comes_from_id_token_claims() {
    assert_eq!(
        identity_from_claims(&id_token(ACCOUNT_ID)),
        (Some(EMAIL.to_string()), Some(PLAN.to_string()))
    );
    assert_eq!(identity_from_claims("opaque"), (None, None));
}

#[test]
fn formats_rfc3339_utc_timestamps() {
    assert_eq!(iso_from_secs(0), "1970-01-01T00:00:00Z");
    assert_eq!(iso_from_secs(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(iso_from_secs(1_700_000_000), "2023-11-14T22:13:20Z");
    assert_eq!(iso_from_secs(-1), "1969-12-31T23:59:59Z");
    assert_eq!(iso_now().len(), 20);
}

#[test]
fn default_path_honours_env_override() {
    let auth_path = std::env::var("CODEX_AUTH_PATH").ok();
    let expected = auth_path.map_or_else(
        || PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".codex/auth.json"),
        PathBuf::from,
    );
    assert_eq!(default_auth_path(), expected);
}

// --- credential file ----------------------------------------------------

#[tokio::test]
async fn missing_file_points_at_codex_login() {
    let file = TempFile::missing();
    let err = err_string(offline_store(&file).get(None).await.unwrap_err());
    assert!(err.contains("cannot read credentials file"), "{err}");
    assert!(err.contains("run `codex login` first"), "{err}");
}

#[tokio::test]
async fn rejects_invalid_json_and_missing_access_token() {
    let file = TempFile::missing();
    file.write(b"{ not json");
    let err = err_string(offline_store(&file).get(None).await.unwrap_err());
    assert!(err.contains("invalid credentials file"), "{err}");

    file.write(br#"{ "tokens": { "refresh_token": "rt-1" } }"#);
    let err = err_string(offline_store(&file).get(None).await.unwrap_err());
    assert!(err.contains("missing tokens.access_token"), "{err}");

    file.write(b"[]");
    let err = err_string(offline_store(&file).get(None).await.unwrap_err());
    assert!(err.contains("missing tokens.access_token"), "{err}");
}

#[tokio::test]
async fn fresh_token_is_returned_without_touching_the_endpoint() {
    let file = TempFile::json(&fresh_auth());
    let endpoint = MockServer::token_endpoint("rt-2").await;
    let store = store(&file, &endpoint.url("/oauth/token"));

    let creds = store.get(None).await.unwrap();
    assert_eq!(creds.access_token, file.read()["tokens"]["access_token"]);
    assert_eq!(creds.account_id.as_deref(), Some(ACCOUNT_ID));
    assert_eq!(endpoint.count(), 0);
    assert!(file.read().get("last_refresh").is_none());
}

#[tokio::test]
async fn expired_token_is_rotated_and_written_back() {
    let file = TempFile::json(&auth_json(json!({
        "access_token": access_token(-1),
        "refresh_token": "rt-1",
        "custom_key": "kept",
    })));
    let endpoint = MockServer::token_endpoint("rt-2").await;
    let store = store(&file, &endpoint.url("/oauth/token"));

    let creds = store.get(None).await.unwrap();

    let sent = &endpoint.requests()[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.uri, "/oauth/token");
    assert_eq!(sent.form("grant_type"), Some("refresh_token"));
    assert_eq!(sent.form("refresh_token"), Some("rt-1"));
    assert_eq!(sent.form("client_id"), Some(CODEX_CLIENT_ID));

    let saved = file.read();
    let tokens = &saved["tokens"];
    assert_eq!(tokens["access_token"], creds.access_token);
    assert_eq!(tokens["refresh_token"], "rt-2");
    assert_eq!(tokens["id_token"], id_token(ACCOUNT_ID));
    // Filled in from the id_token claim because the file lacked it.
    assert_eq!(tokens["account_id"], ACCOUNT_ID);
    assert_eq!(creds.account_id.as_deref(), Some(ACCOUNT_ID));
    // Unknown keys survive the rewrite; last_refresh is stamped.
    assert_eq!(tokens["custom_key"], "kept");
    assert_eq!(saved["OPENAI_API_KEY"], Value::Null);
    assert_eq!(saved["last_refresh"].as_str().unwrap().len(), 20);
    assert!(!needs_refresh(&creds.access_token));
}

#[tokio::test]
async fn stale_token_forces_rotation_even_when_not_expired() {
    let file = TempFile::json(&fresh_auth());
    let endpoint = MockServer::token_endpoint("rt-2").await;
    let store = store(&file, &endpoint.url("/oauth/token"));
    let current = store.get(None).await.unwrap().access_token;

    let creds = store.get(Some(&current)).await.unwrap();
    assert_ne!(creds.access_token, current);
    assert_eq!(endpoint.count(), 1);
    assert_eq!(file.read()["tokens"]["refresh_token"], "rt-2");
}

#[tokio::test]
async fn stale_token_already_rotated_by_someone_else_is_ignored() {
    let file = TempFile::json(&fresh_auth());
    let endpoint = MockServer::token_endpoint("rt-2").await;
    let store = store(&file, &endpoint.url("/oauth/token"));

    let creds = store.get(Some("some-older-token")).await.unwrap();
    assert_eq!(creds.access_token, file.read()["tokens"]["access_token"]);
    assert_eq!(endpoint.count(), 0);
}

#[tokio::test]
async fn concurrent_callers_spend_the_refresh_token_once() {
    let file = TempFile::json(&auth_json(json!({
        "access_token": access_token(-1),
        "refresh_token": "rt-1",
    })));
    let endpoint = MockServer::token_endpoint("rt-2").await;
    let store = Arc::new(store(&file, &endpoint.url("/oauth/token")));

    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move { store.get(None).await.unwrap().access_token })
        })
        .collect();
    let mut tokens = Vec::new();
    for task in tasks {
        tokens.push(task.await.unwrap());
    }

    assert_eq!(endpoint.count(), 1);
    assert!(tokens.iter().all(|t| t == &tokens[0]));
}

#[tokio::test]
async fn refresh_failure_surfaces_status_and_body() {
    let file = TempFile::json(&auth_json(json!({
        "access_token": access_token(-1),
        "refresh_token": "rt-dead",
    })));
    let endpoint =
        MockServer::start(|_, _| json_response(StatusCode::BAD_REQUEST, json!({ "error": "invalid_grant" })))
            .await;
    let store = store(&file, &endpoint.url("/oauth/token"));

    let err = err_string(store.get(None).await.unwrap_err());
    assert!(err.starts_with("token refresh failed: 400"), "{err}");
    assert!(err.contains("invalid_grant"), "{err}");
    // A failed rotation must not clobber the file.
    assert_eq!(file.read()["tokens"]["refresh_token"], "rt-dead");
}

#[tokio::test]
async fn refresh_with_unexpected_payload_fails() {
    let file = TempFile::json(&auth_json(json!({
        "access_token": access_token(-1),
        "refresh_token": "rt-1",
    })));
    let endpoint = MockServer::start(|_, _| json_response(StatusCode::OK, json!({ "hello": "world" }))).await;
    let store = store(&file, &endpoint.url("/oauth/token"));

    let err = err_string(store.get(None).await.unwrap_err());
    assert_eq!(err, "token refresh returned an unexpected payload");
}

#[tokio::test]
async fn refresh_without_refresh_token_fails_before_any_request() {
    let file = TempFile::json(&auth_json(json!({ "access_token": access_token(-1) })));
    let endpoint = MockServer::token_endpoint("rt-2").await;
    let store = store(&file, &endpoint.url("/oauth/token"));

    let err = err_string(store.get(None).await.unwrap_err());
    assert!(err.contains("no refresh_token in credentials file"), "{err}");
    assert_eq!(endpoint.count(), 0);
}

#[tokio::test]
async fn refresh_now_rotates_unconditionally() {
    let file = TempFile::json(&fresh_auth());
    let endpoint = MockServer::token_endpoint("rt-2").await;
    let store = store(&file, &endpoint.url("/oauth/token"));
    let before = file.read()["tokens"]["access_token"].clone();

    let creds = store.refresh_now().await.unwrap();
    assert_ne!(Value::from(creds.access_token), before);
    assert_eq!(endpoint.count(), 1);
    assert_eq!(file.read()["tokens"]["refresh_token"], "rt-2");
}

#[tokio::test]
async fn status_reports_identity_without_refreshing() {
    let mut auth = fresh_auth();
    auth["last_refresh"] = json!("2026-01-01T00:00:00Z");
    let file = TempFile::json(&auth);
    let endpoint = MockServer::token_endpoint("rt-2").await;
    let store = store(&file, &endpoint.url("/oauth/token"));

    let status = store.status().await.unwrap();
    assert_eq!(status.account_id.as_deref(), Some(ACCOUNT_ID));
    assert_eq!(status.email.as_deref(), Some(EMAIL));
    assert_eq!(status.plan_type.as_deref(), Some(PLAN));
    assert_eq!(status.last_refresh.as_deref(), Some("2026-01-01T00:00:00Z"));
    assert!(status.has_refresh_token);
    let expires_in = status
        .expires_at
        .unwrap()
        .duration_since(SystemTime::now())
        .unwrap()
        .as_secs();
    assert!((3590..=3600).contains(&expires_in), "{expires_in}");
    assert_eq!(endpoint.count(), 0);
}

#[tokio::test]
async fn status_of_a_bare_grant_has_no_identity() {
    let file = TempFile::json(&auth_json(json!({ "access_token": "opaque" })));
    let status = offline_store(&file).status().await.unwrap();
    assert_eq!(status.account_id, None);
    assert_eq!(status.email, None);
    assert_eq!(status.plan_type, None);
    assert_eq!(status.expires_at, None);
    assert_eq!(status.last_refresh, None);
    assert!(!status.has_refresh_token);
}
