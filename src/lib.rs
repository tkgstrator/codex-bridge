//! Shared pieces of codex-bridge: OAuth token management and small
//! helpers used by both the HTTP proxy binary (`codex-bridge`) and the
//! MCP server binary (`codex-mcp`).

pub mod auth;

// Shared with the `codex-bridge` binary's own test module. auth.rs's
// tests only exercise a subset of these fixtures, so allow the rest to
// go unused here rather than splitting the file in two.
#[cfg(test)]
#[path = "__tests__/support.rs"]
#[allow(dead_code)]
mod support;

use axum::http::{header, HeaderMap};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.into())
}

// --- caller-side API key authentication ---------------------------------
//
// Shared by codex-bridge (BRIDGE_API_KEY) and codex-mcp (MCP_API_KEY).
// Opt-in: with the variable unset, empty keys means "unauthenticated",
// so the binary must stay behind a firewall. Comma-separated values let
// a key be rotated without downtime: publish the new one, drop the old
// one once clients moved.
pub fn api_keys_from_env(var: &str) -> Vec<String> {
    env_or(var, "")
        .split(',')
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .collect()
}

// Compare without an early exit, so a wrong key cannot be guessed byte
// by byte from the response time. The lengths are not hidden, but the
// presented one is the attacker's own input anyway.
pub fn secret_eq(a: &str, b: &str) -> bool {
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
pub fn presented_key(headers: &HeaderMap) -> Option<&str> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .and_then(|(scheme, token)| scheme.eq_ignore_ascii_case("bearer").then_some(token.trim()));
    bearer.or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_keys_are_split_on_commas_and_trimmed() {
        std::env::set_var("CODEX_BRIDGE_TEST_API_KEY", " sk-one , sk-two ,, ");
        assert_eq!(
            api_keys_from_env("CODEX_BRIDGE_TEST_API_KEY"),
            vec!["sk-one".to_string(), "sk-two".into()]
        );
        std::env::set_var("CODEX_BRIDGE_TEST_API_KEY", "");
        assert!(api_keys_from_env("CODEX_BRIDGE_TEST_API_KEY").is_empty());
        std::env::remove_var("CODEX_BRIDGE_TEST_API_KEY");
        assert!(api_keys_from_env("CODEX_BRIDGE_TEST_API_KEY").is_empty());
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
}
