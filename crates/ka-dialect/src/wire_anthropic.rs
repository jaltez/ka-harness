//! The anthropic-messages wire: Messages API with SSE streaming, content
//! blocks (text / thinking / tool_use), and `cache_control` breakpoints.

use futures_util::StreamExt;
use ka_protocol::Stop;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::client::{WireError, post_sse};
use crate::json_repair::repair_json;
use crate::speaker::{SpeakRequest, Speaker, StreamEvent, ToolCall, speak_failed};
use crate::sse::SseParser;

/// Speaker for `wire = "anthropic_messages"` dialects.
#[derive(Debug, Clone)]
pub struct AnthropicMessages {
    client: reqwest::Client,
}

impl AnthropicMessages {
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

impl Default for AnthropicMessages {
    fn default() -> Self {
        Self::new()
    }
}

impl Speaker for AnthropicMessages {
    fn speak<'a>(
        &'a self,
        req: SpeakRequest,
        out: mpsc::Sender<StreamEvent>,
    ) -> crate::speaker::SpeakFuture<'a> {
        Box::pin(async move {
            if let Err(e) = speak_anthropic(&self.client, &req, &out).await {
                let _ = out.send(speak_failed(e)).await;
            }
        })
    }
}

async fn speak_anthropic(
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
    let url = format!("{}/v1/messages", base.trim_end_matches('/'));

    let mut headers: Vec<(String, String)> = vec![
        ("content-type".into(), "application/json".into()),
        ("anthropic-version".into(), "2023-06-01".into()),
    ];
    if let Some(token) = req.token.clone() {
        headers.push(("x-api-key".into(), token));
    }

    let wire_model = dialect.wire_model.clone().unwrap_or_else(|| {
        req.model_id
            .split_once('/')
            .map(|(_, m)| m.to_string())
            .unwrap_or(req.model_id.clone())
    });

    let mut body = json!({
        "model": wire_model,
        "max_tokens": dialect.max_output,
        "stream": true,
    });
    if !req.system.is_empty() {
        // cache_control on the stable system prefix when the dialect asks
        if dialect.cache == crate::dialects::Cache::Control {
            body["system"] = json!([{
                "type": "text",
                "text": req.system,
                "cache_control": { "type": "ephemeral" }
            }]);
        } else {
            body["system"] = json!(req.system);
        }
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
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        if native_search {
            tools.push(json!({
                "type": "web_search_20250305",
                "name": "web_search",
                "max_uses": 3,
            }));
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
    }
    if let Some(schema) = &req.schema {
        let mut tools: Vec<Value> = body["tools"].as_array().cloned().unwrap_or_default();
        tools.push(json!({
            "name": "structured_output",
            "description": "Reply with the final answer as JSON matching the given schema.",
            "input_schema": schema,
        }));
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!({ "type": "tool", "name": "structured_output" });
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
    let image_blocks = |images: &[crate::ImagePart]| -> Vec<Value> {
        images
            .iter()
            .map(|img| {
                json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": img.media_type,
                        "data": img.data,
                    }
                })
            })
            .collect()
    };
    let mut messages = Vec::new();
    for m in &req.messages {
        match m.role {
            crate::speaker::TurnRole::User if !m.images.is_empty() => {
                let mut blocks = image_blocks(&m.images);
                blocks.push(json!({"type": "text", "text": m.content}));
                messages.push(json!({"role": "user", "content": blocks}));
            }
            crate::speaker::TurnRole::User => {
                messages.push(json!({"role": "user", "content": m.content}));
            }
            crate::speaker::TurnRole::Assistant if !m.calls.is_empty() || !m.results.is_empty() => {
                let mut blocks = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(json!({"type": "text", "text": m.content}));
                }
                for c in &m.calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": c.id,
                        "name": c.tool,
                        "input": c.arguments,
                    }));
                }
                messages.push(json!({"role": "assistant", "content": blocks}));
            }
            crate::speaker::TurnRole::Assistant => {
                messages.push(json!({"role": "assistant", "content": m.content}));
            }
            crate::speaker::TurnRole::Tool => {
                let blocks: Vec<Value> = m
                    .results
                    .iter()
                    .map(|r| {
                        if r.images.is_empty() {
                            json!({
                                "type": "tool_result",
                                "tool_use_id": r.call_id,
                                "content": r.content,
                                "is_error": r.is_error,
                            })
                        } else {
                            // native: tool_result content takes image blocks
                            let mut parts = image_blocks(&r.images);
                            parts.push(json!({"type": "text", "text": r.content}));
                            json!({
                                "type": "tool_result",
                                "tool_use_id": r.call_id,
                                "content": parts,
                                "is_error": r.is_error,
                            })
                        }
                    })
                    .collect();
                messages.push(json!({"role": "user", "content": blocks}));
            }
        }
    }
    if !messages.is_empty() {
        body["messages"] = Value::Array(messages);
    }
    if let Some(effort) = req.effort.as_deref() {
        if let Some(budget) = dialect.effort_budgets.get(effort) {
            let budget = (*budget).min(dialect.max_output.saturating_sub(1).max(1024));
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        }
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
    let mut blocks: Vec<Block> = Vec::new();

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
            let Ok(v) = serde_json::from_str::<Value>(&evt.data) else {
                continue;
            };
            match v.get("type").and_then(Value::as_str).unwrap_or("") {
                "message_start" => {
                    if let Some(u) = v.pointer("/message/usage") {
                        usage.input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                        usage.cache_read = u
                            .get("cache_read_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        usage.cache_write = u
                            .get("cache_creation_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                    }
                }
                "content_block_start" => {
                    let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let kind = v
                        .pointer("/content_block/type")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    while blocks.len() <= index {
                        blocks.push(Block::default());
                    }
                    blocks[index] = Block {
                        kind: kind.to_string(),
                        id: v
                            .pointer("/content_block/id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        tool: v
                            .pointer("/content_block/name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        args: String::new(),
                    };
                    // web_search_tool_result arrives complete in the
                    // start event: surface the sources as text the
                    // model and the transcript both read — or the
                    // failure, when the provider reports one
                    if kind == "web_search_tool_result" {
                        if let Some(sources) = search_sources(&v) {
                            out.send(StreamEvent::Text(sources)).await.ok();
                        } else if let Some(err) = search_error(&v) {
                            out.send(StreamEvent::Text(err)).await.ok();
                        }
                    }
                }
                "content_block_delta" => {
                    let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    if index >= blocks.len() {
                        continue;
                    }
                    let block = &mut blocks[index];
                    match v
                        .pointer("/delta/type")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                    {
                        "text_delta" => {
                            if let Some(text) = v.pointer("/delta/text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    out.send(StreamEvent::Text(text.to_string())).await.ok();
                                }
                            }
                        }
                        "thinking_delta" => {
                            if let Some(thinking) =
                                v.pointer("/delta/thinking").and_then(Value::as_str)
                            {
                                if !thinking.is_empty() {
                                    out.send(StreamEvent::Thought(thinking.to_string()))
                                        .await
                                        .ok();
                                }
                            }
                        }
                        "input_json_delta" => {
                            if let Some(part) =
                                v.pointer("/delta/partial_json").and_then(Value::as_str)
                            {
                                block.args.push_str(part);
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    if index < blocks.len() {
                        let b = &blocks[index];
                        if b.kind == "tool_use" && !b.tool.is_empty() {
                            // a structured-output request surfaces the forced
                            // tool's arguments as the reply text
                            if b.tool == "structured_output" {
                                out.send(StreamEvent::Text(b.args.clone())).await.ok();
                            } else {
                                out.send(StreamEvent::Call(ToolCall {
                                    id: b.id.clone(),
                                    tool: b.tool.clone(),
                                    arguments: repair_json(&b.args),
                                }))
                                .await
                                .ok();
                            }
                        }
                        // provider-executed search: the harness never
                        // runs it — surface the query the model asked
                        if b.kind == "server_tool_use" {
                            let query = repair_json(&b.args)
                                .get("query")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            if !query.is_empty() {
                                out.send(StreamEvent::Text(format!("\n[web_search: {query}]")))
                                    .await
                                    .ok();
                            }
                        }
                    }
                }
                "message_delta" => {
                    if let Some(reason) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                        stop = match reason {
                            "max_tokens" => Stop::Length,
                            // server tools (web search) can pause the
                            // turn server-side — surface it as a
                            // length stop, never a silent Done
                            "pause_turn" => Stop::Length,
                            _ => Stop::Done,
                        };
                    }
                    if let Some(u) = v.pointer("/usage") {
                        usage.output = u
                            .get("output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(usage.output);
                    }
                }
                "message_stop" => {}
                "error" => {
                    let message = v
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("provider stream error")
                        .to_string();
                    return Err(WireError {
                        class: ka_protocol::ErrorClass::Protocol,
                        retryable: false,
                        message,
                    });
                }
                _ => {} // ping etc.
            }
        }
    }
    // trailing frame without a final blank line: a dropped
    // message_delta would silently lose stop_reason + output usage —
    // drain like the openai wire does
    for evt in parser.finish() {
        let Ok(v) = serde_json::from_str::<Value>(&evt.data) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) == Some("message_delta") {
            if let Some(reason) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                stop = match reason {
                    "max_tokens" | "pause_turn" => Stop::Length,
                    _ => Stop::Done,
                };
            }
            if let Some(u) = v.pointer("/usage") {
                usage.output = u
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(usage.output);
            }
        }
    }
    // mixed server+client tool calls: the provider defers the search
    // (it runs only if the assistant message is resent with the
    // server-tool block, which ka's history replay does not carry) —
    // say so, or the query line above reads as if it ran
    if blocks.iter().any(|b| b.kind == "server_tool_use")
        && blocks.iter().any(|b| b.kind == "tool_use")
    {
        out.send(StreamEvent::Text(
            "\n[web_search deferred by the provider alongside client tool calls — it did NOT \
             run; re-issue the search after these tool results]"
                .to_string(),
        ))
        .await
        .ok();
    }
    out.send(StreamEvent::Finished { stop, usage }).await.ok();
    Ok(())
}

#[derive(Debug, Default, Clone)]
struct Block {
    kind: String,
    id: String,
    tool: String,
    args: String,
}

/// Render a `web_search_tool_result` block's content array as source
/// lines (≤5), or None when the block carries no results.
fn search_sources(v: &Value) -> Option<String> {
    let items = v.pointer("/content_block/content")?.as_array()?;
    let mut out = String::from("\nSources:");
    let mut n = 0;
    for it in items {
        let Some(title) = it.get("title").and_then(Value::as_str) else {
            continue;
        };
        let url = it.get("url").and_then(Value::as_str).unwrap_or("");
        out.push_str(&format!("\n- {title} — {url}"));
        n += 1;
        if n >= 5 {
            break;
        }
    }
    (n > 0).then_some(out)
}

/// A failed search reports an error object instead of a results
/// array — surface it or the query line implies it ran.
fn search_error(v: &Value) -> Option<String> {
    let code = v
        .pointer("/content_block/content/error_code")
        .and_then(Value::as_str)?;
    Some(format!("\n[web_search failed: {code}]"))
}
