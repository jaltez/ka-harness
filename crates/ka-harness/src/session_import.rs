//! `import-session` (roadmap CLI-09): import a foreign agent session into a
//! ka strand so history recorded by other tools becomes browsable (and
//! greppable/exportable) through ka's own store.
//!
//! Two formats, both append-only JSONL transcripts:
//!
//! - `claude` — Claude Code session files
//!   (`~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`). Each line carries
//!   `type` (`user` | `assistant` | `summary` | …), `uuid`/`parentUuid`
//!   (a chain of messages — file order is NOT always chain order),
//!   `timestamp`, `sessionId`, and the Anthropic API message under
//!   `message` (`message.role`, `message.content` as a string or a list of
//!   `text` / `thinking` / `tool_use` / `tool_result` blocks).
//! - `codex` — Codex CLI rollouts (`~/.codex/sessions/**/rollout-*.jsonl`).
//!   Each line is `{"timestamp", "type", "payload"}`; `response_item`
//!   payloads carry `message` (`input_text`/`output_text` blocks),
//!   `function_call`, `function_call_output` and `reasoning`
//!   (`summary_text` blocks). `event_msg` lines restate the same
//!   user/assistant text, so they are skipped (counted, noted) rather than
//!   imported twice.
//!
//! Mapping decisions (ka side: [`ka_strand::Record::Message`] with `calls`
//! / `results` / `thinking`):
//!
//! - Claude `tool_result` blocks become a [`ka_strand::Role::Tool`]
//!   message carrying [`ka_strand::StoredResult`]s (how ka persists tool
//!   output); `tool_use` blocks become [`ka_strand::StoredCall`]s on the
//!   assistant message; `thinking` blocks feed the `thinking` field.
//! - Codex `function_call`/`function_call_output` map the same way.
//!   Rollouts carry no error flag on outputs, so `is_error` is always
//!   `false` (documented assumption).
//! - Anything that does not fit (image blocks, unknown payload types,
//!   summary/system/meta lines) is skipped and counted — never silently
//!   dropped, never fatal.
//!
//! The destination strand is created fresh with
//! [`ka_strand::StrandFile::create`] + `set_persist(true)`; its first user
//! message is one line of import metadata (origin, source file, date — no
//! message content), followed by the mapped messages in chain order.
//!
//! Privacy: [`ImportReport`] carries counters and short aggregated notes
//! only, never message content (transcripts are the user's private data).

use std::path::{Path, PathBuf};

use ka_strand::{Role, StoredCall, StoredResult};

/// Result of one import: counters and the strand that was written.
/// Intentionally holds no message content — the report may be shown or
/// logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// The strand file that was created.
    pub path: PathBuf,
    /// Records appended to the strand: 1 context line + every mapped
    /// message.
    pub records: usize,
    /// Source lines successfully mapped into strand messages.
    pub mapped: usize,
    /// Source lines skipped: corrupt, types with no ka mapping, or lines
    /// whose blocks mapped to nothing.
    pub skipped: usize,
    /// Short, aggregated notes about what was skipped or adapted.
    pub notes: Vec<String>,
}

/// One foreign message shaped for a ka `Record::Message`.
#[derive(Debug, Clone, PartialEq)]
pub struct MappedRecord {
    pub role: Role,
    pub content: String,
    pub calls: Vec<StoredCall>,
    pub results: Vec<StoredResult>,
    pub thinking: Option<String>,
}

impl MappedRecord {
    fn message(role: Role, content: String) -> Self {
        Self {
            role,
            content,
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.content.is_empty()
            && self.calls.is_empty()
            && self.results.is_empty()
            && self.thinking.is_none()
    }
}

/// Outcome of parsing a whole transcript: the mapped records plus skip
/// accounting. Pure — no filesystem access.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseOutcome {
    pub mapped: Vec<MappedRecord>,
    pub skipped: usize,
    pub notes: Vec<String>,
}

/// One parsed Claude transcript line: its chain identity plus whatever
/// ka records it maps to (one line usually maps to exactly one record).
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeEntry {
    pub uuid: String,
    /// `parentUuid`; `None` for the chain root (or when the parent lives
    /// in another file — resumed sessions).
    pub parent: Option<String>,
    pub mapped: Vec<MappedRecord>,
    /// Blocks seen on this line that had no ka mapping (image blocks).
    pub unmapped_blocks: usize,
    /// A subagent transcript entry (`isSidechain: true`).
    pub sidechain: bool,
}

/// Import a foreign session transcript into a fresh ka strand for `cwd`.
///
/// `format` is `"claude"` (Claude Code session JSONL) or `"codex"`
/// (Codex CLI rollout JSONL); anything else is an error naming the
/// supported formats. Corrupt lines are counted and noted, never fatal.
/// If the strand cannot be created (e.g. `cwd` maps to an unwritable
/// store path) the error says so.
pub fn import_session(format: &str, source: &Path, cwd: &Path) -> Result<ImportReport, String> {
    // Fail fast on an unknown format before touching the filesystem.
    if format != "claude" && format != "codex" {
        return Err(format!(
            "unknown format {format:?}; supported formats: claude, codex"
        ));
    }
    let text = std::fs::read_to_string(source)
        .map_err(|e| format!("cannot read {}: {e}", source.display()))?;
    let outcome = match format {
        "claude" => parse_claude(&text)?,
        "codex" => parse_codex(&text)?,
        _ => unreachable!("format validated above"),
    };
    write_strand(format, source, cwd, outcome)
}

/// Materialize the parsed transcript as a new strand: header (written by
/// `create`), one context line, then the mapped messages in order.
fn write_strand(
    format: &str,
    source: &Path,
    cwd: &Path,
    outcome: ParseOutcome,
) -> Result<ImportReport, String> {
    let mut strand = ka_strand::StrandFile::create(cwd, None).map_err(|e| {
        format!(
            "cannot create strand in {} (unwritable directory?): {e}",
            cwd.display()
        )
    })?;
    strand.set_persist(true);
    let context = format!(
        "[imported session] origin: {format}; source: {}; date: {}; messages: {}",
        source.display(),
        ka_strand::now_rfc3339(),
        outcome.mapped.len(),
    );
    let mut records = 1usize;
    append_message(&mut strand, MappedRecord::message(Role::User, context))?;
    for mapped in outcome.mapped {
        append_message(&mut strand, mapped)?;
        records += 1;
    }
    let path = strand
        .path()
        .map(Path::to_path_buf)
        .ok_or_else(|| "strand was never materialized".to_string())?;
    Ok(ImportReport {
        path,
        records,
        mapped: records - 1,
        skipped: outcome.skipped,
        notes: outcome.notes,
    })
}

/// Append one mapped record as a strand message.
fn append_message(strand: &mut ka_strand::StrandFile, mapped: MappedRecord) -> Result<(), String> {
    strand
        .append(ka_strand::Record::Message {
            id: ka_strand::new_record_id(),
            role: mapped.role,
            content: mapped.content,
            calls: mapped.calls,
            results: mapped.results,
            thinking: mapped.thinking,
            images: Vec::new(),
        })
        .map_err(|e| format!("cannot write strand: {e}"))
}

// ---------------------------------------------------------------------------
// Claude Code session transcripts
// ---------------------------------------------------------------------------

/// Parse one Claude transcript line into a [`ClaudeEntry`]. `Ok(None)` for
/// line types with no ka mapping (`summary`, `system`, …) — counted as
/// skipped by the caller. `Err` for corrupt lines (bad JSON, or missing
/// required fields on a `user`/`assistant` line).
pub fn parse_claude_line(line: &str) -> Result<Option<ClaudeEntry>, String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let v: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("invalid JSON: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| "line is not a JSON object".to_string())?;
    let kind = obj.get("type").and_then(|t| t.as_str());
    match kind {
        Some("user") | Some("assistant") => {}
        // summary / system / progress / queued-commands / file-history
        // snapshots: no direct ka mapping.
        _ => return Ok(None),
    }
    let uuid = obj
        .get("uuid")
        .and_then(|u| u.as_str())
        .ok_or_else(|| "message line missing uuid".to_string())?
        .to_string();
    let parent = obj
        .get("parentUuid")
        .and_then(|p| p.as_str())
        .map(str::to_string);
    let message = obj
        .get("message")
        .ok_or_else(|| "message line missing message field".to_string())?;
    let mut unmapped_blocks = 0usize;
    let mapped = if kind == Some("user") {
        map_claude_user(message, &mut unmapped_blocks)
    } else {
        map_claude_assistant(message, &mut unmapped_blocks)
    };
    Ok(Some(ClaudeEntry {
        uuid,
        parent,
        mapped,
        unmapped_blocks,
        sidechain: obj.get("isSidechain").and_then(|s| s.as_bool()) == Some(true),
    }))
}

/// Map a Claude `user` line. A `tool_result` block makes this a Tool
/// message carrying results (ka's tool-output shape); any accompanying
/// text blocks ride as that record's content. Plain text (string content
/// or text blocks) is a User message. A user line with neither text nor
/// tool_results (e.g. image-only) maps to nothing.
fn map_claude_user(message: &serde_json::Value, unmapped_blocks: &mut usize) -> Vec<MappedRecord> {
    let mut results = Vec::new();
    let mut text = String::new();
    match message.get("content") {
        Some(serde_json::Value::String(s)) => text.push_str(s),
        Some(serde_json::Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => push_field_text(block, "text", &mut text),
                    Some("tool_result") => results.push(StoredResult {
                        call_id: block
                            .get("tool_use_id")
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        content: block_content_text(block),
                        is_error: block.get("is_error").and_then(|e| e.as_bool()) == Some(true),
                        images: Vec::new(),
                    }),
                    _ => *unmapped_blocks += 1,
                }
            }
        }
        _ => {}
    }
    if results.is_empty() {
        if text.is_empty() {
            Vec::new()
        } else {
            vec![MappedRecord::message(Role::User, text)]
        }
    } else {
        vec![MappedRecord {
            role: Role::Tool,
            content: text,
            calls: Vec::new(),
            results,
            thinking: None,
        }]
    }
}

/// Map a Claude `assistant` line: text blocks → content, `tool_use` →
/// calls, `thinking` blocks → the thinking field. An assistant line with
/// none of those maps to nothing.
fn map_claude_assistant(
    message: &serde_json::Value,
    unmapped_blocks: &mut usize,
) -> Vec<MappedRecord> {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut calls = Vec::new();
    match message.get("content") {
        Some(serde_json::Value::String(s)) => text.push_str(s),
        Some(serde_json::Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => push_field_text(block, "text", &mut text),
                    // Thinking blocks carry their text under `thinking`,
                    // not `text`.
                    Some("thinking") => push_field_text(block, "thinking", &mut thinking),
                    Some("tool_use") => calls.push(StoredCall {
                        id: block
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        tool: block
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        arguments: block
                            .get("input")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    }),
                    _ => *unmapped_blocks += 1,
                }
            }
        }
        _ => {}
    }
    let mapped = MappedRecord {
        role: Role::Assistant,
        content: text,
        calls,
        results: Vec::new(),
        thinking: (!thinking.is_empty()).then_some(thinking),
    };
    if mapped.is_empty() {
        Vec::new()
    } else {
        vec![mapped]
    }
}

/// Parse a whole Claude transcript: corrupt lines and non-message lines
/// are skipped with aggregated notes; the surviving entries are ordered
/// along the `parentUuid` chain (files are not always written in chain
/// order, and resumed sessions can reference parents from older files).
pub fn parse_claude(text: &str) -> Result<ParseOutcome, String> {
    let mut entries = Vec::new();
    let mut skipped = 0usize;
    let mut notes: Vec<String> = Vec::new();
    let mut corrupt = 0usize;
    let mut sidechains = 0usize;
    let mut unmapped_blocks = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_claude_line(line) {
            Ok(Some(entry)) => {
                unmapped_blocks += entry.unmapped_blocks;
                if entry.sidechain {
                    sidechains += 1;
                }
                if entry.mapped.is_empty() {
                    skipped += 1;
                } else {
                    entries.push(entry);
                }
            }
            // Blank lines and non-message types.
            Ok(None) => skipped += 1,
            Err(_) => corrupt += 1,
        }
    }
    if corrupt > 0 {
        skipped += corrupt;
        notes.push(format!("{corrupt} corrupt line(s) skipped"));
    }
    if unmapped_blocks > 0 {
        notes.push(format!("{unmapped_blocks} non-text block(s) skipped"));
    }
    if sidechains > 0 {
        notes.push(format!(
            "{sidechains} subagent (sidechain) entries imported"
        ));
    }
    let (mapped, chain_notes) = chain_order(entries);
    notes.extend(chain_notes);
    Ok(ParseOutcome {
        mapped,
        skipped,
        notes,
    })
}

/// Order Claude entries along their `parentUuid` chain. Roots are entries
/// whose parent is absent from the file; each child subtree follows its
/// parent depth-first, siblings keep file order. Entries unreachable from
/// any root (cycles) keep file order at the end with a note — nothing is
/// dropped.
fn chain_order(entries: Vec<ClaudeEntry>) -> (Vec<MappedRecord>, Vec<String>) {
    let mut notes = Vec::new();
    let positions: std::collections::HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.uuid.as_str(), i))
        .collect();
    // Children of each parent, in file order.
    let mut children: std::collections::HashMap<&str, Vec<usize>> =
        std::collections::HashMap::new();
    let mut roots = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        match e.parent.as_deref() {
            Some(p) if positions.contains_key(p) => {
                children.entry(p).or_default().push(i);
            }
            _ => roots.push(i),
        }
    }
    // Depth-first walk: reverse the root stack so file order survives the
    // LIFO pops; children are pushed reversed for the same reason.
    let mut stack: Vec<usize> = roots;
    stack.reverse();
    let mut ordered = Vec::with_capacity(entries.len());
    let mut seen = vec![false; entries.len()];
    while let Some(i) = stack.pop() {
        if seen[i] {
            continue;
        }
        seen[i] = true;
        ordered.push(i);
        if let Some(kids) = children.get(entries[i].uuid.as_str()) {
            for &c in kids.iter().rev() {
                if !seen[c] {
                    stack.push(c);
                }
            }
        }
    }
    let orphans = entries.len() - ordered.len();
    if orphans > 0 {
        notes.push(format!(
            "{orphans} entries unreachable via parentUuid (cycle) appended in file order"
        ));
    }
    for (i, flag) in seen.iter().enumerate() {
        if !flag {
            ordered.push(i);
        }
    }
    let mut slots: Vec<Option<ClaudeEntry>> = entries.into_iter().map(Some).collect();
    let mapped = ordered
        .into_iter()
        .filter_map(|i| slots[i].take())
        .flat_map(|entry| entry.mapped)
        .collect();
    (mapped, notes)
}

// ---------------------------------------------------------------------------
// Codex CLI rollouts
// ---------------------------------------------------------------------------

/// Parse one Codex rollout line. `Ok(None)` for envelope types with no ka
/// mapping (`event_msg` — duplicates `response_item` text —,
/// `session_meta`, `turn_context`, `compacted`, …). `Err` for corrupt
/// lines.
pub fn parse_codex_line(line: &str) -> Result<Option<MappedRecord>, String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let v: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("invalid JSON: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| "line is not a JSON object".to_string())?;
    if obj.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return Ok(None);
    }
    let payload = obj
        .get("payload")
        .ok_or_else(|| "response_item missing payload".to_string())?;
    let payload_type = payload
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    match payload_type {
        "message" => {
            let role = match payload.get("role").and_then(|r| r.as_str()) {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                // developer/system instructions ride as System text.
                _ => Role::System,
            };
            let mut text = String::new();
            match payload.get("content") {
                Some(serde_json::Value::String(s)) => text.push_str(s),
                Some(serde_json::Value::Array(blocks)) => {
                    for block in blocks {
                        // `input_text` (user), `output_text` (assistant),
                        // plain `text` in older rollouts.
                        if matches!(
                            block.get("type").and_then(|t| t.as_str()),
                            Some("input_text") | Some("output_text") | Some("text")
                        ) {
                            push_field_text(block, "text", &mut text);
                        }
                    }
                }
                _ => {}
            }
            if text.is_empty() {
                Ok(None)
            } else {
                Ok(Some(MappedRecord::message(role, text)))
            }
        }
        "function_call" | "custom_tool_call" => {
            // Codex carries arguments as a JSON-encoded string; fall back
            // to the raw string when it does not parse as JSON.
            let arguments = match payload.get("arguments") {
                Some(serde_json::Value::String(s)) => serde_json::from_str::<serde_json::Value>(s)
                    .unwrap_or(serde_json::Value::String(s.clone())),
                Some(v) => v.clone(),
                None => serde_json::Value::Null,
            };
            Ok(Some(MappedRecord {
                role: Role::Assistant,
                content: String::new(),
                calls: vec![StoredCall {
                    id: payload
                        .get("call_id")
                        .or_else(|| payload.get("id"))
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    tool: payload
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    arguments,
                }],
                results: Vec::new(),
                thinking: None,
            }))
        }
        "function_call_output" | "custom_tool_call_output" => Ok(Some(MappedRecord {
            role: Role::Tool,
            content: String::new(),
            calls: Vec::new(),
            results: vec![StoredResult {
                call_id: payload
                    .get("call_id")
                    .and_then(|i| i.as_str())
                    .unwrap_or_default()
                    .to_string(),
                content: codex_output_text(payload.get("output")),
                // Rollouts carry no error flag on outputs.
                is_error: false,
                images: Vec::new(),
            }],
            thinking: None,
        })),
        "reasoning" => {
            let mut thinking = String::new();
            // `summary` holds the reasoning summary items; older rollouts
            // may carry `content` reasoning_text instead.
            for field in ["summary", "content"] {
                if let Some(serde_json::Value::Array(blocks)) = payload.get(field) {
                    for block in blocks {
                        push_field_text(block, "text", &mut thinking);
                    }
                }
            }
            if thinking.is_empty() {
                // Encrypted/empty reasoning: nothing to map.
                Ok(None)
            } else {
                Ok(Some(MappedRecord {
                    role: Role::Assistant,
                    content: String::new(),
                    calls: Vec::new(),
                    results: Vec::new(),
                    thinking: Some(thinking),
                }))
            }
        }
        // tool_search_call, web_search_call, …: no ka mapping yet.
        _ => Ok(None),
    }
}

/// Parse a whole Codex rollout: file order is transcript order. `event_msg`
/// lines (which restate the same user/assistant text as `response_item`)
/// and envelope lines are skipped with one aggregated note.
pub fn parse_codex(text: &str) -> Result<ParseOutcome, String> {
    let mut mapped = Vec::new();
    let mut skipped = 0usize;
    let mut notes: Vec<String> = Vec::new();
    let mut corrupt = 0usize;
    let mut envelope = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_codex_line(line) {
            Ok(Some(record)) => mapped.push(record),
            Ok(None) => envelope += 1,
            Err(_) => corrupt += 1,
        }
    }
    if corrupt > 0 {
        skipped += corrupt;
        notes.push(format!("{corrupt} corrupt line(s) skipped"));
    }
    if envelope > 0 {
        skipped += envelope;
        notes.push(format!(
            "{envelope} envelope/duplicate line(s) skipped (session_meta, turn_context, event_msg, …)"
        ));
    }
    Ok(ParseOutcome {
        mapped,
        skipped,
        notes,
    })
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Append a block's text field (`text`, `thinking`, …) to `out`, joining
/// multiple blocks with newlines. Non-string values stringify.
fn push_field_text(block: &serde_json::Value, field: &str, out: &mut String) {
    if !out.is_empty() {
        out.push('\n');
    }
    match block.get(field) {
        Some(serde_json::Value::String(s)) => out.push_str(s),
        Some(v) => out.push_str(&v.to_string()),
        None => {}
    }
}

/// Flatten a Claude `tool_result` block's content: a string, or a list of
/// text blocks joined by newlines (other block types are dropped).
fn block_content_text(block: &serde_json::Value) -> String {
    match block.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(blocks)) => {
            let mut text = String::new();
            for b in blocks {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    push_field_text(b, "text", &mut text);
                }
            }
            text
        }
        _ => String::new(),
    }
}

/// Extract text from a Codex `function_call_output` payload. `output` is
/// either plain text or a JSON-encoded envelope
/// (`{"output": "...", "metadata": …}`) — the inner `output` field wins
/// when present; objects without it serialize compactly.
fn codex_output_text(output: Option<&serde_json::Value>) -> String {
    match output {
        Some(serde_json::Value::String(s)) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(serde_json::Value::Object(inner)) => inner
                .get("output")
                .map(|v| codex_output_text(Some(v)))
                .unwrap_or_else(|| s.clone()),
            _ => s.clone(),
        },
        Some(serde_json::Value::Object(inner)) => inner
            .get("output")
            .map(|v| codex_output_text(Some(v)))
            .unwrap_or_else(|| serde_json::Value::Object(inner.clone()).to_string()),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod session_import_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn claude_assistant(uuid: &str, parent: Option<&str>, text: &str) -> String {
        let parent_json = match parent {
            Some(p) => format!("\"{p}\""),
            None => "null".to_string(),
        };
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":{parent_json},"timestamp":"2026-01-01T00:00:00Z","sessionId":"s1","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    fn claude_user(uuid: &str, parent: Option<&str>, text: &str) -> String {
        let parent_json = match parent {
            Some(p) => format!("\"{p}\""),
            None => "null".to_string(),
        };
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":{parent_json},"timestamp":"2026-01-01T00:00:01Z","sessionId":"s1","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    #[test]
    fn claude_orders_by_parent_uuid_with_shuffled_lines() {
        // File order: reply, root, follow-up — the parentUuid chain is the
        // only source of truth for conversation order.
        let text = [
            claude_assistant("b", Some("a"), "hi there"),
            claude_user("a", None, "hello"),
            claude_user("c", Some("b"), "one more"),
        ]
        .join("\n");
        let out = parse_claude(&text).unwrap();
        let roles: Vec<Role> = out.mapped.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant, Role::User]);
        assert_eq!(out.mapped[0].content, "hello");
        assert_eq!(out.mapped[2].content, "one more");
        assert_eq!(out.skipped, 0);
        assert!(out.notes.is_empty(), "{:?}", out.notes);
    }

    #[test]
    fn claude_mixed_blocks_map_to_calls_results_and_thinking() {
        let assistant = r#"{"type":"assistant","uuid":"a2","parentUuid":"a1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"let me check"},{"type":"text","text":"running ls"},{"type":"tool_use","id":"tu_1","name":"bash","input":{"command":["ls"]}},{"type":"image","source":{"type":"base64"}}]}}"#;
        let tool_out = r#"{"type":"user","uuid":"a3","parentUuid":"a2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"file.txt","is_error":false},{"type":"text","text":"here you go"}]}}"#;
        let corrupt = "{nope";
        let summary = r#"{"type":"summary","summary":"a chat","leafUuid":"a3"}"#;
        let text = [assistant, tool_out, corrupt, summary].join("\n");
        let out = parse_claude(&text).unwrap();
        assert_eq!(out.mapped.len(), 2, "{:?}", out.mapped);
        let a = &out.mapped[0];
        assert_eq!(a.role, Role::Assistant);
        assert_eq!(a.content, "running ls");
        assert_eq!(a.thinking.as_deref(), Some("let me check"));
        assert_eq!(a.calls.len(), 1);
        assert_eq!(a.calls[0].tool, "bash");
        assert_eq!(a.calls[0].arguments["command"][0], "ls");
        let t = &out.mapped[1];
        assert_eq!(t.role, Role::Tool);
        assert_eq!(t.results[0].call_id, "tu_1");
        assert_eq!(t.results[0].content, "file.txt");
        assert!(!t.results[0].is_error);
        assert_eq!(t.content, "here you go");
        // corrupt + summary lines; the image block rides as a note
        assert_eq!(out.skipped, 2, "{out:?}");
        assert!(
            out.notes
                .iter()
                .any(|n| n.contains("corrupt") && n.contains('1')),
            "{:?}",
            out.notes
        );
        assert!(out.notes.iter().any(|n| n.contains("non-text block")));
    }

    #[test]
    fn codex_payloads_map_to_messages_calls_results_thinking() {
        let lines = [
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"c1","cwd":"/tmp"}}"#,
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"list files"}]}}"#,
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"need a shell"}]}}"#,
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"function_call","call_id":"fc1","name":"shell","arguments":"{\"command\":[\"ls\"]}"}}"#,
            r#"{"timestamp":"2026-01-01T00:00:04Z","type":"response_item","payload":{"type":"function_call_output","call_id":"fc1","output":"a.txt"}}"#,
            r#"{"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done: a.txt"}]}}"#,
            r#"{"timestamp":"2026-01-01T00:00:06Z","type":"event_msg","payload":{"type":"agent_message","message":"done: a.txt"}}"#,
            r#"{"timestamp":"2026-01-01T00:00:07Z","type":"turn_context","payload":{"cwd":"/tmp"}}"#,
            "not json at all",
        ]
        .join("\n");
        let out = parse_codex(&lines).unwrap();
        assert_eq!(out.mapped.len(), 5, "{:?}", out.mapped);
        assert_eq!(out.mapped[0].role, Role::User);
        assert_eq!(out.mapped[0].content, "list files");
        assert_eq!(out.mapped[1].thinking.as_deref(), Some("need a shell"));
        let call = &out.mapped[2];
        assert_eq!(call.calls[0].tool, "shell");
        assert_eq!(call.calls[0].arguments["command"][0], "ls");
        let result = &out.mapped[3];
        assert_eq!(result.results[0].call_id, "fc1");
        assert_eq!(result.results[0].content, "a.txt");
        assert_eq!(out.mapped[4].role, Role::Assistant);
        assert_eq!(out.mapped[4].content, "done: a.txt");
        // meta + turn_context + event_msg + corrupt line
        assert_eq!(out.skipped, 4, "{out:?}");
        assert!(out.notes.iter().any(|n| n.contains("event_msg")));
        assert!(out.notes.iter().any(|n| n.contains("corrupt")));
    }

    #[test]
    fn codex_output_envelope_extraction() {
        // output wrapped in Codex's JSON envelope: inner `output` wins
        let wrapped: serde_json::Value =
            serde_json::from_str(r#"{"output":"a.txt","metadata":{"exit_code":0}}"#).unwrap();
        assert_eq!(
            codex_output_text(Some(&serde_json::json!({ "output": wrapped }))),
            "a.txt"
        );
        // plain text passes through untouched
        assert_eq!(
            codex_output_text(Some(&serde_json::Value::String("raw text".into()))),
            "raw text"
        );
        assert_eq!(codex_output_text(None), "");
    }

    #[test]
    fn unknown_format_errors_and_corrupt_lines_never_abort() {
        let err = import_session(
            "chatgpt",
            Path::new("/nonexistent.jsonl"),
            Path::new("/tmp"),
        )
        .unwrap_err();
        assert!(err.contains("unknown format") && err.contains("claude") && err.contains("codex"));
        // corrupt lines are counted (blank lines are ignored, not counted)
        let out = parse_claude("{{{broken\n\n}\n").unwrap();
        assert!(out.mapped.is_empty());
        assert_eq!(out.skipped, 2);
        assert!(out.notes.iter().any(|n| n.contains("corrupt")));
    }

    #[test]
    fn import_session_writes_a_strand_in_chain_order() {
        let dir = std::env::temp_dir().join(format!("ka-import-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        ka_strand::set_data_dir_for_tests(dir.join("data"));
        let src = dir.join("session.jsonl");
        // deliberately shuffled: the reply line comes first on disk
        std::fs::write(
            &src,
            [
                claude_assistant("b", Some("a"), "answer"),
                claude_user("a", None, "question"),
            ]
            .join("\n"),
        )
        .unwrap();
        let report = import_session("claude", &src, &dir).unwrap();
        assert_eq!(report.mapped, 2);
        assert_eq!(report.records, 3, "context line + 2 messages");
        assert_eq!(report.skipped, 0);
        // privacy: no message content in the report
        let rendered = format!("{report:?}");
        assert!(!rendered.contains("question") && !rendered.contains("answer"));
        let records = ka_strand::read(&report.path).unwrap();
        assert!(matches!(records[0], ka_strand::Record::Header { .. }));
        let ka_strand::Record::Message { content, .. } = &records[1] else {
            panic!("expected context message, got {:?}", records[1]);
        };
        assert!(
            content.starts_with("[imported session] origin: claude"),
            "{content}"
        );
        assert!(content.contains("session.jsonl"));
        let roles: Vec<Role> = records[2..]
            .iter()
            .filter_map(|r| match r {
                ka_strand::Record::Message { role, .. } => Some(*role),
                _ => None,
            })
            .collect();
        // chain order, not file order
        assert_eq!(roles, vec![Role::User, Role::Assistant]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
