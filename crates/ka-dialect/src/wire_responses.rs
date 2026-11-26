//! The openai-responses wire: `/responses` with typed SSE events — the
//! native API for reasoning models (o-series). Item-based history
//! (`function_call` / `function_call_output` instead of role tuples),
//! flat tool definitions, `reasoning.effort`, and per-event stream
//! frames instead of chat chunks.

use std::collections::HashSet;

use futures_util::StreamExt;
use ka_protocol::Stop;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::client::{WireError, post_sse};
use crate::json_repair::repair_json;
use crate::speaker::{SpeakRequest, Speaker, StreamEvent, ToolCall, speak_failed};
use crate::sse::SseParser;

/// Speaker for `wire = "openai_responses"` dialects.
#[derive(Debug, Clone)]
pub struct OpenaiResponses {
    client: reqwest::Client,
}

impl OpenaiResponses {
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

impl Default for OpenaiResponses {
    fn default() -> Self {
        Self::new()
    }
}

impl Speaker for OpenaiResponses {
    fn speak<'a>(
        &'a self,
        req: SpeakRequest,
        out: mpsc::Sender<StreamEvent>,
    ) -> crate::speaker::SpeakFuture<'a> {
        Box::pin(async move {
            if let Err(e) = speak_responses(&self.client, &req, &out).await {
                let _ = out.send(speak_failed(e)).await;
            }
        })
    }
}

async fn speak_responses(
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
    let url = format!("{}/responses", base.trim_end_matches('/'));

    let mut headers: Vec<(String, String)> =
        vec![("content-type".into(), "application/json".into())];
    if let Some(token) = req.token.clone() {
        headers.push(("authorization".into(), format!("Bearer {token}")));
    }

    let wire_model = dialect.wire_model.clone().unwrap_or_else(|| {
        req.model_id
            .split_once('/')
            .map(|(_, m)| m.to_string())
            .unwrap_or(req.model_id.clone())
    });

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
                    "type": "input_image",
                    "image_url": format!("data:{};base64,{}", img.media_type, img.data),
                })
            })
            .collect()
    };
    // history → input items
    let mut input: Vec<Value> = Vec::new();
    for m in &req.messages {
        match m.role {
            crate::speaker::TurnRole::User if !m.images.is_empty() => {
                let mut parts = vec![json!({"type": "input_text", "text": m.content})];
                parts.extend(image_parts(&m.images));
                input.push(json!({"type": "message", "role": "user", "content": parts}));
            }
            crate::speaker::TurnRole::User => {
                input.push(json!({"type": "message", "role": "user", "content": m.content}));
            }
            crate::speaker::TurnRole::Assistant => {
                if !m.content.trim().is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": m.content,
                    }));
                }
                for c in &m.calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": c.id,
                        "name": c.tool,
                        "arguments": serde_json::to_string(&c.arguments).unwrap_or_default(),
                    }));
                }
            }
            crate::speaker::TurnRole::Tool => {
                for r in &m.results {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": r.call_id,
                        "output": r.content,
                    }));
                    // function outputs are text-only here: the images ride
                    // a user message right after their result
                    if !r.images.is_empty() {
                        input.push(
                            json!({"type": "message", "role": "user", "content": image_parts(&r.images)}),
                        );
                    }
                }
            }
        }
    }

    let mut body = json!({
        "model": wire_model,
        "input": input,
        "stream": true,
        "store": false,
    });
    // MODEL-03: pass-through `service_tier` hint, only when configured
    if let Some(tier) = req.service_tier.as_deref().filter(|t| !t.is_empty()) {
        body["service_tier"] = json!(tier);
    }
    if !req.system.is_empty() {
        body["instructions"] = json!(req.system);
    }
    {
        let native_search = req.native_search && dialect.flags.server_search;
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            // native search replaces the client-side hand — same name,
            // provider executes it server-side
            .filter(|t| !(native_search && t.name == "web_search"))
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        if native_search {
            tools.push(json!({"type": "web_search"}));
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
    }
    if dialect.max_output > 0 {
        body["max_output_tokens"] = json!(dialect.max_output);
    }
    if let Some(schema) = &req.schema {
        body["text"] = json!({
            "format": { "type": "json_schema", "name": "output", "strict": true, "schema": schema }
        });
    }
    // same policy as the chat wire: only published levels, "off" keeps
    // the endpoint default, unlisted levels downgrade to the highest
    if let Some(effort) = req.effort.as_deref()
        && let Some(name) =
            crate::wire_openai::resolved_effort(dialect, effort, !dialect.efforts.is_empty())
    {
        body["reasoning"] = json!({"effort": name});
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
    // function calls accumulate across argument deltas, keyed by item_id
    let mut calls: Vec<PendingItem> = Vec::new();
    let mut saw_any_event = false;
    let mut saw_terminal = false;

    let mut first_chunk = true;
    loop {
        // The HTTP layer's first-byte timeout resolves at headers; the
        // wire must separately bound time-to-first-STREAM-chunk so a
        // stalled prefill behind 200 OK still fails fast and retryably.
        let deadline = if first_chunk && dialect.first_byte_timeout_ms > 0 {
            tokio::time::Duration::from_millis(dialect.first_byte_timeout_ms)
        } else {
            std::time::Duration::from_secs(300)
        };
        let item = match tokio::time::timeout(deadline, stream.next()).await {
            Ok(item) => item,
            Err(_) if first_chunk && dialect.first_byte_timeout_ms > 0 => {
                return Err(WireError::network(format!(
                    "no first byte within {}ms (headers arrived, stream stalled)",
                    dialect.first_byte_timeout_ms
                )));
            }
            Err(_) => {
                return Err(WireError::network(
                    "idle timeout: 300s without a stream chunk",
                ));
            }
        };
        first_chunk = false;
        let Some(chunk) = item else {
            break;
        };
        let chunk = chunk.map_err(|e| WireError::network(e.to_string()))?;
        for evt in parser.feed(&chunk) {
            handle_responses_event(
                &evt,
                &mut calls,
                &mut usage,
                &mut stop,
                &mut saw_any_event,
                &mut saw_terminal,
                out,
            )
            .await?;
        }
    }
    for evt in parser.finish() {
        // trailing frame without a final blank line: drain through the same
        // handler so Stop::Length, usage, output_item.done, and failed/error
        // events are never dropped
        handle_responses_event(
            &evt,
            &mut calls,
            &mut usage,
            &mut stop,
            &mut saw_any_event,
            &mut saw_terminal,
            out,
        )
        .await?;
    }

    if !saw_any_event {
        return Err(WireError {
            class: ka_protocol::ErrorClass::Protocol,
            retryable: false,
            message: "empty stream".to_string(),
        });
    }
    // A clean EOF is not a completed turn: without response.completed,
    // response.incomplete, or [DONE], the provider may have cut the
    // stream mid-answer.
    if !saw_terminal {
        return Err(WireError::network(
            "stream ended without response.completed/incomplete or [DONE]",
        ));
    }

    // Emit complete calls (deduped by call_id, falling back to item_id
    // when parallel calls ship without one, arguments repaired).
    let mut seen: HashSet<String> = HashSet::new();
    for pending in calls {
        let key = dedup_key(&pending.call_id, &pending.item_id);
        if pending.tool.is_empty() || !seen.insert(key) {
            continue;
        }
        let id = if pending.call_id.is_empty() {
            pending.item_id
        } else {
            pending.call_id
        };
        out.send(StreamEvent::Call(ToolCall {
            id,
            tool: pending.tool,
            arguments: repair_json(&pending.args),
        }))
        .await
        .ok();
    }
    out.send(StreamEvent::Finished { stop, usage }).await.ok();
    Ok(())
}

const MAX_TOOL_ARGS_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOOL_CALLS: usize = 1024;

async fn handle_responses_event(
    evt: &crate::sse::SseEvent,
    calls: &mut Vec<PendingItem>,
    usage: &mut ka_protocol::Usage,
    stop: &mut Stop,
    saw_any_event: &mut bool,
    saw_terminal: &mut bool,
    out: &mpsc::Sender<StreamEvent>,
) -> Result<(), WireError> {
    if evt.data.trim() == "[DONE]" {
        *saw_terminal = true;
        return Ok(());
    }
    let Ok(v) = serde_json::from_str::<Value>(&evt.data) else {
        return Ok(()); // malformed frame: drop, keep streaming
    };
    *saw_any_event = true;
    let evt_type = v.get("type").and_then(Value::as_str).unwrap_or("");
    match evt_type {
        "response.output_text.delta" => {
            if let Some(text) = v.get("delta").and_then(Value::as_str) {
                if !text.is_empty() {
                    out.send(StreamEvent::Text(text.to_string())).await.ok();
                }
            }
        }
        "response.reasoning_summary_text.delta" => {
            if let Some(thought) = v.get("delta").and_then(Value::as_str) {
                if !thought.is_empty() {
                    out.send(StreamEvent::Thought(thought.to_string()))
                        .await
                        .ok();
                }
            }
        }
        "response.output_item.added" => {
            let item = &v["item"];
            // provider-executed search: the harness never runs
            // it — surface the action the model took
            if item.get("type").and_then(Value::as_str) == Some("web_search_call") {
                if let Some(action) = item.get("action") {
                    let q = action
                        .pointer("/query/text")
                        .or_else(|| action.get("query"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !q.is_empty() {
                        out.send(StreamEvent::Text(format!("\n[web_search: {q}]")))
                            .await
                            .ok();
                    }
                }
            }
            if item.get("type").and_then(Value::as_str) == Some("function_call")
                && calls.len() < MAX_TOOL_CALLS
            {
                calls.push(PendingItem {
                    item_id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    call_id: item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    tool: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    args: String::new(),
                });
            }
        }
        "response.function_call_arguments.delta" => {
            let item_id = v.get("item_id").and_then(Value::as_str).unwrap_or("");
            if let Some(pending) = calls.iter_mut().find(|c| c.item_id == item_id) {
                if let Some(chunk) = v.get("delta").and_then(Value::as_str) {
                    if pending.args.len().saturating_add(chunk.len()) > MAX_TOOL_ARGS_BYTES {
                        return Err(WireError {
                            class: ka_protocol::ErrorClass::Protocol,
                            retryable: false,
                            message: format!(
                                "tool call arguments exceeded {MAX_TOOL_ARGS_BYTES} byte limit"
                            ),
                        });
                    }
                    pending.args.push_str(chunk);
                }
            }
        }
        "response.output_item.done" => {
            let item = &v["item"];
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                // authoritative full form: replace any accumulated state
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let tool = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if args.len() > MAX_TOOL_ARGS_BYTES {
                    return Err(WireError {
                        class: ka_protocol::ErrorClass::Protocol,
                        retryable: false,
                        message: format!(
                            "tool call arguments exceeded {MAX_TOOL_ARGS_BYTES} byte limit"
                        ),
                    });
                }
                if !tool.is_empty() {
                    let incoming_id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    calls.retain(|c| {
                        dedup_key(&c.call_id, &c.item_id) != dedup_key(&call_id, &incoming_id)
                    });
                    if calls.len() < MAX_TOOL_CALLS {
                        calls.push(PendingItem {
                            // a call_id-less proxy emits `id` on both
                            // added and done: keep it, or emission
                            // would send an empty call id the
                            // provider rejects; with call_id present
                            // the deltas match by call_id, so the
                            // item id stays blank as before
                            item_id: if call_id.is_empty() {
                                incoming_id
                            } else {
                                String::new()
                            },
                            call_id,
                            tool,
                            args,
                        });
                    }
                }
            }
        }
        "response.completed" | "response.incomplete" => {
            *saw_terminal = true;
            let response = &v["response"];
            if evt_type == "response.incomplete" {
                *stop = Stop::Length;
            }
            if let Some(status) = response.get("status").and_then(Value::as_str) {
                if status == "incomplete" {
                    *stop = Stop::Length;
                }
            }
            if let Some(u) = response.get("usage").filter(|u| u.is_object()) {
                let input_tokens = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                let cached = u
                    .get("input_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                usage.input = input_tokens.saturating_sub(cached);
                usage.cache_read = cached;
                usage.output = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
            }
        }
        "response.failed" => {
            let message = v
                .pointer("/response/error/message")
                .and_then(Value::as_str)
                .unwrap_or("response failed");
            let code = v
                .pointer("/response/error/code")
                .and_then(Value::as_str)
                .unwrap_or("");
            return Err(failed_wire_error(code, message.to_string()));
        }
        "error" => {
            let message = v
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("stream error event");
            let code = v
                .pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap_or("");
            return Err(failed_wire_error(code, message.to_string()));
        }
        _ => {} // created/in_progress/other frames: ignored
    }
    Ok(())
}

#[derive(Debug, Default)]
struct PendingItem {
    item_id: String,
    call_id: String,
    tool: String,
    args: String,
}

/// Dedup key for a function call: call_id when present, else the item's
/// own id — two parallel calls without call_id must not collide on "".
fn dedup_key(call_id: &str, item_id: &str) -> String {
    if call_id.is_empty() {
        item_id.to_string()
    } else {
        call_id.to_string()
    }
}

/// A failed-response error: documented transient codes (server_error,
/// rate_limit_exceeded, timeout) stay retryable so the engine's backoff
/// re-drives; everything else is a protocol failure.
fn failed_wire_error(code: &str, message: String) -> WireError {
    match code {
        "server_error" | "rate_limit_exceeded" | "timeout" => WireError::network(message),
        _ => WireError {
            class: ka_protocol::ErrorClass::Protocol,
            retryable: false,
            message,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseEvent;

    #[tokio::test]
    async fn responses_caps_pending_tool_calls_at_limit() {
        let (tx, _rx) = mpsc::channel(16);
        let mut calls = Vec::new();
        let mut usage = ka_protocol::Usage::default();
        let mut stop = Stop::Done;
        let mut saw_any = false;
        let mut saw_terminal = false;

        for i in 0..(MAX_TOOL_CALLS + 200) {
            let evt = SseEvent {
                data: json!({
                    "type": "response.output_item.added",
                    "item": {
                        "type": "function_call",
                        "id": format!("item_{i}"),
                        "call_id": format!("call_{i}"),
                        "name": "read",
                    }
                })
                .to_string(),
            };
            let res = handle_responses_event(
                &evt,
                &mut calls,
                &mut usage,
                &mut stop,
                &mut saw_any,
                &mut saw_terminal,
                &tx,
            )
            .await;
            assert!(res.is_ok());
        }
        assert_eq!(calls.len(), MAX_TOOL_CALLS);
    }

    #[tokio::test]
    async fn responses_rejects_oversized_tool_arguments() {
        let (tx, _rx) = mpsc::channel(16);
        let mut calls = Vec::new();
        let mut usage = ka_protocol::Usage::default();
        let mut stop = Stop::Done;
        let mut saw_any = false;
        let mut saw_terminal = false;

        let big = "x".repeat(MAX_TOOL_ARGS_BYTES + 1);
        let evt = SseEvent {
            data: json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "id": "item_1",
                    "call_id": "call_1",
                    "name": "write",
                    "arguments": big,
                }
            })
            .to_string(),
        };
        let res = handle_responses_event(
            &evt,
            &mut calls,
            &mut usage,
            &mut stop,
            &mut saw_any,
            &mut saw_terminal,
            &tx,
        )
        .await;
        assert!(res.is_err());
    }
}
