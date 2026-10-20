//! The Speaker trait: what every wire must implement. One interface, two
//! Phase-1 implementations (openai-chat, anthropic-messages). A Speaker
//! streams normalized [`StreamEvent`]s into a channel — failures arrive as
//! `Failed` events, not as errors, so the engine treats both alike.

use std::future::Future;
use std::pin::Pin;

use ka_protocol::{ErrorClass, Stop, Usage};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::dialects::Dialect;

/// Conversation message in ka's neutral shape. Assistant messages may carry
/// tool calls; a following `Tool` message carries their results.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnMessage {
    /// Who spoke.
    pub role: TurnRole,
    /// What they said (plain text; tool blocks are carried separately).
    pub content: String,
    /// Reasoning captured alongside assistant text. Display-only:
    /// persisted for session replay, never resent to the LLM.
    pub thinking: Option<String>,
    /// Tool calls issued with this assistant message.
    pub calls: Vec<ToolCall>,
    /// Tool results carried by a `Tool`-role message.
    pub results: Vec<ToolResult>,
    /// Images attached to this message (user prompts, tool outputs).
    pub images: Vec<ImagePart>,
}

/// Attached image (base64 payload + media type).
pub use ka_protocol::ImagePart;

impl TurnMessage {
    /// A plain user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: TurnRole::User,
            content: content.into(),
            thinking: None,
            calls: Vec::new(),
            results: Vec::new(),
            images: Vec::new(),
        }
    }

    /// A user message carrying images.
    pub fn user_with_images(content: impl Into<String>, images: Vec<ImagePart>) -> Self {
        Self {
            role: TurnRole::User,
            content: content.into(),
            thinking: None,
            calls: Vec::new(),
            results: Vec::new(),
            images,
        }
    }

    /// A plain assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: TurnRole::Assistant,
            content: content.into(),
            thinking: None,
            calls: Vec::new(),
            results: Vec::new(),
            images: Vec::new(),
        }
    }

    /// An assistant message with tool calls.
    pub fn assistant_with_calls(content: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            role: TurnRole::Assistant,
            content: content.into(),
            thinking: None,
            calls,
            results: Vec::new(),
            images: Vec::new(),
        }
    }

    /// A tool-results message.
    pub fn tool(results: Vec<ToolResult>) -> Self {
        Self {
            role: TurnRole::Tool,
            content: String::new(),
            thinking: None,
            calls: Vec::new(),
            results,
            images: Vec::new(),
        }
    }
}

/// One executed tool result.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    /// The call this answers.
    pub call_id: String,
    /// Output text.
    pub content: String,
    /// Whether the tool reported an error.
    pub is_error: bool,
    /// Images produced by the tool (e.g. the read hand on an image file).
    pub images: Vec<ImagePart>,
}

/// Model-facing tool definition.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    /// Tool name.
    pub name: String,
    /// Description for the model.
    pub description: String,
    /// JSON schema for the arguments object.
    pub parameters: serde_json::Value,
}

/// Neutral roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    /// Human input.
    User,
    /// Model output.
    Assistant,
    /// Tool results.
    Tool,
}

/// A complete, parsed tool call (arguments accumulated dialect-side).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// Provider call id.
    pub id: String,
    /// Tool name.
    pub tool: String,
    /// Parsed arguments (repaired if the stream truncated).
    pub arguments: Value,
}

impl ToolCall {
    /// The call's primary string argument (what rules match against):
    /// bash→command, file tools→path, search tools→pattern.
    pub fn primary_arg(&self) -> String {
        for key in ["command", "path", "pattern"] {
            if let Some(v) = self.arguments.get(key).and_then(Value::as_str) {
                return v.to_string();
            }
        }
        self.arguments.to_string()
    }
}

/// Everything a Speaker needs for one request.
#[derive(Debug, Clone)]
pub struct SpeakRequest {
    /// `vendor/model` id.
    pub model_id: String,
    /// Resolved dialect (wire + flags + budgets).
    pub dialect: Dialect,
    /// Optional effort level.
    pub effort: Option<String>,
    /// System prompt (placement is wire-specific).
    pub system: String,
    /// Conversation so far (user/assistant/tool).
    pub messages: Vec<TurnMessage>,
    /// Tools offered to the model this request.
    pub tools: Vec<ToolSpec>,
    /// Bearer/API token, if the endpoint needs one.
    pub token: Option<String>,
    /// Cache key for providers that take one.
    pub cache_key: Option<String>,
    /// Stable per-conversation id (the active strand) — the wire sends
    /// it under the header named by `providers::session_header` for
    /// gateways that route on it (opencode-go: `x-opencode-session`).
    pub session_id: Option<String>,
    /// Structured-output schema the reply must satisfy (None = free-form).
    pub schema: Option<serde_json::Value>,
    /// The voice's go-ahead for provider-native server-side search:
    /// true only when the dialect flag is set AND the `web` feature
    /// word is enabled (the wire injects the provider's server tool
    /// only then — the feature toggle stays fail-closed).
    pub native_search: bool,
}

/// Normalized streaming events emitted by any wire.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Visible text delta.
    Text(String),
    /// Reasoning/thought delta.
    Thought(String),
    /// A tool call was fully received (arguments parsed).
    Call(ToolCall),
    /// The stream finished.
    Finished {
        /// Normalized stop reason.
        stop: Stop,
        /// Token usage (best known at finish; cost computed engine-side).
        usage: Usage,
    },
    /// The wire failed before finishing.
    Failed {
        /// Classification.
        class: ErrorClass,
        /// Whether a retry could help.
        retryable: bool,
        /// Human-readable detail.
        message: String,
    },
}

/// A boxed future as returned by [`Speaker::speak`].
pub type SpeakFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// What every wire implements. The channel-based shape keeps streaming
/// live, cancellation natural (drop the receiver), and the trait dyn-safe.
pub trait Speaker: Send + Sync {
    /// Speak one request, streaming events into `out` until `Finished` or
    /// `Failed` (always exactly one of those last).
    fn speak<'a>(&'a self, req: SpeakRequest, out: mpsc::Sender<StreamEvent>) -> SpeakFuture<'a>;
}

/// Convert a [`WireError`](crate::client::WireError) into a `Failed` event.
pub fn speak_failed(e: crate::client::WireError) -> StreamEvent {
    StreamEvent::Failed {
        class: e.class,
        retryable: e.retryable,
        message: e.message,
    }
}
