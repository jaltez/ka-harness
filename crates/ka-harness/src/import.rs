//! `ka import <format>` (roadmap 9.3 / CLI-08): one-shot conversion of
//! another agent's permission config into ka `[[rules]]` TOML.
//! Supported formats: `claude` (settings.json permission lists),
//! `codex` (config.toml permission profiles), `gemini` (settings.json
//! tool approval lists). `cursor` is recognized but rejected: cursor
//! rules files are agent instructions, not permissions, and ka does
//! not invent permissions from prose.
//!
//! Explicit and auditable — ka never live-reads another agent's config;
//! the conversion prints everything it will do (and everything it
//! skipped, with reasons), asks before writing, and validates the
//! final file through the strict-TOML parser before it lands.
//!
//! Semantics notes: claude evaluates deny-anywhere-beats-allow; ka is
//! first-match-wins — so imported rules are emitted deny → ask → allow
//! in that order, preserving the claude outcome. Because rules are
//! APPENDED, any pre-existing [[rules]] entries in the target file
//! still win over the imported ones (a WARNING is printed; the user's
//! existing rules are never reordered or rewritten). Untranslatable
//! entries (tools ka does not have) and restrictions that cannot be
//! carried (mcp tool patterns) are reported, never dropped silently.

use std::io::IsTerminal;
use std::path::PathBuf;

use ka_engine::config::{Config, Rule, Verdict};

/// Where the converted rules land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// `<project root>/.ka/ka.toml` (nearest .git ancestor, else cwd).
    Project,
    /// `~/.config/ka/ka.toml`.
    User,
}

impl Layer {
    fn label(self) -> &'static str {
        match self {
            Layer::Project => "project",
            Layer::User => "user",
        }
    }
}

/// Maximum input config file size (16 MiB).
const MAX_IMPORT_BYTES: u64 = 16 * 1024 * 1024;

fn read_bounded(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read as _;
    // the cap must ride the READ: `metadata().len()` is 0 for /dev/zero
    // and FIFOs, which would otherwise read forever
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_IMPORT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_IMPORT_BYTES {
        return Err(format!(
            "{}: file size exceeds {} byte import cap",
            path.display(),
            MAX_IMPORT_BYTES
        ));
    }
    String::from_utf8(bytes).map_err(|e| format!("{}: not UTF-8: {e}", path.display()))
}

/// Run `ka import <format> <path>`.
pub fn run(
    format: &str,
    path: &std::path::Path,
    layer: Option<Layer>,
    dry_run: bool,
) -> Result<std::process::ExitCode, String> {
    let (conversion, source) = match format {
        "claude" => {
            let text = read_bounded(path)?;
            (
                convert_claude(&text)?,
                "claude code settings.json (ka import claude)",
            )
        }
        "codex" => {
            let text = read_bounded(path)?;
            (convert_codex(&text)?, "codex config.toml (ka import codex)")
        }
        "gemini" => {
            let text = read_bounded(path)?;
            (
                convert_gemini(&text)?,
                "gemini settings.json (ka import gemini)",
            )
        }
        // Recognized on purpose: the error is the honest answer, not a
        // missing-feature message. No file read — the verdict does not
        // depend on the file's contents.
        "cursor" => return Err(convert_cursor()),
        other => {
            return Err(format!(
                "unknown import format {other:?} (supported: claude, codex, gemini, cursor)"
            ));
        }
    };
    print_conversion(&conversion);
    if conversion.rules.is_empty() {
        println!("nothing to import");
        return Ok(std::process::ExitCode::SUCCESS);
    }
    if dry_run {
        println!("\n(dry run — nothing written)");
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let layer = match layer {
        Some(l) => l,
        None => prompt_layer()?,
    };
    let target = match layer {
        Layer::Project => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            ka_engine::project_root(&cwd).join(".ka/ka.toml")
        }
        Layer::User => ka_engine::config::user_config_path(),
    };
    write_rules(&target, &conversion, source)?;
    println!(
        "\nwrote {} rule(s) → the {} layer ({})",
        conversion.rules.len(),
        layer.label(),
        target.display()
    );
    Ok(std::process::ExitCode::SUCCESS)
}

/// A conversion result: the rules to append, the entries that were
/// skipped (entry, reason) so nothing disappears silently, and
/// informational notes (config ka carries differently — mode, sandbox,
/// network policy — suggested, never applied).
#[derive(Debug)]
pub struct Conversion {
    pub rules: Vec<Rule>,
    pub skipped: Vec<(String, String)>,
    pub notes: Vec<String>,
}

/// Parse a Claude Code settings.json and convert its permission lists.
fn convert_claude(text: &str) -> Result<Conversion, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let perms = v
        .get("permissions")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let lists = [
        ("allow", Verdict::Allow),
        ("ask", Verdict::Ask),
        ("deny", Verdict::Deny),
    ];
    // claude: deny anywhere beats allow everywhere; ka: first match
    // wins — emit deny first, then ask, then allow to preserve outcomes
    let ordered = [lists[2], lists[1], lists[0]];
    let mut rules = Vec::new();
    let mut skipped = Vec::new();
    for (list, verdict) in ordered {
        let Some(entries) = perms.get(list).and_then(|l| l.as_array()).map(|a| {
            a.iter()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        }) else {
            continue;
        };
        for entry in entries {
            match map_entry(&entry, verdict) {
                Ok(rule) => rules.push(rule),
                Err(reason) => skipped.push((entry.clone(), reason)),
            }
        }
    }
    Ok(Conversion {
        rules,
        skipped,
        notes: Vec::new(),
    })
}

/// Split `Tool(pattern)` / bare `Tool` expressions (claude and gemini
/// share the shape).
fn split_tool_expr(entry: &str) -> (String, Option<String>) {
    match entry.split_once('(') {
        Some((tool, rest)) => {
            let inner = rest.strip_suffix(')').unwrap_or(rest);
            (tool.to_string(), Some(inner.to_string()))
        }
        None => (entry.to_string(), None),
    }
}

/// Map one claude permission entry to a ka rule; `Err` carries the
/// skip reason (reported, never dropped silently).
fn map_entry(entry: &str, verdict: Verdict) -> Result<Rule, String> {
    let (tool_expr, pattern) = split_tool_expr(entry);
    let rule_for = |tool: &str, pattern: Option<String>| {
        Ok(Rule {
            tool: tool.to_string(),
            pattern,
            verdict,
        })
    };
    match tool_expr.as_str() {
        // Bash(pattern): claude `prefix:*` is prefix-match → ka `prefix*`;
        // other globs pass through as ka globs. Empty patterns like
        // `Bash("")` or `Bash()` are rejected instead of widening to allow-all.
        "Bash" => {
            // An empty pattern on an `allow` entry would widen to
            // allow-all (refuse, reported); on deny/ask it is a
            // legitimate scope-less rule and must be KEPT.
            let empty = |entry: &str| -> Result<Rule, String> {
                if verdict == Verdict::Allow {
                    Err(format!(
                        "empty pattern in {entry:?} — refusing to widen to allow-all"
                    ))
                } else {
                    rule_for("bash", None)
                }
            };
            let pattern = match pattern {
                None => None,
                Some(p) => {
                    let cleaned = p.trim().trim_matches('"').trim_matches('\'').to_string();
                    if cleaned.is_empty() {
                        return empty(entry);
                    }
                    let mapped = if let Some(prefix) = cleaned.strip_suffix(":*") {
                        if prefix.is_empty() {
                            return empty(entry);
                        }
                        // claude `git commit:*` = prefix match → ka `git commit*`
                        format!("{prefix}*")
                    } else if let Some(exact) = cleaned.strip_suffix(':') {
                        if exact.is_empty() {
                            return empty(entry);
                        }
                        // `git commit:` = the bare prefix itself
                        exact.to_string()
                    } else {
                        cleaned
                    };
                    Some(mapped)
                }
            };
            rule_for("bash", pattern)
        }
        "Edit" => rule_for("edit", clean_non_empty_pattern(entry, pattern, verdict)?),
        "Write" => rule_for("write", clean_non_empty_pattern(entry, pattern, verdict)?),
        "Read" => rule_for("read", clean_non_empty_pattern(entry, pattern, verdict)?),
        "WebFetch" => {
            // claude `WebFetch(domain:example.com)` → ka's web_fetch
            // domain semantics take the bare host. Reject `WebFetch(domain:)`.
            let pattern = match pattern {
                None => None,
                Some(p) => {
                    let trimmed = p.trim().trim_matches('"').trim_matches('\'');
                    let host = trimmed
                        .strip_prefix("domain:")
                        .unwrap_or(trimmed)
                        .trim()
                        .trim_matches('"')
                        .trim_matches('\'');
                    if host.is_empty() {
                        return if verdict == Verdict::Allow {
                            Err(format!(
                                "empty pattern in {entry:?} — refusing to widen to allow-all"
                            ))
                        } else {
                            rule_for("web_fetch", None)
                        };
                    }
                    Some(host.to_string())
                }
            };
            rule_for("web_fetch", pattern)
        }
        "WebSearch" => rule_for(
            "web_search",
            clean_non_empty_pattern(entry, pattern, verdict)?,
        ),
        // mcp__server__tool → ka's `<server>.<tool>` hand names; ka
        // mcp rules carry no pattern, so a claude restriction like
        // mcp__github__create_issue(args) is reported as skipped
        // rather than silently dropping the restriction
        other if other.starts_with("mcp__") => {
            let parts: Vec<&str> = other.trim_start_matches("mcp__").splitn(2, "__").collect();
            if parts.len() == 2 {
                if let Some(p) = clean_non_empty_pattern(entry, pattern, verdict)? {
                    Err(format!(
                        "ka mcp rules carry no pattern — {other:?} loses its ({p}) restriction"
                    ))
                } else {
                    rule_for(&format!("{}.{}", parts[0], parts[1]), None)
                }
            } else {
                Err(format!("unrecognized MCP tool form {entry:?}"))
            }
        }
        // tools ka does not carry (Task, Grep, Glob, ...) or unknown
        other => Err(format!("no ka counterpart for {other:?}")),
    }
}

/// One codex filesystem grant: a path/glob and its access kind.
struct CodexGrant {
    pattern: String,
    kind: String,
    /// `true` when the glob was scoped to codex `:workspace_roots`
    /// (relative paths; ka matches rule patterns against the tool's
    /// path argument, so these deserve a second look).
    workspace_relative: bool,
}

/// Parse a codex `config.toml` and convert the active permission
/// profile's filesystem grants (beta `default_permissions` +
/// `[permissions.<name>]`).
///
/// Mapping (verified against the codex configuration reference,
/// learn.chatgpt.com/docs/config-file/config-reference):
/// - `filesystem.<glob> = "write"` → allow rules on ka's file tools
///   (edit + write); `"read"` → allow read; `"deny"` (deny-read in
///   codex, and with deny > write > read precedence also unwritable)
///   → deny rules on read + write + edit so first-match-wins ka keeps
///   the path untouchable.
/// - `extends` chains are resolved (child overrides parent per path);
///   extending a builtin (`:read-only`, `:workspace`) keeps only the
///   profile's own entries, with a note.
/// - network domains are codex-proxy socket policy — not ka rules;
///   every entry is reported as skipped, plus an informational note.
/// - `approval_policy` / `sandbox_mode` are ka `mode` / `[sandbox]`
///   concerns — suggested in a note, never applied.
fn convert_codex(text: &str) -> Result<Conversion, String> {
    let v: toml::Value = toml::from_str(text).map_err(|e| format!("invalid TOML: {e}"))?;
    let mut rules = Vec::new();
    let mut skipped = Vec::new();
    let mut notes = Vec::new();

    // Approval/sandbox plumbing: ka equivalent is mode/[[rules]]/[sandbox].
    if let Some(p) = v.get("approval_policy") {
        let hint = match p.as_str() {
            Some("never") => "suggests mode = \"allow\"",
            Some("on-request") => "suggests ka's default mode = \"ask\"",
            _ => "ka carries this in mode = \"ask\"|\"accept-edits\"|\"allow\"|\"readonly\"",
        };
        notes.push(format!(
            "codex approval_policy = {} — not imported ({hint})",
            codex_scalar(p)
        ));
    }
    if let Some(p) = v.get("sandbox_mode") {
        let hint = match p.as_str() {
            Some("read-only") => "suggests mode = \"readonly\"",
            Some("workspace-write") => "suggests [sandbox] mode = \"fs\" + allow_write",
            Some("danger-full-access") => "has no safe ka equivalent ([sandbox] stays on)",
            _ => "ka carries this in [sandbox] mode = \"off\"|\"fs\"",
        };
        notes.push(format!(
            "codex sandbox_mode = {} — not imported ({hint})",
            codex_scalar(p)
        ));
    }
    if let Some(sw) = v.get("sandbox_workspace_write") {
        notes.push(format!(
            "codex [sandbox_workspace_write] ({}) — not imported; ka equivalent: [sandbox] allow_write = [...] (writable_roots), network_access has no ka rule equivalent",
            sw.as_table()
                .map(|t| t.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_else(|| "table".into())
        ));
    }
    if v.get("profiles").is_some() {
        notes.push(
            "codex [profiles.*] (legacy profile selection) not inspected — only beta [permissions.*] profiles convert".into(),
        );
    }

    match v.get("default_permissions").and_then(|d| d.as_str()) {
        Some(name) if name.starts_with(':') => {
            // Built-in profiles are sandbox presets, not glob lists.
            let hint = match name {
                ":read-only" => "suggests mode = \"readonly\"",
                ":workspace" => "suggests mode = \"accept-edits\" + [sandbox] confinement",
                ":danger-full-access" => "suggests mode = \"allow\" (keep [sandbox] on)",
                _ => "unknown codex builtin",
            };
            notes.push(format!(
                "codex default_permissions = {name:?} (builtin profile) — not imported as rules ({hint})"
            ));
        }
        Some(name) => {
            let grants = collect_codex_profile(&v, name, 0, &mut notes, &mut skipped)?;
            rules = codex_rules_from_grants(&grants, &mut notes);
        }
        None => {
            // only [permissions.<name>] SUBTABLES are profiles; a stray
            // scalar (a mis-nested `default_permissions = "workspace"`)
            // is not one
            let profiles = v
                .get("permissions")
                .and_then(|p| p.as_table())
                .map(|t| {
                    t.iter()
                        .filter(|(_, v)| v.is_table())
                        .map(|(k, _)| k.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            match profiles.len() {
                0 => notes.push(
                    "codex config defines no [permissions.*] profiles — nothing to convert; ka permissions live in mode / [[rules]] / [sandbox]".into(),
                ),
                1 => {
                    let name = profiles[0].clone();
                    notes.push(format!(
                        "codex default_permissions unset — importing the only defined profile {name:?}"
                    ));
                    let grants = collect_codex_profile(&v, &name, 0, &mut notes, &mut skipped)?;
                    rules = codex_rules_from_grants(&grants, &mut notes);
                }
                _ => {
                    for name in &profiles {
                        skipped.push((
                            format!("[permissions.{name}]"),
                            "not selected by codex default_permissions — only the active profile converts".into(),
                        ));
                    }
                }
            }
        }
    }
    Ok(Conversion {
        rules,
        skipped,
        notes,
    })
}

/// Flatten a codex `[permissions.<name>.filesystem]` table into grants.
/// Nested tables scope entries under a root path; `:workspace_roots`
/// sub-entries are workspace-relative globs.
fn collect_codex_fs(
    table: &toml::Table,
    prefix: Option<&str>,
    workspace_relative: bool,
    grants: &mut Vec<CodexGrant>,
    skipped: &mut Vec<(String, String)>,
    notes: &mut Vec<String>,
) {
    for (key, val) in table {
        match val {
            toml::Value::String(s) if matches!(s.as_str(), "read" | "write" | "deny") => {
                if key.starts_with(':') && prefix.is_none() {
                    skipped.push((
                        format!("filesystem {key} = {s}"),
                        "codex special token, not a path glob".into(),
                    ));
                } else {
                    grants.push(CodexGrant {
                        pattern: codex_join(prefix, key),
                        kind: s.clone(),
                        workspace_relative,
                    });
                }
            }
            toml::Value::String(other) => skipped.push((
                format!("filesystem {key} = {other}"),
                "expected \"read\", \"write\" or \"deny\"".into(),
            )),
            toml::Value::Table(sub) if key == ":workspace_roots" => {
                for (sub_key, sub_val) in sub {
                    match sub_val {
                        toml::Value::String(kind)
                            if matches!(kind.as_str(), "read" | "write" | "deny") =>
                        {
                            if sub_key == "." {
                                // Root-wide grant: ka's honest equivalent is
                                // the sandbox, not a patternless allow rule.
                                notes.push(format!(
                                    "codex :workspace_roots \".\" = {kind} (workspace-root-wide) — ka equivalent is [sandbox] allow_write / mode, not a rule"
                                ));
                            } else {
                                grants.push(CodexGrant {
                                    pattern: sub_key.clone(),
                                    kind: kind.clone(),
                                    workspace_relative: true,
                                });
                            }
                        }
                        other => skipped.push((
                            format!(
                                "filesystem \":workspace_roots\" {sub_key} = {}",
                                codex_scalar(other)
                            ),
                            "expected \"read\", \"write\" or \"deny\"".into(),
                        )),
                    }
                }
            }
            toml::Value::Table(sub) => {
                let joined = codex_join(prefix, key);
                collect_codex_fs(
                    sub,
                    Some(&joined),
                    workspace_relative,
                    grants,
                    skipped,
                    notes,
                );
            }
            other => skipped.push((
                format!("filesystem {key} = {}", codex_scalar(other)),
                "not a filesystem grant".into(),
            )),
        }
    }
}

fn codex_join(prefix: Option<&str>, key: &str) -> String {
    match prefix {
        Some(p) => format!("{p}/{key}"),
        None => key.to_string(),
    }
}

fn codex_scalar(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Resolve a named profile through its `extends` chain (child entries
/// override the parent's on the same path) and collect its grants.
fn collect_codex_profile(
    v: &toml::Value,
    name: &str,
    depth: usize,
    notes: &mut Vec<String>,
    skipped: &mut Vec<(String, String)>,
) -> Result<Vec<CodexGrant>, String> {
    if depth > 8 {
        return Err("cycle in codex [permissions.*] extends chain".into());
    }
    let Some(profile) = v
        .get("permissions")
        .and_then(|p| p.get(name))
        .and_then(|p| p.as_table())
    else {
        return Err(format!(
            "codex default_permissions points at {name:?} but no [permissions.{name}] table exists"
        ));
    };
    let mut grants = Vec::new();
    match profile.get("extends").and_then(|e| e.as_str()) {
        Some(parent) if parent.starts_with(':') => {
            notes.push(format!(
                "codex profile {name:?} extends builtin {parent} — builtin base semantics are sandbox presets, not glob rules; only this profile's filesystem entries are imported"
            ));
        }
        Some(parent) => {
            grants = collect_codex_profile(v, parent, depth + 1, notes, skipped)?;
        }
        None => {}
    }
    if let Some(roots) = profile.get("workspace_roots").and_then(|w| w.as_table()) {
        for (path, on) in roots {
            skipped.push((
                format!("[permissions.{name}.workspace_roots] {path} = {}", codex_scalar(on)),
                "defines codex workspace roots — ka has no rule equivalent ([sandbox]/mode carry confinement)".into(),
            ));
        }
    }
    // Child entries override the parent's on the same path; the
    // parent's remaining entries still apply (codex merges profiles).
    let child_start = grants.len();
    if let Some(fs) = profile.get("filesystem").and_then(|f| f.as_table()) {
        collect_codex_fs(fs, None, false, &mut grants, skipped, notes);
        let overridden: Vec<String> = grants[child_start..]
            .iter()
            .map(|g| g.pattern.clone())
            .collect();
        let mut merged = Vec::with_capacity(grants.len());
        for (i, g) in grants.drain(..).enumerate() {
            let dropped = i < child_start && overridden.contains(&g.pattern);
            if !dropped {
                merged.push(g);
            }
        }
        grants = merged;
    }
    if let Some(net) = profile.get("network") {
        collect_codex_network(name, net, skipped, notes);
    }
    Ok(grants)
}

/// Report codex network config: socket policy for the codex proxy —
/// no ka rule can carry it, so every domain entry is named.
fn collect_codex_network(
    profile: &str,
    net: &toml::Value,
    skipped: &mut Vec<(String, String)>,
    notes: &mut Vec<String>,
) {
    let Some(table) = net.as_table() else { return };
    let mut other_keys = Vec::new();
    for (key, val) in table {
        if matches!(key.as_str(), "domains" | "unix_sockets") {
            if let Some(entries) = val.as_table() {
                for (target, verdict) in entries {
                    skipped.push((
                        format!("[permissions.{profile}.network.{key}] {target} = {}", codex_scalar(verdict)),
                        "codex proxy socket policy — ka [[rules]] gate ka's own tools (bash/files/web), not network destinations".into(),
                    ));
                }
            }
        } else {
            other_keys.push(key.clone());
        }
    }
    if !other_keys.is_empty() {
        notes.push(format!(
            "codex [permissions.{profile}.network] keys ({}) — codex proxy plumbing, not per-tool permissions; not imported",
            other_keys.join(", ")
        ));
    }
    if table.contains_key("domains") {
        notes.push(
            "codex network domain rules cannot become ka rules — ka's web_fetch/web_search patterns gate ka's own web tools only, and bash networking is sandbox territory".into(),
        );
    }
}

/// Order grants deny → write → read (codex filesystem precedence is
/// deny > write > read; ka is first-match-wins).
fn codex_rules_from_grants(grants: &[CodexGrant], notes: &mut Vec<String>) -> Vec<Rule> {
    if grants.iter().any(|g| g.workspace_relative) {
        let rel: Vec<String> = grants
            .iter()
            .filter(|g| g.workspace_relative)
            .map(|g| g.pattern.clone())
            .collect();
        notes.push(format!(
            "workspace-root-relative globs ({}) imported as written — ka matches rule patterns against the tool's path argument; verify relative globs resolve the same way",
            rel.join(", ")
        ));
    }
    let mut rules = Vec::new();
    let mut push = |tool: &str, pattern: &str, verdict: Verdict| {
        rules.push(Rule {
            tool: tool.to_string(),
            pattern: Some(pattern.to_string()),
            verdict,
        });
    };
    for g in grants.iter().filter(|g| g.kind == "deny") {
        // codex "deny" denies reads and, with deny > write > read
        // precedence, leaves no write grant — deny all three ka file
        // tools so the path stays untouchable under first-match-wins.
        for tool in ["read", "write", "edit"] {
            push(tool, &g.pattern, Verdict::Deny);
        }
    }
    for g in grants.iter().filter(|g| g.kind == "write") {
        push("edit", &g.pattern, Verdict::Allow);
        push("write", &g.pattern, Verdict::Allow);
    }
    for g in grants.iter().filter(|g| g.kind == "read") {
        push("read", &g.pattern, Verdict::Allow);
    }
    rules
}

fn clean_non_empty_pattern(
    entry: &str,
    pattern: Option<String>,
    verdict: Verdict,
) -> Result<Option<String>, String> {
    match pattern {
        None => Ok(None),
        Some(p) => {
            let cleaned = p.trim().trim_matches('"').trim_matches('\'').to_string();
            if cleaned.is_empty() {
                // a scope-less `allow` would widen to allow-all — refuse.
                // A scope-less deny/ask is a legitimate "all calls" rule:
                // keep the restriction instead of dropping it.
                if verdict == Verdict::Allow {
                    Err(format!(
                        "empty pattern in {entry:?} — refusing to widen to allow-all"
                    ))
                } else {
                    Ok(None)
                }
            } else {
                Ok(Some(cleaned))
            }
        }
    }
}

/// Parse a gemini-cli `settings.json` and convert its tool-approval
/// lists (verified against google-gemini/gemini-cli's
/// `packages/cli/src/config/settingsSchema.ts` — current keys are
/// `tools.allowed`, `tools.confirmationRequired`, `tools.exclude`;
/// there is no `allowedTools`/`toolApproval` setting).
///
/// Mapping: gemini checks `tools.exclude` first (blocklist), then
/// `tools.confirmationRequired` over `tools.allowed` — ka is
/// first-match-wins, so rules are emitted deny → ask → allow.
/// `run_shell_command(<prefix>)` entries are prefix matches in gemini
/// → ka glob `prefix*` (same looseness as claude's `prefix:*`).
/// Availability knobs (`tools.core`, `mcp.allowed/excluded`) and
/// auto-accept modes (`general.defaultApprovalMode`) are not
/// permissions — reported as notes.
fn convert_gemini(text: &str) -> Result<Conversion, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let mut rules = Vec::new();
    let mut skipped = Vec::new();
    let mut notes = Vec::new();
    let tools = v.get("tools");
    // gemini precedence: exclude (deny) > confirmationRequired (ask)
    // > allowed (allow) — emit in that order for first-match-wins ka.
    let lists = [
        ("exclude", Verdict::Deny),
        ("confirmationRequired", Verdict::Ask),
        ("allowed", Verdict::Allow),
    ];
    for (key, verdict) in lists {
        let Some(entries) = tools
            .and_then(|t| t.get(key))
            .and_then(|l| l.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
        else {
            continue;
        };
        for entry in entries {
            match map_gemini_entry(&entry, verdict) {
                Ok(rule) => rules.push(rule),
                Err(reason) => skipped.push((entry.clone(), reason)),
            }
        }
    }
    if tools
        .and_then(|t| t.get("core"))
        .and_then(|c| c.as_array())
        .is_some()
    {
        notes.push(
            "gemini tools.core is an availability allowlist (only those built-in tools load at all), not per-call permission — ka equivalent (deny rules for everything not listed) is not invented here".into(),
        );
    }
    if let Some(mode) = v
        .pointer("/general/defaultApprovalMode")
        .and_then(|m| m.as_str())
    {
        let ka_mode = match mode {
            "default" => "ask",
            "auto_edit" => "accept-edits",
            "plan" => "readonly",
            _ => "ask",
        };
        notes.push(format!(
            "gemini general.defaultApprovalMode = {mode:?} — auto-accept knob, not a rule; ka equivalent: mode = {ka_mode:?} in .ka/ka.toml (not applied; gemini yolo is CLI-only)"
        ));
    }
    if v.pointer("/security/toolSandboxing") == Some(&serde_json::Value::Bool(true)) {
        notes.push(
            "gemini security.toolSandboxing — ka equivalent: [sandbox] mode = \"fs\" (+ allow_write); not applied".into(),
        );
    }
    if v.get("sandbox").is_some() || tools.and_then(|t| t.get("sandbox")).is_some() {
        notes.push(
            "gemini sandbox / sandboxNetworkAccess / sandboxAllowedPaths — ka equivalent: [sandbox] mode = \"fs\", allow_write; network access has no ka rule equivalent; not applied".into(),
        );
    }
    if v.pointer("/mcp/allowed").is_some() || v.pointer("/mcp/excluded").is_some() {
        notes.push(
            "gemini mcp.allowed / mcp.excluded choose which MCP servers load at all; ka gates loaded mcp tools per call via [[rules]] tool = \"<server>.<tool>\" — translate manually if you want the same cut".into(),
        );
    }
    Ok(Conversion {
        rules,
        skipped,
        notes,
    })
}

/// Map one gemini tool-approval entry to a ka rule; `Err` carries the
/// skip reason (reported, never dropped silently).
fn map_gemini_entry(entry: &str, verdict: Verdict) -> Result<Rule, String> {
    let (tool_expr, pattern) = split_tool_expr(entry);
    let rule_for = |tool: &str, pattern: Option<String>| {
        Ok(Rule {
            tool: tool.to_string(),
            pattern,
            verdict,
        })
    };
    match tool_expr.as_str() {
        // gemini `run_shell_command(git)` is a prefix match (`git`,
        // `git status`, ...) → ka glob `git*`
        "run_shell_command" | "shell" => {
            let pattern =
                clean_non_empty_pattern(entry, pattern, verdict)?.map(|p| format!("{p}*"));
            rule_for("bash", pattern)
        }
        "replace" => rule_for("edit", clean_non_empty_pattern(entry, pattern, verdict)?),
        "write_file" => rule_for("write", clean_non_empty_pattern(entry, pattern, verdict)?),
        "read_file" | "read_many_files" => {
            rule_for("read", clean_non_empty_pattern(entry, pattern, verdict)?)
        }
        "web_fetch" => rule_for(
            "web_fetch",
            clean_non_empty_pattern(entry, pattern, verdict)?,
        ),
        "web_search" => rule_for(
            "web_search",
            clean_non_empty_pattern(entry, pattern, verdict)?,
        ),
        // tools ka does not gate (glob, grep_search, list_directory,
        // save_memory, ...) or unknown
        other => Err(format!("no ka counterpart for {other:?}")),
    }
}

/// Cursor rules files are agent instructions, not permissions — there
/// is nothing honest to convert into [[rules]], so this fails loudly
/// instead of inventing permissions from prose.
fn convert_cursor() -> String {
    "cursor rules (.cursorrules, .cursor/rules/*.mdc) are agent instructions — how the model should behave — not permission rules; nothing to map onto ka [[rules]], and ka does not invent permissions from prose. ka already picks up instructions today: AGENTS.override.md > AGENTS.md > CLAUDE.md per directory, plus any project_doc_fallback_filenames you set in .ka/ka.toml (e.g. [\".cursorrules\"]), and glob-scoped markdown rules in .ka/rules/*.md. Nothing was imported."
        .to_string()
}

/// Print the conversion: every rule as it will land, then skips.
fn print_conversion(c: &Conversion) {
    println!("converted rules (emitted deny → ask → allow; first match wins):");
    for r in &c.rules {
        match &r.pattern {
            Some(p) => println!("  {}({}) = {:?}", r.tool, p, verdict_str(r.verdict)),
            None => println!("  {} = {:?}", r.tool, verdict_str(r.verdict)),
        }
    }
    if !c.skipped.is_empty() {
        println!("\nskipped (no ka counterpart — reported, never dropped silently):");
        for (entry, reason) in &c.skipped {
            println!("  {entry} — {reason}");
        }
    }
    if !c.notes.is_empty() {
        println!("\nnotes (ka carries these differently — suggested, not applied):");
        for note in &c.notes {
            println!("  {note}");
        }
    }
}

fn verdict_str(v: Verdict) -> &'static str {
    match v {
        Verdict::Allow => "allow",
        Verdict::Ask => "ask",
        Verdict::Deny => "deny",
    }
}

/// Interactively choose the target layer (TTY only; non-TTY callers
/// must pass --project/--user).
fn prompt_layer() -> Result<Layer, String> {
    if !std::io::stdin().is_terminal() {
        return Err("choose a layer with --project or --user (no TTY to ask)".to_string());
    }
    loop {
        print!("write to [p]roject .ka/ka.toml, [u]ser config, or [n]othing? ");
        use std::io::Write;
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| format!("stdin: {e}"))?;
        match line.trim() {
            "p" | "project" => return Ok(Layer::Project),
            "u" | "user" => return Ok(Layer::User),
            "n" | "no" | "" => return Err("aborted — nothing written".to_string()),
            _ => continue,
        }
    }
}

/// Append the rules to `target`, preserving existing content, and
/// re-validate the whole file through the strict parser before the
/// write lands (a file that would not parse is never written).
/// `source` names the origin format in the appended comment.
fn write_rules(
    target: &std::path::Path,
    conversion: &Conversion,
    source: &str,
) -> Result<(), String> {
    let existing = match std::fs::read_to_string(target) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {}: {e}", target.display())),
    };
    if existing.contains("[[rules]]") {
        // ka is first-match-wins and these rules are appended, so any
        // pre-existing [[rules]] entry beats them. Warn; never reorder
        // or rewrite the user's existing rules.
        println!(
            "\nWARNING: {target} already has [[rules]] entries; ka is first-match-wins, so your existing rules win over the appended ones",
            target = target.display()
        );
    }
    let mut text = existing.clone();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("\n# imported from {source}\n"));
    for r in &conversion.rules {
        text.push_str("\n[[rules]]\n");
        text.push_str(&format!("tool = {}\n", toml_quote(&r.tool)));
        if let Some(p) = &r.pattern {
            text.push_str(&format!("pattern = {}\n", toml_quote(p)));
        }
        text.push_str(&format!("verdict = \"{}\"\n", verdict_str(r.verdict)));
    }
    // strict validation before writing: the merged file must parse
    Config::parse_layer(&text, &target.display().to_string()).map_err(|e| e.to_string())?;
    let parent = target.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("ka.toml"),
        std::process::id()
    ));
    std::fs::write(&tmp, &text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "rename {} → {}: {e}",
            tmp.display(),
            target.display()
        ));
    }
    Ok(())
}

fn toml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const SAMPLE: &str = r#"{
      "permissions": {
        "allow": [
          "Bash(npm run *)",
          "Bash(git commit:*)",
          "Edit(src/**)",
          "Read(./.env)",
          "WebFetch(domain:example.com)",
          "mcp__github__create_issue",
          "Task"
        ],
        "ask": ["Bash(cargo publish*)"],
        "deny": ["Read(./secrets/*)", "Bash(rm -rf *)"]
      }
    }"#;

    #[test]
    fn converts_rules_preserving_claude_precedence() {
        let c = convert_claude(SAMPLE).unwrap();
        // deny first, then ask, then allow
        let order: Vec<Verdict> = c.rules.iter().map(|r| r.verdict).collect();
        let first_allow = order.iter().position(|v| *v == Verdict::Allow).unwrap();
        let last_deny = order.iter().rposition(|v| *v == Verdict::Deny).unwrap();
        let ask_pos = order.iter().position(|v| *v == Verdict::Ask).unwrap();
        assert!(last_deny < ask_pos && ask_pos < first_allow, "{order:?}");
        // pattern mapping: prefix:* → prefix*, domain: passthrough
        let find = |tool: &str, pat: Option<&str>| {
            c.rules
                .iter()
                .find(|r| r.tool == tool && r.pattern.as_deref() == pat)
        };
        assert!(find("bash", Some("git commit*")).is_some(), "{:?}", c.rules);
        assert!(find("bash", Some("npm run *")).is_some());
        assert!(find("bash", Some("rm -rf *")).is_some());
        assert!(find("edit", Some("src/**")).is_some());
        assert!(find("read", Some("./secrets/*")).is_some());
        assert!(find("web_fetch", Some("example.com")).is_some());
        assert!(find("github.create_issue", None).is_some());
        // untranslatable reported, not dropped
        assert!(
            c.skipped.iter().any(|(e, _)| e == "Task"),
            "{:?}",
            c.skipped
        );
    }

    #[test]
    fn writes_strict_validating_layer_and_preserves_existing() {
        let dir = std::env::temp_dir().join(format!("ka-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("ka.toml");
        std::fs::write(&target, "model = \"a/x\"\n").unwrap();
        let c = convert_claude(SAMPLE).unwrap();
        write_rules(&target, &c, "claude code settings.json (ka import claude)").unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        // existing keys survive, new rules parse back
        let layer = Config::parse_layer(&text, "check").unwrap();
        assert_eq!(layer.model.as_deref(), Some("a/x"));
        assert!(layer.rules.len() >= 8, "{}", layer.rules.len());
        assert!(text.contains("# imported from claude code settings.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn mcp_pattern_restrictions_are_reported_not_dropped() {
        let c = convert_claude(
            r#"{"permissions": {"allow": [
                "mcp__github__create_issue",
                "mcp__github__list_issues(repo:acme/*)"
            ]}}"#,
        )
        .unwrap();
        // the bare tool converts; the restricted one is skipped with the
        // dropped restriction named — never silently converted
        assert!(c.rules.iter().any(|r| r.tool == "github.create_issue"));
        let skipped: Vec<_> = c.skipped.iter().map(|(e, _)| e.as_str()).collect();
        assert!(
            skipped.contains(&"mcp__github__list_issues(repo:acme/*)"),
            "{skipped:?}"
        );
        assert!(
            c.skipped
                .iter()
                .any(|(e, r)| e.contains("list_issues") && r.contains("acme/*")),
            "{:?}",
            c.skipped
        );
    }

    #[test]
    fn appending_after_existing_rules_warns_and_keeps_them_first() {
        let dir = std::env::temp_dir().join(format!("ka-import-warn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("ka.toml");
        // a pre-existing allow rule: first-match-wins beats the appended deny
        std::fs::write(
            &target,
            "[[rules]]\ntool = \"bash\"\npattern = \"rm *\"\nverdict = \"allow\"\n",
        )
        .unwrap();
        let c = convert_claude(SAMPLE).unwrap();
        write_rules(&target, &c, "claude code settings.json (ka import claude)").unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        // the user's rule stays ahead of the appended block
        let existing = text.find("pattern = \"rm *\"").unwrap();
        let appended = text.find("# imported from claude").unwrap();
        assert!(existing < appended, "{text}");
        let layer = Config::parse_layer(&text, "check").unwrap();
        assert!(layer.rules.len() >= 9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_rejects_unknown_formats_and_bad_json() {
        // unknown format: the error names every supported format
        let err = run("windsurf", std::path::Path::new("/nonexistent"), None, true)
            .expect_err("unknown format must fail");
        for supported in ["claude", "codex", "gemini", "cursor"] {
            assert!(err.contains(supported), "{err}");
        }
        // supported formats still fail on a missing file
        assert!(run("codex", std::path::Path::new("/nonexistent"), None, true).is_err());
        assert!(convert_claude("{nope").is_err());
        // empty settings: nothing to do, no error
        let c = convert_claude("{}").unwrap();
        assert!(c.rules.is_empty() && c.skipped.is_empty() && c.notes.is_empty());
    }

    // ---- codex ---------------------------------------------------------

    const CODEX_SAMPLE: &str = r#"
model = "gpt-6.1-sol"
approval_policy = "on-request"
sandbox_mode = "workspace-write"
default_permissions = "work"

[sandbox_workspace_write]
network_access = true

[permissions.work]
extends = ":workspace"

[permissions.work.filesystem]
"/repo/**" = "write"
"/repo/docs" = "read"
"/repo/.env" = "deny"
":minimal" = "read"

[permissions.work.filesystem.":workspace_roots"]
"**/*.env" = "deny"

[permissions.work.network]
enabled = true
proxy_url = "http://localhost:9876"

[permissions.work.network.domains]
"api.example.com" = "allow"

[permissions.work.workspace_roots]
"/opt/data" = true
"#;

    #[test]
    fn codex_filesystem_grants_convert_in_codex_precedence() {
        let c = convert_codex(CODEX_SAMPLE).unwrap();
        let find = |tool: &str, pat: Option<&str>, verdict: Verdict| {
            c.rules
                .iter()
                .any(|r| r.tool == tool && r.pattern.as_deref() == pat && r.verdict == verdict)
        };
        // deny first (codex deny > write > read; ka first-match-wins)
        assert_eq!(c.rules.first().map(|r| r.verdict), Some(Verdict::Deny));
        assert!(find("read", Some("/repo/.env"), Verdict::Deny));
        assert!(find("write", Some("/repo/.env"), Verdict::Deny));
        assert!(find("edit", Some("/repo/.env"), Verdict::Deny));
        // write grants cover both ka file tools
        assert!(find("edit", Some("/repo/**"), Verdict::Allow));
        assert!(find("write", Some("/repo/**"), Verdict::Allow));
        assert!(
            !c.rules.iter().any(|r| r.tool == "bash"
                || (r.tool == "read" && r.pattern.as_deref() == Some("/repo/**")))
        );
        // read grant
        assert!(find("read", Some("/repo/docs"), Verdict::Allow));
        // workspace-root-relative deny carried with a verification note
        assert!(find("read", Some("**/*.env"), Verdict::Deny));
        assert!(
            c.notes
                .iter()
                .any(|n| n.contains("workspace-root-relative") && n.contains("**/*.env"))
        );
        // special token reported, not converted
        assert!(
            c.skipped
                .iter()
                .any(|(e, _)| e.contains(":minimal") && e.contains("read"))
        );
        // network domains are socket policy — named and skipped
        assert!(
            c.skipped
                .iter()
                .any(|(e, _)| e.contains("api.example.com") && e.contains("network.domains")),
            "{:?}",
            c.skipped
        );
        assert!(
            c.notes
                .iter()
                .any(|n| n.contains("network domain rules cannot become ka rules"))
        );
        // proxy plumbing summarized in a note
        assert!(c.notes.iter().any(|n| n.contains("proxy_url")));
        // workspace_roots definitions skipped with the sandbox pointer
        assert!(
            c.skipped
                .iter()
                .any(|(e, _)| e.contains("/opt/data") && e.contains("workspace_roots"))
        );
        // extends a builtin: own entries only, builtin noted
        assert!(
            c.notes
                .iter()
                .any(|n| n.contains("extends builtin :workspace"))
        );
        // approval/sandbox knobs are notes suggesting ka equivalents
        assert!(
            c.notes
                .iter()
                .any(|n| n.contains("approval_policy") && n.contains("mode = \"ask\""))
        );
        assert!(c.notes.iter().any(|n| n.contains("sandbox_mode")));
        assert!(c.notes.iter().any(|n| n.contains("allow_write")));
        // strict write path works end to end
        let dir = std::env::temp_dir().join(format!("ka-import-codex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("ka.toml");
        write_rules(&target, &c, "codex config.toml (ka import codex)").unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("# imported from codex config.toml"));
        let layer = Config::parse_layer(&text, "check").unwrap();
        assert!(layer.rules.len() >= 8, "{}", layer.rules.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_extends_resolves_with_child_overrides() {
        let cfg = r#"
default_permissions = "child"

[permissions.parent]
[permissions.parent.filesystem]
"/a" = "read"
"/b" = "read"

[permissions.child]
extends = "parent"
[permissions.child.filesystem]
"/a" = "write"
"#;
        let c = convert_codex(cfg).unwrap();
        let find = |tool: &str, pat: Option<&str>, verdict: Verdict| {
            c.rules
                .iter()
                .any(|r| r.tool == tool && r.pattern.as_deref() == pat && r.verdict == verdict)
        };
        // child overrides the parent's "/a" (write beats read)
        assert!(find("edit", Some("/a"), Verdict::Allow));
        assert!(find("write", Some("/a"), Verdict::Allow));
        assert!(!find("read", Some("/a"), Verdict::Allow));
        // parent's untouched "/b" still applies
        assert!(find("read", Some("/b"), Verdict::Allow));
    }

    #[test]
    fn codex_extends_cycles_and_missing_profiles_fail_loudly() {
        let cycle = r#"
default_permissions = "a"
[permissions.a]
extends = "b"
[permissions.a.filesystem]
"/x" = "read"
[permissions.b]
extends = "a"
[permissions.b.filesystem]
"/y" = "read"
"#;
        let err = convert_codex(cycle).expect_err("cycle must fail");
        assert!(err.contains("cycle"), "{err}");
        let missing = "default_permissions = \"ghost\"\n";
        let err = convert_codex(missing).expect_err("missing profile must fail");
        assert!(
            err.contains("ghost") && err.contains("no [permissions.ghost]"),
            "{err}"
        );
    }

    #[test]
    fn codex_without_default_permissions_selects_deterministically() {
        // two profiles, none selected: both reported, nothing invented
        let multi = r#"
[permissions.one]
[permissions.one.filesystem]
"/a" = "read"
[permissions.two]
[permissions.two.filesystem]
"/b" = "read"
"#;
        let c = convert_codex(multi).unwrap();
        assert!(c.rules.is_empty());
        assert_eq!(c.skipped.len(), 2, "{:?}", c.skipped);
        // a single profile is imported with a note
        let single = r#"
[permissions.solo]
[permissions.solo.filesystem]
"/a" = "read"
"#;
        let c = convert_codex(single).unwrap();
        assert_eq!(c.rules.len(), 1);
        assert!(c.notes.iter().any(|n| n.contains("only defined profile")));
        // builtin selection is a note, never rules
        let builtin = "default_permissions = \":workspace\"\n";
        let c = convert_codex(builtin).unwrap();
        assert!(c.rules.is_empty());
        assert!(c.notes.iter().any(|n| n.contains(":workspace")));
    }

    // ---- gemini --------------------------------------------------------

    const GEMINI_SAMPLE: &str = r#"{
      "general": { "defaultApprovalMode": "auto_edit" },
      "tools": {
        "allowed": [
          "run_shell_command(git)",
          "run_shell_command",
          "replace",
          "read_file",
          "web_search",
          "glob",
          "save_memory"
        ],
        "confirmationRequired": ["run_shell_command(npm publish)"],
        "exclude": ["write_file"],
        "core": ["run_shell_command", "replace"]
      },
      "mcp": { "allowed": ["github"], "excluded": ["internal"] },
      "security": { "toolSandboxing": true }
    }"#;

    #[test]
    fn gemini_tool_lists_convert_with_gemini_precedence() {
        let c = convert_gemini(GEMINI_SAMPLE).unwrap();
        let find = |tool: &str, pat: Option<&str>, verdict: Verdict| {
            c.rules
                .iter()
                .any(|r| r.tool == tool && r.pattern.as_deref() == pat && r.verdict == verdict)
        };
        // deny (exclude) first, then ask (confirmationRequired), then allow
        assert_eq!(c.rules.first().map(|r| r.verdict), Some(Verdict::Deny));
        let order: Vec<Verdict> = c.rules.iter().map(|r| r.verdict).collect();
        let first_allow = order.iter().position(|v| *v == Verdict::Allow).unwrap();
        let ask_pos = order.iter().position(|v| *v == Verdict::Ask).unwrap();
        assert!(ask_pos < first_allow, "{order:?}");
        // gemini shell prefixes → ka bash globs
        assert!(find("bash", Some("git*"), Verdict::Allow));
        assert!(find("bash", None, Verdict::Allow));
        assert!(find("bash", Some("npm publish*"), Verdict::Ask));
        // gemini tool names → ka tools
        assert!(find("edit", None, Verdict::Allow));
        assert!(find("read", None, Verdict::Allow));
        assert!(find("web_search", None, Verdict::Allow));
        assert!(find("write", None, Verdict::Deny));
        // ungated tools reported, never dropped
        let skipped: Vec<_> = c.skipped.iter().map(|(e, _)| e.as_str()).collect();
        assert!(skipped.contains(&"glob"), "{skipped:?}");
        assert!(skipped.contains(&"save_memory"), "{skipped:?}");
        // availability knobs and auto-accept modes are notes
        assert!(c.notes.iter().any(|n| n.contains("tools.core")));
        assert!(
            c.notes
                .iter()
                .any(|n| n.contains("defaultApprovalMode") && n.contains("accept-edits"))
        );
        assert!(c.notes.iter().any(|n| n.contains("mcp.allowed")));
        assert!(c.notes.iter().any(|n| n.contains("toolSandboxing")));
        // strict write path works end to end
        let dir = std::env::temp_dir().join(format!("ka-import-gemini-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("ka.toml");
        write_rules(&target, &c, "gemini settings.json (ka import gemini)").unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("# imported from gemini settings.json"));
        let layer = Config::parse_layer(&text, "check").unwrap();
        assert!(layer.rules.len() >= 6, "{}", layer.rules.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gemini_bad_json_and_empty_config_behave_like_claude() {
        assert!(convert_gemini("{nope").is_err());
        let c = convert_gemini("{}").unwrap();
        assert!(c.rules.is_empty() && c.skipped.is_empty());
        assert!(c.notes.is_empty());
    }

    // ---- cursor --------------------------------------------------------

    #[test]
    fn cursor_is_rejected_as_instructions_not_permissions() {
        let err = convert_cursor();
        assert!(err.contains("instructions"), "{err}");
        assert!(err.contains("[[rules]]"), "{err}");
        // ka's actual instruction sources are named in the message
        assert!(err.contains("AGENTS.md"), "{err}");
        assert!(err.contains("project_doc_fallback_filenames"), "{err}");
        assert!(err.contains(".ka/rules/*.md"), "{err}");
        // dispatching through run() fails the same way, file or not
        let err = run("cursor", std::path::Path::new("/nonexistent"), None, true)
            .expect_err("cursor must fail");
        assert!(err.contains("instructions"), "{err}");
    }
}
