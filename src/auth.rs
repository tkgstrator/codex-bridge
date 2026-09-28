//! Codex (ChatGPT subscription) OAuth token management.
//!
//! Reads the credentials the official Codex CLI writes to
//! `~/.codex/auth.json`, refreshes the access token against
//! https://auth.openai.com/oauth/token when it is at or near expiry, and
//! writes the rotated grant back to the same file so the CLI and this
//! proxy never hold diverging refresh tokens.
//!
//! The refresh_token rotates on every use, so every read/refresh goes
//! through one mutex: two concurrent callers can never both decide to
//! spend the same single-use token.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

use crate::Error;

// Same client_id the official Codex CLI uses for `codex login`.
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEFAULT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_AUTH_CLAIM: &str = "https://api.openai.com/auth";

// `codex login --device-auth`'s API, captured from the openai/codex CLI
// binary and cross-checked against a working implementation. Not part
// of the public OAuth surface — `/deviceauth/usercode` (no `/api/accounts`
// prefix) is a Cloudflare-challenge-protected browser route and is NOT
// this; these `/api/accounts/...` paths are the plain machine API the
// CLI itself calls, verified reachable without solving a challenge.
const DEFAULT_DEVICE_USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEFAULT_DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
// Shown to the human; never itself called by this client.
const DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
// Only has to match what the token exchange sends back, never be
// reachable — the same RFC 6749 loopback-redirect rule applies.
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEFAULT_DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);

// Refresh this far ahead of `exp` so a long request started right at
// the threshold doesn't 401 mid-flight.
const REFRESH_LEEWAY: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug)]
pub struct Credentials {
    pub access_token: String,
    pub account_id: Option<String>,
}

/// Snapshot of the stored grant for `/health`. Nothing here is a secret:
/// identity claims and timestamps only, never a token.
#[derive(Debug)]
pub struct TokenStatus {
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub expires_at: Option<SystemTime>,
    pub last_refresh: Option<String>,
    pub has_refresh_token: bool,
}

pub struct TokenStore {
    path: PathBuf,
    token_url: String,
    device_usercode_url: String,
    device_token_url: String,
    client: reqwest::Client,
    lock: Mutex<()>,
}

/// A freshly issued device-code grant, ready to show to a human.
#[derive(Debug)]
pub struct DeviceCode {
    pub device_auth_id: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: Duration,
}

/// Result of one poll of an outstanding device-code flow.
pub enum DevicePoll {
    /// Not yet approved by the human.
    Pending,
    /// Approved; exchange this for tokens via `install_from_device_code`.
    Authorized { code: String, code_verifier: String },
}

pub fn default_auth_path() -> PathBuf {
    if let Ok(p) = std::env::var("CODEX_AUTH_PATH") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".codex").join("auth.json")
}

fn decode_jwt_payload(token: &str) -> Option<Map<String, Value>> {
    let segment = token.split('.').nth(1)?.trim_end_matches('=');
    let bytes = URL_SAFE_NO_PAD.decode(segment).ok()?;
    match serde_json::from_slice(&bytes).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

fn access_token_expiry(token: &str) -> Option<SystemTime> {
    let exp = decode_jwt_payload(token)?.get("exp")?.as_f64()?;
    Some(UNIX_EPOCH + Duration::from_secs_f64(exp))
}

// `chatgpt_account_id` lives under the namespaced auth claim of both
// the id_token and the access_token. auth.json carries it explicitly as
// `tokens.account_id`; fall back to the claim when it is missing.
fn account_id_from_claims(token: &str) -> Option<String> {
    let claims = decode_jwt_payload(token)?;
    let id = claims
        .get(OPENAI_AUTH_CLAIM)?
        .get("chatgpt_account_id")?
        .as_str()?;
    (!id.is_empty()).then(|| id.to_string())
}

// Identity fields the id_token carries alongside `chatgpt_account_id`.
fn identity_from_claims(token: &str) -> (Option<String>, Option<String>) {
    let Some(claims) = decode_jwt_payload(token) else {
        return (None, None);
    };
    let email = str_field(&claims, "email").map(str::to_string);
    let plan = claims
        .get(OPENAI_AUTH_CLAIM)
        .and_then(|a| a.get("chatgpt_plan_type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    (email, plan)
}

fn needs_refresh(token: &str) -> bool {
    match access_token_expiry(token) {
        // Without an `exp` claim we cannot tell; let the upstream 401 decide.
        None => false,
        Some(exp) => exp.duration_since(SystemTime::now()).unwrap_or_default() <= REFRESH_LEEWAY,
    }
}

fn str_field<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn credentials_of(tokens: &Map<String, Value>) -> Credentials {
    let access_token = str_field(tokens, "access_token").unwrap_or_default().to_string();
    let account_id = str_field(tokens, "account_id").map(str::to_string).or_else(|| {
        str_field(tokens, "id_token")
            .and_then(account_id_from_claims)
            .or_else(|| account_id_from_claims(&access_token))
    });
    Credentials {
        access_token,
        account_id,
    }
}

impl TokenStore {
    pub fn new(path: PathBuf, client: reqwest::Client) -> Self {
        let token_url = std::env::var("CODEX_TOKEN_URL").unwrap_or_else(|_| DEFAULT_TOKEN_URL.into());
        let device_usercode_url =
            std::env::var("CODEX_DEVICE_USERCODE_URL").unwrap_or_else(|_| DEFAULT_DEVICE_USERCODE_URL.into());
        let device_token_url =
            std::env::var("CODEX_DEVICE_TOKEN_URL").unwrap_or_else(|_| DEFAULT_DEVICE_TOKEN_URL.into());
        Self::with_urls(path, token_url, device_usercode_url, device_token_url, client)
    }

    pub fn with_token_url(path: PathBuf, token_url: String, client: reqwest::Client) -> Self {
        Self::with_urls(
            path,
            token_url,
            DEFAULT_DEVICE_USERCODE_URL.into(),
            DEFAULT_DEVICE_TOKEN_URL.into(),
            client,
        )
    }

    pub fn with_urls(
        path: PathBuf,
        token_url: String,
        device_usercode_url: String,
        device_token_url: String,
        client: reqwest::Client,
    ) -> Self {
        Self {
            path,
            token_url,
            device_usercode_url,
            device_token_url,
            client,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    // Shape of ~/.codex/auth.json. Kept as a generic Value so unknown
    // top-level keys survive a rewrite untouched.
    async fn read_file(&self) -> Result<Map<String, Value>, Error> {
        let raw = tokio::fs::read(&self.path).await.map_err(|e| {
            format!(
                "cannot read credentials file {} ({e}); run `codex login` first",
                self.path.display()
            )
        })?;
        let value: Value = serde_json::from_slice(&raw)
            .map_err(|e| format!("invalid credentials file {}: {e}", self.path.display()))?;
        match value {
            Value::Object(m)
                if m.get("tokens")
                    .and_then(|t| t.get("access_token"))
                    .and_then(Value::as_str)
                    .is_some() =>
            {
                Ok(m)
            }
            _ => Err(format!(
                "invalid credentials file {}: missing tokens.access_token",
                self.path.display()
            )
            .into()),
        }
    }

    async fn write_file(&self, auth: &Map<String, Value>) -> Result<(), Error> {
        let mut out = serde_json::to_vec_pretty(auth)?;
        out.push(b'\n');
        tokio::fs::write(&self.path, out).await?;
        Ok(())
    }

    async fn refresh(&self, refresh_token: &str) -> Result<Map<String, Value>, Error> {
        let res = self
            .client
            .post(&self.token_url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", CODEX_CLIENT_ID),
            ])
            .send()
            .await?;
        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            return Err(format!("token refresh failed: {status} {body}").trim().into());
        }
        match serde_json::from_slice::<Value>(&res.bytes().await?) {
            Ok(Value::Object(m)) if str_field(&m, "access_token").is_some() => Ok(m),
            _ => Err("token refresh returned an unexpected payload".into()),
        }
    }

    async fn rotate(&self, mut auth: Map<String, Value>) -> Result<Credentials, Error> {
        let tokens = auth
            .get("tokens")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let current = str_field(&tokens, "refresh_token")
            .ok_or("no refresh_token in credentials file; run `codex login` again")?
            .to_string();

        let rotated = self.refresh(&current).await?;
        let mut next = tokens.clone();
        for key in ["access_token", "refresh_token", "id_token"] {
            if let Some(v) = str_field(&rotated, key) {
                next.insert(key.into(), json!(v));
            }
        }
        if str_field(&next, "account_id").is_none() {
            let from = str_field(&rotated, "id_token").or_else(|| str_field(&rotated, "access_token"));
            if let Some(id) = from.and_then(account_id_from_claims) {
                next.insert("account_id".into(), json!(id));
            }
        }
        auth.insert("tokens".into(), Value::Object(next.clone()));
        auth.insert("last_refresh".into(), json!(iso_now()));
        self.write_file(&auth).await?;
        println!("[auth] access token refreshed, saved to {}", self.path.display());
        Ok(credentials_of(&next))
    }

    /// Read-only view of the stored grant; never triggers a refresh.
    pub async fn status(&self) -> Result<TokenStatus, Error> {
        let _guard = self.lock.lock().await;
        let auth = self.read_file().await?;
        let tokens = auth
            .get("tokens")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let creds = credentials_of(&tokens);
        let (email, plan_type) = str_field(&tokens, "id_token")
            .map(identity_from_claims)
            .unwrap_or_default();
        Ok(TokenStatus {
            account_id: creds.account_id,
            email,
            plan_type,
            expires_at: access_token_expiry(&creds.access_token),
            last_refresh: str_field(&auth, "last_refresh").map(str::to_string),
            has_refresh_token: str_field(&tokens, "refresh_token").is_some(),
        })
    }

    /// Rotate unconditionally (manual `/refresh`).
    pub async fn refresh_now(&self) -> Result<Credentials, Error> {
        let _guard = self.lock.lock().await;
        let auth = self.read_file().await?;
        self.rotate(auth).await
    }

    /// Return a usable access token, rotating it first when it is near
    /// expiry. Pass the token that just got a 401 as `stale` to force a
    /// rotation of exactly that token — if the file no longer holds it,
    /// someone else already rotated and we simply return the new one.
    pub async fn get(&self, stale: Option<&str>) -> Result<Credentials, Error> {
        let _guard = self.lock.lock().await;
        let auth = self.read_file().await?;
        let tokens = auth
            .get("tokens")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let access_token = str_field(&tokens, "access_token").unwrap_or_default();
        let rejected = stale.is_some_and(|s| s == access_token);
        if !rejected && !needs_refresh(access_token) {
            return Ok(credentials_of(&tokens));
        }
        self.rotate(auth).await
    }

    /// Start a `codex login --device-auth`-equivalent sign-in: ask
    /// auth.openai.com for a one-time code to show a human. Does not
    /// touch the credentials file — nothing is written until
    /// `install_from_device_code` succeeds.
    pub async fn request_device_code(&self) -> Result<DeviceCode, Error> {
        let res = self
            .client
            .post(&self.device_usercode_url)
            .header("content-type", "application/json")
            .json(&json!({ "client_id": CODEX_CLIENT_ID }))
            .send()
            .await?;
        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            return Err(format!("device-code request failed: {status} {body}").trim().into());
        }
        let Value::Object(body) = serde_json::from_slice::<Value>(&res.bytes().await?)? else {
            return Err("device-code request returned an unexpected payload".into());
        };
        let device_auth_id = str_field(&body, "device_auth_id")
            .ok_or("device-code response carried no device_auth_id")?
            .to_string();
        // The struct this mirrors accepts either key; only one is ever
        // actually sent, but which one is not documented.
        let user_code = str_field(&body, "user_code")
            .or_else(|| str_field(&body, "usercode"))
            .ok_or("device-code response carried neither user_code nor usercode")?
            .to_string();
        // `interval` arrives as a numeric string in practice, but parse
        // permissively: fall back to the default rather than fail a
        // login over a formatting quirk in a field that is a pacing hint.
        let interval = body
            .get("interval")
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
            .map(Duration::from_secs)
            .filter(|d| !d.is_zero())
            .unwrap_or(DEFAULT_DEVICE_POLL_INTERVAL);
        Ok(DeviceCode {
            device_auth_id,
            user_code,
            verification_uri: DEVICE_VERIFICATION_URL.into(),
            interval,
        })
    }

    /// One poll of an outstanding device-code flow. The caller owns the
    /// interval / TTL loop (see `device_flow.rs`).
    pub async fn poll_device_code(&self, device_auth_id: &str, user_code: &str) -> Result<DevicePoll, Error> {
        let res = self
            .client
            .post(&self.device_token_url)
            .header("content-type", "application/json")
            .json(&json!({ "device_auth_id": device_auth_id, "user_code": user_code }))
            .send()
            .await?;
        // 403/404 both mean "not approved yet" — codex-rs treats them
        // identically rather than as an error.
        if res.status() == reqwest::StatusCode::FORBIDDEN || res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(DevicePoll::Pending);
        }
        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            return Err(format!("device-code poll failed: {status} {body}").trim().into());
        }
        let Value::Object(body) = serde_json::from_slice::<Value>(&res.bytes().await?)? else {
            return Err("device-code poll returned an unexpected payload".into());
        };
        let code = str_field(&body, "authorization_code")
            .ok_or("device-code poll response carried no authorization_code")?
            .to_string();
        let code_verifier = str_field(&body, "code_verifier")
            .ok_or("device-code poll response carried no code_verifier")?
            .to_string();
        Ok(DevicePoll::Authorized { code, code_verifier })
    }

    /// Redeem an authorized device-code grant for tokens and write them
    /// to the credentials file — creating it if this is a first-ever
    /// sign-in. Unlike `rotate`, an unreadable or missing file is not an
    /// error here: it just means there is nothing to preserve.
    pub async fn install_from_device_code(&self, code: &str, code_verifier: &str) -> Result<Credentials, Error> {
        let _guard = self.lock.lock().await;
        let res = self
            .client
            .post(&self.token_url)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", DEVICE_REDIRECT_URI),
                ("client_id", CODEX_CLIENT_ID),
                ("code_verifier", code_verifier),
            ])
            .send()
            .await?;
        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            return Err(format!("device-code token exchange failed: {status} {body}").trim().into());
        }
        let exchanged = match serde_json::from_slice::<Value>(&res.bytes().await?) {
            Ok(Value::Object(m)) if str_field(&m, "access_token").is_some() => m,
            _ => return Err("device-code token exchange returned an unexpected payload".into()),
        };

        let mut auth = self.read_file().await.unwrap_or_default();
        let mut tokens = auth
            .get("tokens")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for key in ["access_token", "refresh_token", "id_token"] {
            if let Some(v) = str_field(&exchanged, key) {
                tokens.insert(key.into(), json!(v));
            }
        }
        let from = str_field(&tokens, "id_token").or_else(|| str_field(&tokens, "access_token"));
        match from.and_then(account_id_from_claims) {
            Some(id) => {
                tokens.insert("account_id".into(), json!(id));
            }
            None => {
                tokens.remove("account_id");
            }
        }
        auth.insert("tokens".into(), Value::Object(tokens.clone()));
        auth.insert("last_refresh".into(), json!(iso_now()));
        self.write_file(&auth).await?;
        println!(
            "[auth] signed in via device code, saved to {}",
            self.path.display()
        );
        Ok(credentials_of(&tokens))
    }
}

// RFC 3339 UTC timestamp without pulling in a datetime crate; matches
// the `last_refresh` field the Codex CLI itself writes.
fn iso_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    iso_from_secs(secs)
}

fn iso_from_secs(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
#[path = "__tests__/auth.rs"]
mod tests;
