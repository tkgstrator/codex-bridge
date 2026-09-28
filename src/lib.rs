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

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.into())
}
