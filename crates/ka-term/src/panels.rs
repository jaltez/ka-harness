//! Read-only management panels (TUI-18): `/hooks`, `/permissions`,
//! `/doctor`. Every row is a fact the surface can actually see — the
//! layered ka.toml files the TUI reads (user + trusted project) and
//! live session state. Nothing probes the network here: the doctor
//! CLI owns those checks, the TUI panel states that out of scope at
//! the bottom.

use std::path::Path;

/// `/hooks`: config `[[hooks]]` entries (event · tool · command) plus
/// the `.ka/hooks/` file-convention scripts with their live presence.
pub fn hooks_rows(cfg: &ka_engine::Config, cwd: &Path) -> Vec<String> {
    let mut rows = Vec::new();
    if cfg.hooks.is_empty() {
        rows.push("(no [[hooks]] in the ka.toml layers the TUI reads)".to_string());
    } else {
        rows.push("config hooks ([[hooks]], first listed runs first)".to_string());
        for h in &cfg.hooks {
            let tool = h.tool.as_deref().unwrap_or("all tools");
            rows.push(format!(
                "  {:<18} {:<12} {}",
                h.event.wire_name(),
                tool,
                h.command
            ));
        }
    }
    rows.push(String::new());
    rows.push("file hooks (.ka/hooks/, executable scripts)".to_string());
    let root = ka_engine::project_root(cwd);
    for name in ["pre-turn", "post-turn", "pre-tool"] {
        let path = root.join(".ka/hooks").join(name);
        match file_state(&path) {
            FileState::Executable => rows.push(format!(
                "  {name:<10} {}  (executable)",
                short(&path.display().to_string())
            )),
            FileState::Plain => rows.push(format!(
                "  {name:<10} {}  (present, not executable)",
                short(&path.display().to_string())
            )),
            FileState::Missing => rows.push(format!("  {name:<10} (absent)")),
        }
    }
    rows.push(String::new());
    rows.push("sources: user + trusted-project ka.toml; profile/CLI".to_string());
    rows.push("layers stay engine-side and are not shown here".to_string());
    rows
}

enum FileState {
    Executable,
    Plain,
    Missing,
}

fn file_state(path: &Path) -> FileState {
    let Ok(meta) = std::fs::metadata(path) else {
        return FileState::Missing;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            FileState::Executable
        } else {
            FileState::Plain
        }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        FileState::Plain
    }
}

fn short(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    path.strip_prefix(&home)
        .map(|r| format!("~{r}"))
        .unwrap_or_else(|| path.to_string())
}

/// `/permissions`: the live mode, first-match rules, the persistent
/// always-allow list, protected paths, and the sandbox posture.
pub fn permissions_rows(
    cfg: &ka_engine::Config,
    live_sandbox: Option<&str>,
    mode: &str,
) -> Vec<String> {
    let mut rows = Vec::new();
    rows.push(format!("mode: {mode} (live session mode)"));
    rows.push(String::new());
    if cfg.rules.is_empty() {
        rows.push("rules: none configured ([permissions.rules])".to_string());
    } else {
        rows.push("rules (first matching rule wins, in order)".to_string());
        for r in &cfg.rules {
            let pattern = r.pattern.as_deref().unwrap_or("*");
            let verdict = match r.verdict {
                ka_engine::config::Verdict::Allow => "allow",
                ka_engine::config::Verdict::Ask => "ask",
                ka_engine::config::Verdict::Deny => "deny",
            };
            rows.push(format!(
                "  {:<10} {:<28} {}",
                r.tool,
                trunc(pattern, 28),
                verdict
            ));
        }
    }
    rows.push(String::new());
    if cfg.permissions.allow.is_empty() {
        rows.push("always-allow: none ([permissions] allow)".to_string());
    } else {
        rows.push(format!(
            "always-allow: {} (asks answered \"always\" land here)",
            cfg.permissions.allow.join(", ")
        ));
    }
    rows.push(String::new());
    rows.push("protected paths (engine-fixed — writes always ask):".to_string());
    for item in [
        ".git internals (hooks, config, modules)",
        "~/.ssh keys and configuration",
        "shell startup files (~/.bashrc, ~/.zshrc, ~/.profile, …)",
        "ka's own config, credentials, and trust store",
    ] {
        rows.push(format!("  · {item}"));
    }
    rows.push(String::new());
    let cfg_mode = cfg.sandbox.mode.as_deref().unwrap_or("off (default)");
    match live_sandbox {
        Some(live) => rows.push(format!("sandbox: config {cfg_mode} · live override {live}")),
        None => rows.push(format!("sandbox: {cfg_mode}")),
    }
    if cfg.sandbox.allow_write.is_empty() {
        rows.push("sandbox allow_write: none beyond the defaults".to_string());
    } else {
        rows.push(format!(
            "sandbox allow_write: {}",
            cfg.sandbox.allow_write.join(", ")
        ));
    }
    rows.push(String::new());
    rows.push("layers: user + trusted-project ka.toml (TUI view);".to_string());
    rows.push("session grants made mid-turn live engine-side".to_string());
    rows
}

/// Runtime facts the doctor panel renders — everything here is
/// process-local state or config; the network checks stay in the
/// doctor CLI.
pub struct DoctorFacts<'a> {
    /// Active strand id ("" when the engine has not announced one yet).
    pub session: &'a str,
    /// Model selector.
    pub model: &'a str,
    /// Reasoning effort label.
    pub effort: &'a str,
    /// Permission mode label.
    pub mode: &'a str,
    /// Provider catalog with live key_set flags.
    pub providers: &'a [crate::tui::ProviderInfo],
    /// Live sandbox override (None = follows config).
    pub live_sandbox: Option<&'a str>,
    /// Finished turns.
    pub turns: u64,
    /// Session cost.
    pub cost: f64,
    /// Context (used, window).
    pub context: (u64, u64),
    /// Pending update badge.
    pub update: Option<&'a str>,
    /// MCP server summaries.
    pub mcp: &'a [ka_protocol::McpSummary],
}

/// `/doctor`: the live state ka-term knows, one fact per row.
pub fn doctor_rows(cfg: &ka_engine::Config, facts: &DoctorFacts) -> Vec<String> {
    let mut rows = Vec::new();
    let mut version = format!("ka {}", env!("CARGO_PKG_VERSION"));
    if let Some(latest) = facts.update {
        version.push_str(&format!("  · update pending: {latest}"));
    }
    rows.push(version);
    rows.push(String::new());
    rows.push(format!(
        "model: {}  · effort {}  · mode {}",
        facts.model, facts.effort, facts.mode
    ));
    // provider + key: the current model's vendor, key state from the
    // catalog (env vars are checked in-process at startup)
    let vendor = facts
        .model
        .trim_start_matches('+')
        .split('/')
        .next()
        .unwrap_or("");
    if let Some(p) = facts.providers.iter().find(|p| p.name == vendor) {
        let key = if p.env_var.is_empty() {
            "keyless".to_string()
        } else if p.key_set {
            format!("key set ({})", p.env_var)
        } else {
            format!("key MISSING — {} unset", p.env_var)
        };
        rows.push(format!("provider: {} · {key}", p.name));
    } else if !vendor.is_empty() {
        rows.push(format!("provider: {vendor} · (not in the catalog)"));
    }
    rows.push(String::new());
    let cfg_sandbox = cfg.sandbox.mode.as_deref().unwrap_or("off");
    let sandbox = match facts.live_sandbox {
        Some(live) => format!("{live} (config: {cfg_sandbox})"),
        None => format!("{cfg_sandbox} (config)"),
    };
    let backend = match ka_sandbox::detect_tool() {
        Some(tool) => format!("{tool:?}"),
        None => "none available".to_string(),
    };
    rows.push(format!("sandbox: {sandbox} · backend: {backend}"));
    rows.push(format!(
        "safe mode: {}",
        if ka_engine::conventions::bare_mode() {
            "on (--safe-mode)"
        } else {
            "off"
        }
    ));
    rows.push(String::new());
    let session = if facts.session.is_empty() {
        "(starting)"
    } else {
        facts.session
    };
    rows.push(format!("session: {session} · turns {}", facts.turns));
    rows.push(format!("cost: ${:.4}", facts.cost));
    let (used, window) = facts.context;
    if window > 0 {
        rows.push(format!(
            "context: {} of {} tok ({}%)",
            used,
            window,
            (used as f64 / window as f64 * 100.0) as u64
        ));
    } else if used > 0 {
        rows.push(format!("context: ~{used} tok (window unknown)"));
    } else {
        rows.push("context: no turn yet".to_string());
    }
    if !facts.mcp.is_empty() {
        let ok = facts.mcp.iter().filter(|m| m.ok).count();
        rows.push(format!(
            "mcp: {} server(s), {ok} connected",
            facts.mcp.len()
        ));
    }
    rows.push(String::new());
    rows.push("network/provider reachability checks live in the".to_string());
    rows.push("doctor CLI — this panel shows local state only".to_string());
    rows
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn cfg(toml_text: &str) -> ka_engine::Config {
        let mut base = ka_engine::Config::default();
        let layer = ka_engine::Config::parse_layer(toml_text, "test").expect("parses");
        base.overlay(layer);
        base
    }

    #[test]
    fn hooks_rows_list_config_and_file_hooks() {
        let c =
            cfg("[[hooks]]\nevent = \"pre_tool_use\"\ntool = \"bash\"\ncommand = \"guard.sh\"\n");
        let rows = hooks_rows(&c, Path::new("."));
        let joined = rows.join("\n");
        assert!(joined.contains("pre_tool_use"), "{joined}");
        assert!(joined.contains("bash"), "{joined}");
        assert!(joined.contains("guard.sh"), "{joined}");
        assert!(joined.contains("pre-turn"), "{joined}");

        let empty = hooks_rows(&ka_engine::Config::default(), Path::new("."));
        assert!(empty.join("\n").contains("(no [[hooks]]"));
    }

    #[test]
    fn permissions_rows_render_rules_and_sandbox() {
        let c = cfg(
            "[[rules]]\ntool = \"bash\"\npattern = \"cargo *\"\nverdict = \"allow\"\n\
             [[rules]]\ntool = \"write\"\nverdict = \"deny\"\n\
             [permissions]\nallow = [\"read\"]\n\
             [sandbox]\nmode = \"fs\"\nallow_write = [\"/opt/data\"]\n",
        );
        let rows = permissions_rows(&c, Some("off"), "ask");
        let joined = rows.join("\n");
        assert!(joined.contains("mode: ask"), "{joined}");
        assert!(joined.contains("cargo *"), "{joined}");
        assert!(joined.contains("allow"), "{joined}");
        assert!(joined.contains("deny"), "{joined}");
        assert!(joined.contains("read"), "{joined}");
        assert!(joined.contains("live override off"), "{joined}");
        assert!(joined.contains("/opt/data"), "{joined}");
        assert!(joined.contains(".git internals"), "{joined}");
    }

    #[test]
    fn doctor_rows_cover_the_live_facts() {
        let providers = vec![crate::tui::ProviderInfo {
            name: "zai".to_string(),
            env_var: "ZAI_API_KEY".to_string(),
            base_url: String::new(),
            key_set: true,
        }];
        let facts = DoctorFacts {
            session: "sabc",
            model: "zai/glm-5.3",
            effort: "high",
            mode: "allow",
            providers: &providers,
            live_sandbox: None,
            turns: 7,
            cost: 0.5,
            context: (10_000, 100_000),
            update: Some("0.8.0"),
            mcp: &[],
        };
        let rows = doctor_rows(&cfg("[sandbox]\nmode = \"fs\"\n"), &facts);
        let joined = rows.join("\n");
        assert!(joined.contains("key set (ZAI_API_KEY)"), "{joined}");
        assert!(joined.contains("update pending: 0.8.0"), "{joined}");
        assert!(joined.contains("fs (config)"), "{joined}");
        assert!(joined.contains("turns 7"), "{joined}");
        assert!(joined.contains("(10%)"), "{joined}");
        assert!(joined.contains("doctor CLI"), "{joined}");
    }
}
