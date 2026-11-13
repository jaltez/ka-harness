//! Session feature toggles: the runtime overlay that decides which
//! capabilities the model sees. One grammar everywhere (CLI
//! `--disable`/`--enable`, `[features] disable` in ka.toml, `/features`
//! in the TUI, `Record::Change` snapshots in the strand) — the parsed
//! form is [`ka_protocol::FeatureSpec`].
//!
//! Disabling is fail-closed: the tool vanishes from the model-facing
//! specs AND a stray call is rejected by name at admit time. Disabling
//! never weakens the permission tiers — it only removes capabilities.
//! Bare mode (`--safe-mode`) outranks every toggle: nothing disabled by
//! bare mode can be re-enabled.

use std::collections::BTreeSet;

use ka_protocol::FeatureSpec;

/// Shared toggle state: the voice filters specs/admits through it, and
/// the hands that validate dynamically (delegate roster, lazy MCP calls)
/// hold a clone so their def()/execute() stay truthful.
pub type FeatureSlot = std::sync::Arc<parking_lot::RwLock<FeatureToggles>>;

/// Parse + validate `[features] disable` / `--disable` spec strings.
/// Unknown specs are a hard error naming them — strict-config culture,
/// same as unknown TOML keys.
pub fn parse_specs(specs: &[String]) -> Result<Vec<FeatureSpec>, String> {
    let mut parsed = Vec::with_capacity(specs.len());
    let mut bad = Vec::new();
    for s in specs {
        match s.parse() {
            Ok(spec) => parsed.push(spec),
            Err(_) => bad.push(s.clone()),
        }
    }
    if bad.is_empty() {
        Ok(parsed)
    } else {
        Err(format!(
            "unknown feature spec(s) {} — expected agents|skills|mcp|hooks|web|lsp|debug \
             or mcp:<server>|skill:<name>|tool:<name>|agent:<name>",
            bad.join(", ")
        ))
    }
}

/// The live toggle set. `disabled` is a snapshot (persisted whole); the
/// live-MCP names feed the `server__tool` prefix matching.
#[derive(Debug, Clone, Default)]
pub struct FeatureToggles {
    disabled: BTreeSet<FeatureSpec>,
    live_mcp: Vec<String>,
    sandbox: Option<String>,
}

impl FeatureToggles {
    /// A slot primed with the given disabled specs (engine bootstrap).
    pub fn slot(disabled: Vec<FeatureSpec>) -> FeatureSlot {
        std::sync::Arc::new(parking_lot::RwLock::new(Self {
            disabled: disabled.into_iter().collect(),
            live_mcp: Vec::new(),
            sandbox: None,
        }))
    }

    /// The disabled specs, display-ordered (snapshot form).
    pub fn disabled_specs(&self) -> Vec<String> {
        self.disabled.iter().map(|s| s.to_string()).collect()
    }

    /// Whether a spec is switched off.
    pub fn is_disabled(&self, spec: &FeatureSpec) -> bool {
        self.disabled.contains(spec)
    }

    /// Set one spec; returns whether the set changed.
    pub fn set(&mut self, spec: &FeatureSpec, enabled: bool) -> bool {
        if enabled {
            self.disabled.remove(spec)
        } else {
            self.disabled.insert(spec.clone())
        }
    }

    /// Replace the whole disabled set + sandbox override (strand
    /// restore). Live MCP names describe the world, not the toggles —
    /// they stay.
    pub fn replace_snapshot(&mut self, disabled: Vec<FeatureSpec>, sandbox: Option<String>) {
        self.disabled = disabled.into_iter().collect();
        self.sandbox = sandbox;
    }

    /// Names of the connected MCP servers (prefix matching for eager
    /// hands). Maintained by the engine as servers come and go.
    pub fn set_live_mcp(&mut self, names: Vec<String>) {
        self.live_mcp = names;
    }

    /// Whether one MCP server is usable (`mcp` and `mcp:<name>` both off).
    pub fn server_enabled(&self, name: &str) -> bool {
        !self.is_disabled(&FeatureSpec::Mcp)
            && !self.is_disabled(&FeatureSpec::McpServer(name.into()))
    }

    /// Whether the skills block of the system prompt is live, and one
    /// named skill within it.
    pub fn skills_enabled(&self) -> bool {
        !self.is_disabled(&FeatureSpec::Skills)
    }

    /// Whether the web tools (client or provider-native search) are
    /// offered at all — the fail-closed gate the wires honor for
    /// server-side search injection.
    pub fn web_enabled(&self) -> bool {
        !self.is_disabled(&FeatureSpec::Web)
    }

    /// Whether one named skill is offered.
    pub fn skill_enabled(&self, name: &str) -> bool {
        self.skills_enabled() && !self.is_disabled(&FeatureSpec::Skill(name.into()))
    }

    /// Whether one named subagent is offered.
    pub fn agent_enabled(&self, name: &str) -> bool {
        !self.is_disabled(&FeatureSpec::Agent(name.into()))
    }

    /// Whether config + file hooks run at all.
    pub fn hooks_enabled(&self) -> bool {
        !self.is_disabled(&FeatureSpec::Hooks)
    }

    /// The live sandbox-mode override (None = follows config).
    pub fn sandbox_override(&self) -> Option<&str> {
        self.sandbox.as_deref()
    }

    /// Record a sandbox-mode override.
    pub fn set_sandbox_override(&mut self, mode: Option<String>) {
        self.sandbox = mode;
    }

    /// Which disabled spec hides this tool, if any. `None` = visible.
    /// Prompt-side toggles (skills, hooks, individual agents) never hide
    /// a tool and are not consulted here.
    pub fn hidden_reason(&self, tool: &str) -> Option<FeatureSpec> {
        for spec in &self.disabled {
            let hit = match spec {
                FeatureSpec::Tool(name) => tool == name,
                FeatureSpec::Agents => tool == "delegate" || tool == "tasks",
                FeatureSpec::Web => tool == "web_search" || tool == "web_fetch",
                FeatureSpec::Lsp => tool.starts_with("lsp_"),
                FeatureSpec::Debug => tool == "debug",
                // the meta hand is named `mcp`; lazy calls go through
                // `mcp_call`; eager hands are `server__tool` wire names
                FeatureSpec::Mcp => tool == "mcp" || tool == "mcp_call" || self.is_eager_mcp(tool),
                FeatureSpec::McpServer(server) => {
                    tool.starts_with(&crate::mcp::wire_prefix(server))
                }
                FeatureSpec::Skills
                | FeatureSpec::Hooks
                | FeatureSpec::Skill(_)
                | FeatureSpec::Agent(_) => false,
            };
            if hit {
                return Some(spec.clone());
            }
        }
        None
    }

    fn is_eager_mcp(&self, tool: &str) -> bool {
        self.live_mcp
            .iter()
            .any(|s| tool.starts_with(&crate::mcp::wire_prefix(s)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn toggles(specs: &[FeatureSpec]) -> FeatureToggles {
        FeatureToggles {
            disabled: specs.iter().cloned().collect(),
            live_mcp: vec!["github".into(), "jira".into()],
            sandbox: None,
        }
    }

    #[test]
    fn feature_words_hide_their_hands() {
        let t = toggles(&[FeatureSpec::Agents, FeatureSpec::Web, FeatureSpec::Debug]);
        for tool in ["delegate", "tasks", "web_search", "web_fetch", "debug"] {
            assert!(
                t.hidden_reason(tool).is_some(),
                "{tool} must be hidden by its feature word"
            );
        }
        for tool in ["read", "bash", "mcp_call", "lsp_symbols"] {
            assert!(t.hidden_reason(tool).is_none(), "{tool} stays visible");
        }
    }

    #[test]
    fn lsp_hides_by_prefix() {
        let t = toggles(&[FeatureSpec::Lsp]);
        assert!(t.hidden_reason("lsp_symbols").is_some());
        assert!(t.hidden_reason("lsp_rename").is_some());
        assert!(t.hidden_reason("read").is_none());
    }

    #[test]
    fn mcp_feature_and_per_server_hide_eager_hands() {
        // toggles filter the wire names the model sees (`server__tool`),
        // derived exactly the way mcp.rs builds them
        let github = crate::mcp::wire_name("github", "create_issue");
        let jira = crate::mcp::wire_name("jira", "search");
        let all = toggles(&[FeatureSpec::Mcp]);
        for tool in [github.as_str(), jira.as_str(), "mcp_call", "mcp"] {
            assert!(all.hidden_reason(tool).is_some(), "{tool} hidden by `mcp`");
        }
        // a prefix collision with a server name must not hide base tools
        assert!(all.hidden_reason("read").is_none());

        let one = toggles(&[FeatureSpec::McpServer("github".into())]);
        assert!(one.hidden_reason(&github).is_some());
        assert!(one.hidden_reason(&jira).is_none());
        // lazy/meta hands stay visible: they front other servers and
        // reject the disabled one per-call
        assert!(one.hidden_reason("mcp_call").is_none());
        assert!(one.hidden_reason("mcp").is_none());
        assert!(!one.server_enabled("github"));
        assert!(one.server_enabled("jira"));
    }

    #[test]
    fn per_server_hiding_follows_wire_name_cleaning() {
        // wire_name cleans off-pattern server names; hiding must match
        // the cleaned prefix or `mcp:my.srv` would hide nothing at all
        let t = toggles(&[FeatureSpec::McpServer("my.srv".into())]);
        let wire = crate::mcp::wire_name("my.srv", "create_issue");
        assert_eq!(wire, "my_srv__create_issue");
        assert!(
            t.hidden_reason(&wire).is_some(),
            "`{wire}` hidden by mcp:my.srv"
        );
        assert!(
            t.hidden_reason(&crate::mcp::wire_name("other", "call"))
                .is_none()
        );
    }

    #[test]
    fn per_tool_and_prompt_side_specs() {
        let t = toggles(&[
            FeatureSpec::Tool("bash".into()),
            FeatureSpec::Skills,
            FeatureSpec::Skill("pdf".into()),
            FeatureSpec::Hooks,
            FeatureSpec::Agent("reviewer".into()),
        ]);
        assert!(t.hidden_reason("bash").is_some());
        assert!(t.hidden_reason("read").is_none());
        assert!(!t.skills_enabled());
        assert!(!t.skill_enabled("pdf"));
        assert!(!t.hooks_enabled());
        assert!(!t.agent_enabled("reviewer"));
        assert!(t.agent_enabled("coder"));
    }

    #[test]
    fn set_reports_changes_and_snapshots_orderedly() {
        let mut t = FeatureToggles::default();
        assert!(t.set(&FeatureSpec::McpServer("z".into()), false));
        assert!(t.set(&FeatureSpec::Agents, false));
        assert!(
            !t.set(&FeatureSpec::Agents, false),
            "no-op set reports false"
        );
        assert!(t.set(&FeatureSpec::Agents, true));
        assert_eq!(t.disabled_specs(), vec!["mcp:z".to_string()]);
    }
}
