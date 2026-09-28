//! codex-mcp — an MCP (stdio) server that lets an MCP client (e.g. Claude
//! Code) call the ChatGPT Codex backend directly, using the same
//! `~/.codex/auth.json` OAuth grant as codex-bridge and the official
//! Codex CLI. Unlike codex-bridge it talks to CODEX_UPSTREAM itself; no
//! separate HTTP proxy process needs to be running.
//!
//! Three tools are exposed:
//! - `ask_codex`: send a prompt to the Responses API and return the
//!   model's text answer (the upstream requires `stream: true` /
//!   `store: false`, so the SSE response is buffered and the final
//!   `response.completed` event's output text is extracted).
//! - `generate_image`: send a prompt to the image generation endpoint
//!   and return the resulting image.
//! - `list_models`: passthrough of `GET /models`, unmodified.

use std::sync::Arc;

use codex_bridge::auth::{self, TokenStore};
use codex_bridge::{env_or, Error};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{tool, tool_router, ErrorData as McpError, ServiceExt};
use serde_json::{json, Map, Value};

struct UpstreamCtx {
    client: reqwest::Client,
    upstream: String,
    user_agent: String,
    tokens: TokenStore,
    default_model: String,
    cli_version: String,
}

impl UpstreamCtx {
    fn from_env() -> Self {
        let upstream = env_or("CODEX_UPSTREAM", "https://chatgpt.com/backend-api/codex")
            .trim_end_matches('/')
            .to_string();
        let cli_version = env_or("CODEX_CLI_VERSION", "0.0.0");
        let user_agent = format!(
            "codex_cli/{cli_version} ({} {}; {})",
            std::env::consts::OS,
            std::env::consts::FAMILY,
            std::env::consts::ARCH
        );
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build the HTTP client");
        let tokens = TokenStore::new(auth::default_auth_path(), client.clone());
        Self {
            client,
            upstream,
            user_agent,
            tokens,
            default_model: env_or("CODEX_MCP_DEFAULT_MODEL", "gpt-5.6-luna"),
            cli_version,
        }
    }
}

// One request with the headers the backend uses to classify traffic as
// the Codex CLI (mirrors main.rs's `upstream_headers`, minus anything
// that only makes sense when relaying an inbound HTTP request).
async fn send(
    ctx: &UpstreamCtx,
    method: reqwest::Method,
    path: &str,
    query: &[(&str, &str)],
    body: Option<&Value>,
    accept_sse: bool,
    stale: Option<&str>,
) -> Result<(reqwest::Response, String), Error> {
    let creds = ctx.tokens.get(stale).await?;
    let mut req = ctx
        .client
        .request(method, format!("{}{path}", ctx.upstream))
        .query(query)
        .bearer_auth(&creds.access_token)
        .header("originator", "codex_cli")
        .header("user-agent", &ctx.user_agent)
        .header("session_id", uuid::Uuid::new_v4().to_string());
    if let Some(id) = &creds.account_id {
        req = req.header("chatgpt-account-id", id);
    }
    if accept_sse {
        req = req.header("accept", "text/event-stream");
    }
    if let Some(body) = body {
        req = req.json(body);
    }
    let res = req.send().await?;
    Ok((res, creds.access_token))
}

// Retries once, with a rotated token, on an upstream 401 — same pattern
// as main.rs's proxy handler.
async fn send_with_retry(
    ctx: &UpstreamCtx,
    method: reqwest::Method,
    path: &str,
    query: &[(&str, &str)],
    body: Option<&Value>,
    accept_sse: bool,
) -> Result<reqwest::Response, Error> {
    let (mut res, used) = send(ctx, method.clone(), path, query, body, accept_sse, None).await?;
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        (res, _) = send(ctx, method, path, query, body, accept_sse, Some(&used)).await?;
    }
    Ok(res)
}

// Per-model catalog entries carry a couple of fields — observed as
// `model_messages` and `base_instructions` — that are tens of KB of
// system prompt template meant for the Codex CLI itself, not for a
// caller deciding which model to pick. Rather than naming those fields
// (which drift between backend releases), drop any per-model field
// whose serialized value exceeds a size threshold, and drop models
// marked `visibility: "hide"` (internal/unreleased). This mirrors what
// other Codex-backed tools that hit this same endpoint keep: an id/
// capability-level view, not the raw prompt payloads.
const MAX_MODEL_FIELD_BYTES: usize = 4096;

fn trim_model_catalog(body: &str) -> Result<String, String> {
    let mut value: Value = serde_json::from_str(body).map_err(|err| err.to_string())?;
    let models = value
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .ok_or("upstream response had no \"models\" array")?;
    let mut trimmed = Vec::with_capacity(models.len());
    for model in models.drain(..) {
        let Value::Object(fields) = model else { continue };
        if fields.get("visibility").and_then(Value::as_str) == Some("hide") {
            continue;
        }
        let kept: Map<String, Value> = fields
            .into_iter()
            .filter(|(_, v)| {
                serde_json::to_string(v).map(|s| s.len()).unwrap_or(0) <= MAX_MODEL_FIELD_BYTES
            })
            .collect();
        trimmed.push(Value::Object(kept));
    }
    let mut out = Map::new();
    out.insert("models".into(), Value::Array(trimmed));
    serde_json::to_string(&out).map_err(|err| err.to_string())
}

fn upstream_error(status: reqwest::StatusCode, body: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "upstream returned {status}: {body}"
    ))])
}

// Pulls the final answer out of a buffered SSE response from
// `/responses`. Frames are separated by a blank line; each frame's
// `data:` line(s) carry one JSON event.
//
// The authoritative source is each `response.output_item.done` event's
// `item.content` (a completed message item's full text) — observed in
// practice to be reliable even when `response.completed`'s own
// `response.output` array comes back empty. `response.output_text.delta`
// is accumulated too and used as a fallback for responses that stream
// deltas without ever emitting `output_item.done`.
fn extract_output_text(sse_body: &str) -> Result<String, String> {
    let mut item_text = String::new();
    let mut delta_text = String::new();
    let mut incomplete_reason: Option<String> = None;
    let mut terminated = false;

    for frame in sse_body.split("\n\n") {
        let data: String = frame
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    delta_text.push_str(delta);
                }
            }
            "response.output_item.done" => {
                let item = event.get("item");
                if item.and_then(|i| i.get("type")).and_then(Value::as_str) != Some("message") {
                    continue;
                }
                for content in item
                    .and_then(|i| i.get("content"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if content.get("type").and_then(Value::as_str) == Some("output_text") {
                        if let Some(t) = content.get("text").and_then(Value::as_str) {
                            item_text.push_str(t);
                        }
                    }
                }
            }
            "response.completed" => {
                terminated = true;
                break;
            }
            "response.incomplete" => {
                terminated = true;
                incomplete_reason = event
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                break;
            }
            "response.failed" | "error" => {
                let message = event
                    .pointer("/response/error/message")
                    .or_else(|| event.pointer("/error/message"))
                    .and_then(Value::as_str)
                    .unwrap_or("the upstream reported a failure")
                    .to_string();
                return Err(message);
            }
            _ => continue,
        }
    }

    if !terminated {
        return Err("upstream stream ended without a response.completed event".into());
    }
    let text = if !item_text.is_empty() { item_text } else { delta_text };
    if text.is_empty() {
        return Err(format!(
            "response incomplete: {}",
            incomplete_reason.as_deref().unwrap_or("no output text")
        ));
    }
    Ok(text)
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct AskCodexParams {
    /// The question or instruction to send to the model.
    prompt: String,
    /// Model slug (e.g. "gpt-5.6-luna"). Defaults to CODEX_MCP_DEFAULT_MODEL.
    #[serde(default)]
    model: Option<String>,
    /// Optional system instructions for the model.
    #[serde(default)]
    instructions: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct GenerateImageParams {
    /// Description of the image to generate.
    prompt: String,
    /// Image size, e.g. "1024x1024". Omit to use the upstream default.
    #[serde(default)]
    size: Option<String>,
    /// "opaque" or "transparent". Omit to use the upstream default.
    #[serde(default)]
    background: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ListModelsParams {
    /// `client_version` query parameter the backend expects (matches the
    /// Codex CLI's own version string). Defaults to CODEX_CLI_VERSION.
    #[serde(default)]
    client_version: Option<String>,
}

#[derive(Clone)]
struct CodexMcp {
    ctx: Arc<UpstreamCtx>,
}

#[tool_router(server_handler)]
impl CodexMcp {
    #[tool(description = "Ask the ChatGPT Codex backend a question and return its text answer.")]
    async fn ask_codex(
        &self,
        Parameters(AskCodexParams {
            prompt,
            model,
            instructions,
        }): Parameters<AskCodexParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = Map::new();
        body.insert(
            "model".into(),
            json!(model.unwrap_or_else(|| self.ctx.default_model.clone())),
        );
        if let Some(instructions) = instructions {
            body.insert("instructions".into(), json!(instructions));
        }
        body.insert(
            "input".into(),
            json!([{ "role": "user", "content": [{ "type": "input_text", "text": prompt }] }]),
        );
        body.insert("store".into(), json!(false));
        body.insert("stream".into(), json!(true));

        let res = send_with_retry(
            &self.ctx,
            reqwest::Method::POST,
            "/responses",
            &[],
            Some(&Value::Object(body)),
            true,
        )
        .await
        .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        if !status.is_success() {
            return Ok(upstream_error(status, &text));
        }
        match extract_output_text(&text) {
            Ok(answer) => Ok(CallToolResult::success(vec![ContentBlock::text(answer)])),
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(
        description = "Generate an image from a text prompt via the Codex backend's image generation endpoint. There is only one backend model behind this endpoint (it accepts no model selection)."
    )]
    async fn generate_image(
        &self,
        Parameters(GenerateImageParams {
            prompt,
            size,
            background,
        }): Parameters<GenerateImageParams>,
    ) -> Result<CallToolResult, McpError> {
        // The upstream ignores `model` entirely — verified empirically: a
        // nonsense string, an empty string, and an omitted field all
        // succeed identically. There is exactly one image model behind
        // this endpoint, so we don't send a field that would suggest
        // otherwise.
        let mut body = Map::new();
        body.insert("prompt".into(), json!(prompt));
        if let Some(size) = size {
            body.insert("size".into(), json!(size));
        }
        if let Some(background) = background {
            body.insert("background".into(), json!(background));
        }

        let res = send_with_retry(
            &self.ctx,
            reqwest::Method::POST,
            "/images/generations",
            &[],
            Some(&Value::Object(body)),
            false,
        )
        .await
        .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        if !status.is_success() {
            return Ok(upstream_error(status, &text));
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|err| McpError::internal_error(format!("invalid JSON from upstream: {err}"), None))?;
        match value.pointer("/data/0/b64_json").and_then(Value::as_str) {
            Some(b64) => Ok(CallToolResult::success(vec![ContentBlock::image(
                b64,
                "image/png",
            )])),
            None => Ok(CallToolResult::error(vec![ContentBlock::text(
                "upstream response had no image data",
            )])),
        }
    }

    #[tool(
        description = "List the models available on the Codex backend for this account (id, display name, description, context window, supported reasoning efforts, etc.) — the huge per-model prompt template fields the raw upstream catalog carries are stripped out."
    )]
    async fn list_models(
        &self,
        Parameters(ListModelsParams { client_version }): Parameters<ListModelsParams>,
    ) -> Result<CallToolResult, McpError> {
        let version = client_version.unwrap_or_else(|| self.ctx.cli_version.clone());
        let res = send_with_retry(
            &self.ctx,
            reqwest::Method::GET,
            "/models",
            &[("client_version", &version)],
            None,
            false,
        )
        .await
        .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        if !status.is_success() {
            return Ok(upstream_error(status, &text));
        }
        match trim_model_catalog(&text) {
            Ok(trimmed) => Ok(CallToolResult::success(vec![ContentBlock::text(trimmed)])),
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "could not parse the upstream model catalog: {message}"
            ))])),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let server = CodexMcp {
        ctx: Arc::new(UpstreamCtx::from_env()),
    };
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The upstream backend has been observed to leave `response.completed`'s
    // own `response.output` array empty, so the fixtures below carry the
    // text through `response.output_item.done` instead — the shape actually
    // seen on the wire (see `extract_output_text`'s doc comment).
    #[test]
    fn extracts_text_from_a_completed_response() {
        let sse = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"pon\"}\n\n\
                    event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"pong\"}]}}\n\n\
                    event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n";
        assert_eq!(extract_output_text(sse), Ok("pong".to_string()));
    }

    #[test]
    fn concatenates_multiple_output_item_done_events() {
        let sse = "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"A\"}]}}\n\n\
                    data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"B\"}]}}\n\n\
                    data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n";
        assert_eq!(extract_output_text(sse), Ok("AB".to_string()));
    }

    #[test]
    fn falls_back_to_accumulated_deltas_when_no_output_item_done_arrives() {
        let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n\
                    data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n\
                    data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n";
        assert_eq!(extract_output_text(sse), Ok("Hello".to_string()));
    }

    #[test]
    fn surfaces_the_failure_message_from_a_failed_response() {
        let sse = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"rate limited\"}}}\n\n";
        assert_eq!(extract_output_text(sse), Err("rate limited".to_string()));
    }

    #[test]
    fn surfaces_the_incomplete_reason_when_there_is_no_text() {
        let sse = "data: {\"type\":\"response.incomplete\",\"response\":{\"output\":[],\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n";
        assert_eq!(
            extract_output_text(sse),
            Err("response incomplete: max_output_tokens".to_string())
        );
    }

    #[test]
    fn errors_when_no_completed_event_is_present() {
        let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n";
        assert!(extract_output_text(sse).is_err());
    }

    #[test]
    fn ignores_frames_without_a_recognised_type() {
        let sse = "data: {\"type\":\"response.created\"}\n\n\
                    data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}}\n\n\
                    data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n";
        assert_eq!(extract_output_text(sse), Ok("ok".to_string()));
    }

    #[test]
    fn trim_model_catalog_drops_huge_fields_and_hidden_models() {
        let huge = "x".repeat(MAX_MODEL_FIELD_BYTES + 1);
        let body = json!({
            "models": [
                { "slug": "gpt-6-astra", "display_name": "GPT-6-Astra", "model_messages": { "persistent_instructions": huge } },
                { "slug": "internal-eval", "visibility": "hide", "display_name": "hidden" },
            ]
        })
        .to_string();

        let trimmed = trim_model_catalog(&body).unwrap();
        let value: Value = serde_json::from_str(&trimmed).unwrap();
        let models = value["models"].as_array().unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["slug"], "gpt-6-astra");
        assert_eq!(models[0]["display_name"], "GPT-6-Astra");
        assert!(models[0].get("model_messages").is_none());
        assert!(trimmed.len() < MAX_MODEL_FIELD_BYTES);
    }

    #[test]
    fn trim_model_catalog_keeps_small_fields_untouched() {
        let body = json!({
            "models": [{ "slug": "gpt-5.5", "context_window": 272000, "input_modalities": ["text", "image"] }]
        })
        .to_string();

        let trimmed = trim_model_catalog(&body).unwrap();
        let value: Value = serde_json::from_str(&trimmed).unwrap();
        assert_eq!(value["models"][0]["context_window"], 272000);
        assert_eq!(value["models"][0]["input_modalities"][1], "image");
    }

    #[test]
    fn trim_model_catalog_rejects_a_response_without_a_models_array() {
        assert!(trim_model_catalog(r#"{"not_models": []}"#).is_err());
    }
}
