//! The pathfinder: a read-only research subagent. One tool call spawns a
//! nested voice restricted to inspect tools (read/glob/grep + readonly
//! bash), runs the query to completion, and returns only the final text —
//! the parent's context sees a dense summary, never the noisy transcript.

use std::pin::Pin;

use Future;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};
use crate::voice::{GuardRuntime, Voice};

/// Shared bootstrap the engine injects after catalog/model resolution.
#[derive(Default, Clone)]
pub struct PathfinderSource {
    /// Catalog for the nested voice.
    pub catalog: ka_dialect::Catalog,
    /// Model selector for the nested voice.
    pub model: Option<String>,
    /// Active strand id — nested voices seed their requests with it so
    /// gateways that route by conversation see stable traffic
    /// (engine writes it at strand attach).
    pub session_id: Option<String>,
}

/// The pathfinder tool.
pub struct PathfinderHand {
    source: Arc<parking_lot::RwLock<PathfinderSource>>,
}

impl PathfinderHand {
    /// New hand with a fresh source slot.
    pub fn new() -> Self {
        Self {
            source: Arc::new(parking_lot::RwLock::new(PathfinderSource::default())),
        }
    }

    /// Hand over an engine-owned slot.
    pub fn from_slot(slot: Arc<parking_lot::RwLock<PathfinderSource>>) -> Self {
        Self { source: slot }
    }
}

impl Default for PathfinderHand {
    fn default() -> Self {
        Self::new()
    }
}

const PATHFINDER_SYSTEM: &str = "You are pathfinder, a read-only research \\
subagent. Investigate the query with the read/glob/grep tools (read-only \\
bash allowed for inspection like `ls` or `git log`). Never modify \\
anything. When done, reply with ONLY a dense factual summary: findings, \\
exact file paths, line references, and direct answers. No preamble.";

impl Hand for PathfinderHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "pathfinder".to_string(),
            description: "Delegate a read-only research question to a subagent that \
                explores the repository and returns a dense summary. Use for broad \
                searches ('where is X handled?') to keep this context clean."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The research question" }
                },
                "required": ["query"]
            }),
            clearance: Clearance::Read,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let Some(query) = args.get("query").and_then(Value::as_str) else {
                return ToolOutput::err("pathfinder: missing required 'query'");
            };
            let source = self.source.read().clone();
            let Some(model) = source.model else {
                return ToolOutput::err("pathfinder: no model configured for the parent session");
            };

            let mut voice =
                Voice::new_readonly(source.catalog, ctx.cwd.clone(), ka_protocol::Mode::Free, 12);
            voice.set_model_selector(&model, 4.0);
            voice.set_session_id(source.session_id.clone());
            // system rides via the voice's git line; inject the subagent
            // framing by prefixing the query
            let prompt = format!("{PATHFINDER_SYSTEM}\\n\\nResearch query: {query}");

            let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
            let (evt_tx, mut evt_rx) = mpsc::channel(256);
            let handle = tokio::spawn(async move {
                let mut steers = Vec::new();
                let mut queue = std::collections::VecDeque::new();
                voice
                    .turn(
                        &model,
                        prompt,
                        &mut cmd_rx,
                        &evt_tx,
                        &mut steers,
                        &mut queue,
                        &mut GuardRuntime::default(),
                        None,
                        Vec::new(),
                    )
                    .await;
            });

            let mut summary = String::new();
            let mut thought = String::new();
            let mut failed: Option<String> = None;
            // 10-minute cap on research
            let deadline = tokio::time::timeout(std::time::Duration::from_secs(600), async {
                while let Some(evt) = evt_rx.recv().await {
                    match evt {
                        ka_protocol::Event::Delta {
                            kind: ka_protocol::DeltaKind::Text(t),
                        } => summary.push_str(&t),
                        ka_protocol::Event::Delta {
                            kind: ka_protocol::DeltaKind::Thought(t),
                        } => thought.push_str(&t),
                        ka_protocol::Event::Error { message, .. } => failed = Some(message),
                        ka_protocol::Event::TurnFinished { .. } => break,
                        _ => {}
                    }
                }
            })
            .await;
            drop(cmd_tx);
            let _ = handle.await;

            if !matches!(deadline, Ok(())) {
                return ToolOutput::err("pathfinder: research timed out (10m)");
            }
            if summary.trim().is_empty() && !thought.trim().is_empty() {
                summary = thought; // thinking models: reason-only replies
            }
            if summary.trim().is_empty() {
                return ToolOutput::err(format!(
                    "pathfinder failed: {}",
                    failed.unwrap_or_else(|| "no summary produced".to_string())
                ));
            }
            ToolOutput::ok(summary)
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn def_is_readonly_with_query_param() {
        let hand = PathfinderHand::new();
        let def = hand.def();
        assert_eq!(def.name, "pathfinder");
        assert_eq!(def.clearance, Clearance::Read);
        assert!(def.read_only);
        assert!(def.parameters.to_string().contains("query"));
    }
}
