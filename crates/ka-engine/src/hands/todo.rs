//! The todo hand: the model's live plan, mirrored to surfaces as
//! [`ka_protocol::Event::Todos`]. Each call REPLACES the whole list — the
//! model sends every item with its current state, so there is no merge
//! logic to drift. The normalized list lands in a shared slot; the voice
//! forwards it to surfaces after execution (the hand itself has no event
//! channel by design).

use std::future::Future;
use std::pin::Pin;

use serde_json::{Value, json};

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};

/// Maximum items kept in one todo list.
pub const MAX_ITEMS: usize = 32;
/// Maximum characters per item text.
pub const MAX_TEXT: usize = 200;

/// Shared slot the hand writes and the voice reads to emit the
/// [`ka_protocol::Event::Todos`] surface event.
pub type TodoSlot = std::sync::Arc<parking_lot::Mutex<Vec<ka_protocol::TodoItem>>>;

/// A fresh, empty slot.
pub fn slot() -> TodoSlot {
    std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()))
}

/// The todo tool.
pub struct TodoHand {
    slot: TodoSlot,
}

impl TodoHand {
    /// Hand over a shared slot (usually one per voice, cloned into it).
    pub fn new(slot: TodoSlot) -> Self {
        Self { slot }
    }
}

/// Normalize model arguments into the canonical list. Unknown states fall
/// back to pending, texts truncate at [`MAX_TEXT`] chars, and the list
/// caps at [`MAX_ITEMS`] items. An empty array clears the list. Malformed
/// payloads (missing `items`, non-array `items`, item without string
/// `text`, out-of-range `confidence`) error so the model can self-correct.
pub fn normalize(args: &Value) -> Result<Vec<ka_protocol::TodoItem>, String> {
    let Some(items) = args.get("items") else {
        return Err("todo requires an `items` array".to_string());
    };
    let Some(items) = items.as_array() else {
        return Err("todo `items` must be an array".to_string());
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().take(MAX_ITEMS).enumerate() {
        let Some(text) = item.get("text").and_then(Value::as_str) else {
            return Err(format!("items[{i}] needs a string `text`"));
        };
        let text: String = text.chars().take(MAX_TEXT).collect();
        let state = match item.get("state").and_then(Value::as_str) {
            Some("done") => ka_protocol::TodoState::Done,
            _ => ka_protocol::TodoState::Pending,
        };
        let confidence = match item.get("confidence") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let c = v
                    .as_u64()
                    .filter(|c| *c <= 100)
                    .ok_or_else(|| format!("items[{i}] `confidence` must be an integer 0–100"))?;
                Some(c as u8)
            }
        };
        out.push(ka_protocol::TodoItem {
            text,
            state,
            confidence,
        });
    }
    Ok(out)
}

/// A low rating at assignment — below this, a later high rating is a
/// spike worth checking (jcode's confidence-stepping thresholds).
const SPIKE_LOW: u8 = 40;
/// A high rating at completion.
const SPIKE_HIGH: u8 = 90;

/// Marker prefix the voice relays as a transcript note. Shared const so
/// the hand's advisory line and the voice's detection cannot drift.
pub const SPIKE_MARK: &str = "⚠ confidence spike";

/// Confidence-spike detection between two consecutive lists: an item
/// flipped pending→done whose assignment confidence was low (≤
/// [`SPIKE_LOW`]) but whose completion confidence is high (≥
/// [`SPIKE_HIGH`]). The jump is a claim, not a measurement — the harness
/// asks for verification instead of accepting it. Unrated sides never
/// spike (only real low-assignment signal counts). One advisory line
/// per spike, item text capped like the item itself.
pub fn confidence_spikes(
    prev: &[ka_protocol::TodoItem],
    next: &[ka_protocol::TodoItem],
) -> Vec<String> {
    next.iter()
        .filter(|n| n.state == ka_protocol::TodoState::Done)
        .filter_map(|n| {
            let p = prev
                .iter()
                .find(|p| p.text == n.text && p.state == ka_protocol::TodoState::Pending)?;
            let (Some(lo), Some(hi)) = (p.confidence, n.confidence) else {
                return None;
            };
            (lo <= SPIKE_LOW && hi >= SPIKE_HIGH).then(|| {
                format!(
                    "{SPIKE_MARK} \"{}\" ({lo}→{hi}): verify this item before relying on it",
                    n.text.chars().take(MAX_TEXT).collect::<String>()
                )
            })
        })
        .collect()
}

impl Hand for TodoHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "todo".into(),
            description: "Maintain your live todo list for the session sidebar. Each call \
replaces the WHOLE list: send every item with its state (`pending` or `done`) and your \
confidence 0–100 — rate an item when you assign it, and re-rate it when you mark it done \
(let verification raise it step by step, don't just declare 100). An empty list clears \
it. Keep it current on multi-step tasks."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "maxItems": MAX_ITEMS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": { "type": "string", "maxLength": MAX_TEXT },
                                "state": { "type": "string", "enum": ["pending", "done"] },
                                "confidence": { "type": "integer", "minimum": 0, "maximum": 100 }
                            },
                            "required": ["text", "state"]
                        }
                    }
                },
                "required": ["items"]
            }),
            clearance: Clearance::Read,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let sent = args
                .get("items")
                .and_then(Value::as_array)
                .map_or(0, |a| a.len());
            match normalize(args) {
                Ok(items) => {
                    let kept = items.len();
                    let done = items
                        .iter()
                        .filter(|i| i.state == ka_protocol::TodoState::Done)
                        .count();
                    let spikes = {
                        let mut slot = self.slot.lock();
                        let spikes = confidence_spikes(&slot, &items);
                        *slot = items;
                        spikes
                    };
                    let mut note = format!("todo list updated: {kept} items ({done} done)");
                    if sent > kept {
                        note.push_str(&format!("; capped at {MAX_ITEMS} items"));
                    }
                    for spike in &spikes {
                        note.push_str(&format!("\n{spike}"));
                    }
                    ToolOutput::ok(note)
                }
                Err(e) => ToolOutput::err(e),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_keeps_full_shape() {
        let items = normalize(&json!({
            "items": [
                {"text": "first", "state": "pending"},
                {"text": "second", "state": "done"},
            ]
        }))
        .unwrap();
        assert_eq!(
            items,
            vec![
                ka_protocol::TodoItem {
                    text: "first".into(),
                    state: ka_protocol::TodoState::Pending,
                    confidence: None
                },
                ka_protocol::TodoItem {
                    text: "second".into(),
                    state: ka_protocol::TodoState::Done,
                    confidence: None
                },
            ]
        );
    }

    #[test]
    fn normalize_parses_confidence_and_rejects_out_of_range() {
        let items = normalize(&json!({
            "items": [
                {"text": "a", "state": "pending", "confidence": 35},
                {"text": "b", "state": "done", "confidence": 100},
                {"text": "c", "state": "pending", "confidence": null},
            ]
        }))
        .unwrap();
        assert_eq!(items[0].confidence, Some(35));
        assert_eq!(items[1].confidence, Some(100));
        assert_eq!(items[2].confidence, None);
        assert!(
            normalize(&json!({"items": [{"text": "x", "confidence": 101}]})).is_err(),
            "above 100 errors"
        );
        assert!(
            normalize(&json!({"items": [{"text": "x", "confidence": -1}]})).is_err(),
            "negative errors"
        );
        assert!(
            normalize(&json!({"items": [{"text": "x", "confidence": "high"}]})).is_err(),
            "non-integer errors"
        );
    }

    #[test]
    fn confidence_spikes_flag_low_to_high_completion_jumps() {
        let item =
            |text: &str, state: ka_protocol::TodoState, c: Option<u8>| ka_protocol::TodoItem {
                text: text.into(),
                state,
                confidence: c,
            };
        let prev = vec![
            item("spiky", ka_protocol::TodoState::Pending, Some(35)),
            item("honest", ka_protocol::TodoState::Pending, Some(85)),
            item("unrated", ka_protocol::TodoState::Pending, None),
            item("still-pending", ka_protocol::TodoState::Pending, Some(10)),
        ];
        let next = vec![
            item("spiky", ka_protocol::TodoState::Done, Some(100)),
            item("honest", ka_protocol::TodoState::Done, Some(95)),
            item("unrated", ka_protocol::TodoState::Done, Some(100)),
            item("still-pending", ka_protocol::TodoState::Pending, Some(10)),
        ];
        let spikes = confidence_spikes(&prev, &next);
        assert_eq!(spikes.len(), 1, "{spikes:?}");
        assert!(spikes[0].contains(SPIKE_MARK), "{}", spikes[0]);
        assert!(spikes[0].contains("\"spiky\" (35→100)"), "{}", spikes[0]);
        // mid-range completion on a low assignment is not a spike
        let prev2 = vec![item("m", ka_protocol::TodoState::Pending, Some(20))];
        let next2 = vec![item("m", ka_protocol::TodoState::Done, Some(80))];
        assert!(confidence_spikes(&prev2, &next2).is_empty());
    }

    #[test]
    fn execute_appends_spike_advisory_to_the_result() {
        let hand = TodoHand::new(slot());
        let ctx = test_ctx();
        let assign = tokio_test_block(hand.execute(
            &json!({"items": [{"text": "wire it", "state": "pending", "confidence": 30}]}),
            &ctx,
        ));
        assert!(!assign.is_error);
        assert!(!assign.content.contains(SPIKE_MARK));
        let done = tokio_test_block(hand.execute(
            &json!({"items": [{"text": "wire it", "state": "done", "confidence": 100}]}),
            &ctx,
        ));
        assert!(!done.is_error, "{}", done.content);
        assert!(done.content.contains(SPIKE_MARK), "{}", done.content);
        assert!(
            done.content.contains("verify this item"),
            "{}",
            done.content
        );
    }

    #[test]
    fn normalize_caps_items_and_text() {
        let items: Vec<_> = (0..40)
            .map(|i| json!({"text": "x".repeat(300), "state": "pending", "n": i}))
            .collect();
        let out = normalize(&json!({ "items": items })).unwrap();
        assert_eq!(out.len(), MAX_ITEMS, "list caps at {MAX_ITEMS}");
        assert!(out.iter().all(|i| i.text.chars().count() == MAX_TEXT));
    }

    #[test]
    fn normalize_empty_array_clears_and_unknown_state_falls_back() {
        assert!(normalize(&json!({ "items": [] })).unwrap().is_empty());
        let items = normalize(&json!({ "items": [{"text": "a", "state": "bananas"}] })).unwrap();
        assert_eq!(items[0].state, ka_protocol::TodoState::Pending);
        let items = normalize(&json!({ "items": [{"text": "a"}] })).unwrap();
        assert_eq!(items[0].state, ka_protocol::TodoState::Pending);
    }

    #[test]
    fn normalize_rejects_malformed_payloads() {
        assert!(normalize(&json!({})).is_err(), "missing items");
        assert!(
            normalize(&json!({ "items": "hurry" })).is_err(),
            "non-array"
        );
        assert!(
            normalize(&json!({ "items": [{"state": "done"}] })).is_err(),
            "item without text"
        );
    }

    #[test]
    fn execute_replaces_slot_and_reports() {
        let hand = TodoHand::new(slot());
        let ctx = test_ctx();
        let out = tokio_test_block(hand.execute(
            &json!({"items": [
                {"text": "dig", "state": "done"},
                {"text": "fill", "state": "pending"},
            ]}),
            &ctx,
        ));
        assert!(!out.is_error);
        assert!(out.content.contains("2 items (1 done)"), "{}", out.content);
        let held = hand.slot.lock();
        assert_eq!(held.len(), 2);
        assert_eq!(held[0].state, ka_protocol::TodoState::Done);
    }

    #[test]
    fn execute_error_leaves_slot_untouched() {
        let hand = TodoHand::new(slot());
        let ctx = test_ctx();
        let ok = tokio_test_block(hand.execute(
            &json!({"items": [{"text": "kept", "state": "pending"}]}),
            &ctx,
        ));
        assert!(!ok.is_error);
        let bad = tokio_test_block(hand.execute(&json!({}), &ctx));
        assert!(bad.is_error);
        assert_eq!(
            hand.slot.lock().len(),
            1,
            "failed call must not touch the list"
        );
    }

    // — helpers —

    fn test_ctx() -> HandContext {
        HandContext {
            cwd: std::env::temp_dir(),
            ledger: std::sync::Arc::new(parking_lot::Mutex::new(super::super::Ledger::default())),
            spill: std::sync::Arc::new(super::super::Spill::new()),
            snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                super::super::snapshots::Snapshots::inert(),
            )),
            jobs: std::sync::Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
            shell_env: Default::default(),
        }
    }

    fn tokio_test_block(fut: impl Future<Output = ToolOutput>) -> ToolOutput {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }
}
