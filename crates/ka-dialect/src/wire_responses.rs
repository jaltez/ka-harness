//! The openai-responses wire: `/responses` with typed SSE events — the
//! native API for reasoning models (o-series). Item-based history
//! (`function_call` / `function_call_output` instead of role tuples),
//! flat tool definitions, `reasoning.effort`, and per-event stream
//! frames instead of chat chunks.

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
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&evt.data) else {
                continue; // malformed frame: drop, keep streaming
            };
            saw_any_event = true;
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
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
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
                        if !tool.is_empty() {
                            calls.retain(|c| c.call_id != call_id);
                            calls.push(PendingItem {
                                item_id: String::new(),
                                call_id,
                                tool,
                                args,
                            });
                        }
                    }
                }
                "response.completed" | "response.incomplete" => {
                    let response = &v["response"];
                    if evt_type == "response.incomplete" {
                        stop = Stop::Length;
                    }
                    if let Some(status) = response.get("status").and_then(Value::as_str) {
                        if status == "incomplete" {
                            stop = Stop::Length;
                        }
                    }
                    if let Some(u) = response.get("usage").filter(|u| u.is_object()) {
                        let input_tokens =
                            u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
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
                    return Err(WireError {
                        class: ka_protocol::ErrorClass::Protocol,
                        retryable: false,
                        message: message.to_string(),
                    });
                }
                "error" => {
                    let message = v
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("stream error event");
                    return Err(WireError {
                        class: ka_protocol::ErrorClass::Protocol,
                        retryable: false,
                        message: message.to_string(),
                    });
                }
                _ => {} // created/in_progress/other frames: ignored
            }
        }
    }
    for evt in parser.finish() {
        let _ = evt; // responses terminates with response.completed; nothing to drain
    }

    if !saw_any_event {
        return Err(WireError {
            class: ka_protocol::ErrorClass::Protocol,
            retryable: false,
            message: "empty stream".to_string(),
        });
    }

    // Emit complete calls (deduped by call_id, arguments repaired).
    let mut seen: Vec<String> = Vec::new();
    for pending in calls {
        if pending.tool.is_empty() || seen.contains(&pending.call_id) {
            continue;
        }
        seen.push(pending.call_id.clone());
        out.send(StreamEvent::Call(ToolCall {
            id: if pending.call_id.is_empty() {
                pending.item_id.clone()
            } else {
                pending.call_id
            },
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
struct PendingItem {
    item_id: String,
    call_id: String,
    tool: String,
    args: String,
}
