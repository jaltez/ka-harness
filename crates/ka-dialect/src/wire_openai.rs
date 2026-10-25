//! The openai-chat wire: Chat Completions with SSE streaming, covering
//! OpenAI, Ollama/LM Studio/vLLM compat endpoints, and OpenAI-shaped
//! gateways.

use futures_util::StreamExt;
use ka_protocol::Stop;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::client::{WireError, post_sse};
use crate::json_repair::repair_json;
use crate::speaker::{SpeakRequest, Speaker, StreamEvent, ToolCall, speak_failed};
use crate::sse::SseParser;

/// Speaker for `wire = "openai_chat"` dialects.
#[derive(Debug, Clone)]
pub struct OpenaiChat {
    client: reqwest::Client,
}

impl OpenaiChat {
    /// New speaker with a shared HTTP client (rustls, 10s connect).
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .user_agent(concat!("ka/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

impl Default for OpenaiChat {
    fn default() -> Self {
        Self::new()
    }
}

impl Speaker for OpenaiChat {
    fn speak<'a>(
        &'a self,
        req: SpeakRequest,
        out: mpsc::Sender<StreamEvent>,
    ) -> crate::speaker::SpeakFuture<'a> {
        Box::pin(async move {
            if let Err(e) = speak_openai(&self.client, &req, &out).await {
                let _ = out.send(speak_failed(e)).await;
            }
        })
    }
}

async fn speak_openai(
    client: &reqwest::Client,
    req: &SpeakRequest,
    out: &mpsc::Sender<StreamEvent>,
) -> Result<(), WireError> {
    let dialect = &req.dialect;
    let Some(base) = dialect.base_url.clone() else {
        return Err(WireError {
            class: ka_protocol::ErrorClass::Protocol,
            retryable: false,
            message: format!("dialect {} has no base_url", req.model_id),
        });
    };
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));

    let mut headers: Vec<(String, String)> =
        vec![("content-type".into(), "application/json".into())];
    if let Some(token) = req.token.clone() {
        headers.push(("authorization".into(), format!("Bearer {token}")));
    }
    // gateways that route by conversation require a stable session id on
    // every request (opencode-go answers HTTP 400 without
    // `x-opencode-session`)
    if let Some(header) =
        crate::providers::session_header(req.model_id.split('/').next().unwrap_or_default())
    {
        if let Some(sid) = req.session_id.as_deref().filter(|s| !s.is_empty()) {
            headers.push((header.to_string(), sid.to_string()));
        }
    }

    let wire_model = dialect.wire_model.clone().unwrap_or_else(|| {
        req.model_id
            .split_once('/')
            .map(|(_, m)| m.to_string())
            .unwrap_or(req.model_id.clone())
    });
    if dialect.flags.server_search && req.native_search {
        // Chat Completions has no server-side search tool — the native
        // path exists on anthropic_messages and openai_responses only
        return Err(WireError {
            class: ka_protocol::ErrorClass::Unsupported,
            retryable: false,
            message: format!(
                "model {}: server_search is not supported on the openai_chat wire — remove the \
                 flag (ka's [[search]] providers cover this wire)",
                req.model_id
            ),
        });
    }
    let mut body = json!({
        "model": wire_model,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }
    if let Some(schema) = &req.schema {
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": { "name": "output", "strict": true, "schema": schema },
        });
    }
    // vision gate: images on the wire require an image-input dialect
    let has_images = req
        .messages
        .iter()
        .any(|m| !m.images.is_empty() || m.results.iter().any(|r| !r.images.is_empty()));
    if has_images && !dialect.input.contains(&crate::dialects::Modality::Image) {
        return Err(WireError {
            class: ka_protocol::ErrorClass::Unsupported,
            retryable: false,
            message: format!("model {} has no vision", req.model_id),
        });
    }
    let image_parts = |images: &[crate::ImagePart]| -> Vec<Value> {
        images
            .iter()
            .map(|img| {
                json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{};base64,{}", img.media_type, img.data) }
                })
            })
            .collect()
    };
    let mut messages = Vec::new();
    if !req.system.is_empty() {
        messages.push(json!({"role": "system", "content": req.system}));
    }
    for m in &req.messages {
        match m.role {
            crate::speaker::TurnRole::User if !m.images.is_empty() => {
                let mut parts = vec![json!({"type": "text", "text": m.content})];
                parts.extend(image_parts(&m.images));
                messages.push(json!({"role": "user", "content": parts}));
            }
            crate::speaker::TurnRole::User => {
                messages.push(json!({"role": "user", "content": m.content}));
            }
            crate::speaker::TurnRole::Assistant if !m.calls.is_empty() => {
                let calls: Vec<Value> = m
                    .calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.tool,
                                "arguments": serde_json::to_string(&c.arguments).unwrap_or_default(),
                            }
                        })
                    })
                    .collect();
                let content = if m.content.is_empty() {
                    Value::Null
                } else {
                    json!(m.content)
                };
                messages
                    .push(json!({"role": "assistant", "content": content, "tool_calls": calls}));
            }
            crate::speaker::TurnRole::Assistant => {
                messages.push(json!({"role": "assistant", "content": m.content}));
            }
            crate::speaker::TurnRole::Tool => {
                for r in &m.results {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": r.call_id,
                        "content": r.content,
                    }));
                    // tool messages take text only on this wire: the
                    // images ride a user turn right after their result
                    if !r.images.is_empty() {
                        messages.push(json!({"role": "user", "content": image_parts(&r.images)}));
                    }
                }
            }
        }
    }
    if !messages.is_empty() {
        body["messages"] = Value::Array(messages);
    }
    let max_field = dialect
        .flags
        .max_tokens_field
        .clone()
        .unwrap_or_else(|| "max_tokens".into());
    if dialect.max_output > 0 {
        body[max_field.as_str()] = json!(dialect.max_output);
    }
    if let Some(effort) = req.effort.as_deref()
        && let Some(name) =
            resolved_effort(dialect, effort, dialect.flags.reasoning_field.is_some())
    {
        body["reasoning_effort"] = json!(name);
    }

    let resp = post_sse(
        client,
        &url,
        &headers,
        serde_json::to_string(&body).unwrap_or_else(|_| "{}".into()),
        dialect.first_byte_timeout_ms,
        3,
    )
    .await?;

    let mut parser = SseParser::new();
    let mut stream = resp.bytes_stream();
    let mut usage = ka_protocol::Usage::default();
    let mut stop = Stop::Done;
    // openai streams tool_calls by index; we accumulate per index and emit
    // complete Call events at stream end (or index switch).
    let mut calls: Vec<PendingCall> = Vec::new();
    let mut finished = false;
    let mut saw_finish_reason = false;

    loop {
        let item =
            match tokio::time::timeout(std::time::Duration::from_secs(300), stream.next()).await {
                Ok(item) => item,
                Err(_) => {
                    return Err(WireError::network(
                        "idle timeout: 300s without a stream chunk",
                    ));
                }
            };
        let Some(chunk) = item else {
            break;
        };
        let chunk = chunk.map_err(|e| WireError::network(e.to_string()))?;
        for evt in parser.feed(&chunk) {
            if evt.data.trim() == "[DONE]" {
                finished = true;
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&evt.data) else {
                continue; // malformed chunk: drop, keep streaming
            };
            if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                // OpenAI reports cached_tokens as a SUBSET of
                // prompt_tokens: keep input cache-exclusive (matching
                // wire_responses and the Anthropic wire) so consumers
                // that compute input + cache_read don't double-count
                let prompt = u.get("prompt_tokens").and_then(Value::as_u64);
                let cached = u
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(Value::as_u64)
                    .or_else(|| u.get("cached_tokens").and_then(Value::as_u64))
                    .unwrap_or(0);
                if let Some(p) = prompt {
                    usage.input = p.saturating_sub(cached);
                }
                usage.output = u
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(usage.output);
                usage.cache_read = cached;
            }
            let Some(choice) = v.pointer("/choices/0") else {
                continue;
            };
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                saw_finish_reason = true;
                stop = match reason {
                    "length" => Stop::Length,
                    _ => Stop::Done,
                };
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    out.send(StreamEvent::Text(text.to_string())).await.ok();
                }
            }
            for key in ["reasoning_content", "reasoning"] {
                if let Some(thought) = delta.get(key).and_then(Value::as_str) {
                    if !thought.is_empty() {
                        out.send(StreamEvent::Thought(thought.to_string()))
                            .await
                            .ok();
                    }
                    break;
                }
            }
            if let Some(tc) = delta.get("tool_calls").and_then(Value::as_array) {
                for frag in tc {
                    ingest_tool_fragment(&mut calls, frag);
                }
            }
        }
    }
    for evt in parser.finish() {
        // trailing frame without [DONE]: treat identically
        if evt.data.trim() == "[DONE]" {
            finished = true;
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&evt.data) {
            if let Some(choice) = v.pointer("/choices/0") {
                if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                    saw_finish_reason = true;
                    stop = match reason {
                        "length" => Stop::Length,
                        _ => Stop::Done,
                    };
                }
                if let Some(delta) = choice.get("delta") {
                    if let Some(text) = delta.get("content").and_then(Value::as_str) {
                        out.send(StreamEvent::Text(text.to_string())).await.ok();
                    }
                }
            }
        }
    }

    // A clean EOF is not a completed turn: without [DONE] or a
    // finish_reason the provider may have cut the stream mid-answer.
    // Fail retryably before emitting calls so a truncated tool call is
    // never executed.
    if !finished && !saw_finish_reason {
        return Err(WireError::network(
            "stream ended without [DONE] or finish_reason",
        ));
    }

    // Emit complete calls.
    for pending in calls {
        if pending.tool.is_empty() {
            continue;
        }
        out.send(StreamEvent::Call(ToolCall {
            id: pending.id,
            tool: pending.tool,
            arguments: repair_json(&pending.args),
        }))
        .await
        .ok();
    }
    out.send(StreamEvent::Finished { stop, usage }).await.ok();
    Ok(())
}

#[derive(Debug, Default)]
struct PendingCall {
    id: String,
    tool: String,
    args: String,
}

fn ingest_tool_fragment(calls: &mut Vec<PendingCall>, frag: &Value) {
    let index = frag.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
    while calls.len() <= index {
        calls.push(PendingCall::default());
    }
    let pending = &mut calls[index];
    if let Some(id) = frag.get("id").and_then(Value::as_str) {
        pending.id = id.to_string();
    }
    if let Some(name) = frag.pointer("/function/name").and_then(Value::as_str) {
        pending.tool = name.to_string();
    }
    if let Some(args) = frag.pointer("/function/arguments").and_then(Value::as_str) {
        pending.args.push_str(args);
    }
}

/// The reasoning-effort value the endpoint accepts, if any. The dialect
/// must expose reasoning control (`controllable`: the chat wires key off
/// `reasoning_field`, the Responses wire off a non-empty `efforts`
/// list) AND publish its levels; `"off"` and empty names are omitted so
/// the endpoint keeps its own default. An unlisted level downgrades to
/// the HIGHEST supported tier (the `resolve_effort` precedent) instead
/// of being silently dropped — an explicit user request deserves the
/// closest behavior, not a quiet no-op.
pub(crate) fn resolved_effort(
    dialect: &crate::dialects::Dialect,
    effort: &str,
    controllable: bool,
) -> Option<String> {
    if !controllable || effort.is_empty() || effort == "off" {
        return None;
    }
    if dialect.efforts.iter().any(|s| s == effort) {
        return Some(effort.to_string());
    }
    const RANK: [(&str, u8); 4] = [("low", 1), ("medium", 2), ("high", 3), ("max", 4)];
    let rank = |name: &str| RANK.iter().find(|(n, _)| *n == name).map(|(_, r)| *r);
    match dialect
        .efforts
        .iter()
        .filter_map(|s| rank(s).map(|r| (r, s)))
        .max_by_key(|(r, _)| *r)
    {
        // no known tier published (e.g. a vendor-specific level list):
        // the last published level is the vendor's own highest
        Some((_, s)) => Some(s.to_string()),
        None => dialect.efforts.last().map(|s| s.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn dialect(toml: &str) -> crate::dialects::Dialect {
        crate::dialects::Catalog::parse(toml)
            .expect("fixture parses")
            .dialects
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .expect("one row")
    }

    #[test]
    fn effort_sent_only_when_the_dialect_lists_the_level() {
        let glm = dialect(
            r#"[dialects."zai/glm-5.3"]
wire = "openai_chat"
efforts = ["low", "high", "max"]
[dialects."zai/glm-5.3".flags]
reasoning_field = "reasoning_effort"
"#,
        );
        let controllable = true;
        assert_eq!(
            resolved_effort(&glm, "high", controllable),
            Some("high".to_string())
        );
        assert_eq!(
            resolved_effort(&glm, "max", controllable),
            Some("max".to_string())
        );
        // glm-5.3 has no "medium": downgrade to its highest tier, not a
        // silent drop
        assert_eq!(
            resolved_effort(&glm, "medium", controllable),
            Some("max".to_string())
        );
        // Off never sends a level
        assert_eq!(resolved_effort(&glm, "off", controllable), None);

        let plain = dialect(
            r#"[dialects."x/y"]
wire = "openai_chat"
"#,
        );
        // no published control: nothing is ever sent
        assert_eq!(resolved_effort(&plain, "high", false), None);
    }
}
