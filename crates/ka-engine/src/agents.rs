//! Markdown agents: user-defined subagents discovered from `.md` files.
//! One file = one agent: an optional `---` frontmatter block (`name`,
//! `description`, `max-steps`) and a body that becomes the subagent's
//! system prompt. The model delegates via the `delegate` tool.

use std::path::PathBuf;

/// One parsed agent definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    /// Slug the model references in `delegate`.
    pub name: String,
    /// When to delegate to this agent (shown in the tool description).
    pub description: String,
    /// The markdown body: the subagent's system prompt.
    pub system: String,
    /// Step budget for the nested voice.
    pub max_steps: u32,
    /// Run this agent in an isolated git worktree (its own branch);
    /// requires a git repository.
    pub isolate: bool,
    /// Model selector override for this agent (`vendor/model@effort`).
    /// Default: the parent session's model.
    pub model: Option<String>,
    /// Reasoning-effort override applied to the parent model when
    /// `model` is absent (`off|low|medium|high|max`).
    pub effort: Option<ka_protocol::Effort>,
    /// Tool allowlist for the nested voice (comma-separated in
    /// frontmatter, e.g. `tools: read, grep, glob`). Default: all
    /// (read-only) hands.
    pub tools: Option<Vec<String>>,
    /// JSON schema (compact, one frontmatter line) the final reply must
    /// satisfy — validated speaker-side through ka's structured-output
    /// path. Invalid JSON here is ignored (the agent stays usable).
    pub output: Option<serde_json::Value>,
}

impl AgentDef {
    /// Parse one markdown file. `fallback_name` (the file stem) is used
    /// when the frontmatter carries no `name`.
    pub fn parse(text: &str, fallback_name: &str) -> Self {
        let mut name = String::new();
        let mut description = String::new();
        let mut max_steps = 12u32;
        let mut isolate = false;
        let mut model: Option<String> = None;
        let mut effort: Option<ka_protocol::Effort> = None;
        let mut tools: Option<Vec<String>> = None;
        let mut output: Option<serde_json::Value> = None;
        let mut body = text.to_string();

        if let Some(rest) = text.strip_prefix("---") {
            if let Some(end) = rest.find("\n---") {
                for line in rest[..end].lines() {
                    let Some((key, value)) = line.split_once(':') else {
                        continue;
                    };
                    let value = value.trim();
                    match key.trim() {
                        "name" if !value.is_empty() => name = value.to_string(),
                        "description" if !value.is_empty() => description = value.to_string(),
                        "max-steps" | "max_steps" => {
                            max_steps = value.parse().unwrap_or(12).clamp(1, 64);
                        }
                        "isolate" => {
                            isolate =
                                matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "1");
                        }
                        "model" if !value.is_empty() => model = Some(value.to_string()),
                        "effort" if !value.is_empty() => {
                            effort = parse_effort(value);
                        }
                        "tools" if !value.is_empty() => {
                            tools = Some(
                                value
                                    .split(',')
                                    .map(str::trim)
                                    .filter(|t| !t.is_empty())
                                    .map(str::to_string)
                                    .collect(),
                            );
                        }
                        "output" if !value.is_empty() => {
                            output = serde_json::from_str(value).ok();
                        }
                        _ => {}
                    }
                }
                body = rest[end + 4..].trim_start_matches('\n').to_string();
            }
        }
        if name.is_empty() {
            name = fallback_name.to_string();
        }
        Self {
            name: name.to_string(),
            description,
            system: body.trim().to_string(),
            max_steps,
            isolate,
            model,
            effort,
            tools,
            output,
        }
    }

    /// Discovery roots for a working directory. Project scope anchors
    /// at the root project (nearest `.git` ancestor, else cwd), not the
    /// launch dir — and is trust-gated like rules/skills/hooks:
    /// untrusted projects contribute NOTHING. The user-level agent dir
    /// always stays.
    fn roots(cwd: &std::path::Path) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if crate::trust::project_trusted(cwd) {
            let root = crate::project_root(cwd);
            roots.extend([
                root.join(".ka/agents"),
                root.join(".agents"),
                root.join(".claude/agents"),
            ]);
        }
        if let Ok(home) = std::env::var("HOME") {
            roots.push(PathBuf::from(home).join(".config/ka/agents"));
        }
        roots
    }

    /// Discover agents (name-sorted; project dirs win over the user dir
    /// on name collisions — first sighting wins).
    pub fn discover(cwd: &std::path::Path) -> Vec<AgentDef> {
        let mut agents: Vec<AgentDef> = Vec::new();
        for root in Self::roots(cwd) {
            let Ok(entries) = std::fs::read_dir(&root) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "md") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let stem = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if stem.is_empty() {
                    continue;
                }
                let def = Self::parse(&text, &stem);
                if def.system.is_empty() {
                    continue;
                }
                if !agents.iter().any(|a| a.name == def.name) {
                    agents.push(def);
                }
            }
        }
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        agents
    }
}

/// Frontmatter `effort` value → enum (case-insensitive).
fn parse_effort(value: &str) -> Option<ka_protocol::Effort> {
    match value.to_ascii_lowercase().as_str() {
        "off" | "none" => Some(ka_protocol::Effort::Off),
        "low" => Some(ka_protocol::Effort::Low),
        "medium" | "mid" => Some(ka_protocol::Effort::Medium),
        "high" => Some(ka_protocol::Effort::High),
        "max" | "ultra" => Some(ka_protocol::Effort::Max),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn output_frontmatter_parses_and_ignores_invalid_json() {
        let def = AgentDef::parse(
            "---\nname: checker\noutput: {\"type\":\"object\",\"properties\":{\"ok\":{\"type\":\"boolean\"}}}\n---\nbody",
            "fallback",
        );
        assert_eq!(def.name, "checker");
        assert_eq!(def.output.as_ref().unwrap()["type"], "object");

        // invalid JSON is ignored — the agent stays usable, unstructured
        let bad = AgentDef::parse("---\nname: bad\noutput: {not json\n---\nbody", "fallback");
        assert_eq!(bad.name, "bad");
        assert!(bad.output.is_none());
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let md = "---\nname: reviewer\ndescription: reviews code for bugs\nmax-steps: 20\n---\nYou are a code reviewer.\nBe harsh.";
        let def = AgentDef::parse(md, "fallback");
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.description, "reviews code for bugs");
        assert_eq!(def.max_steps, 20);
        assert_eq!(def.system, "You are a code reviewer.\nBe harsh.");
    }

    #[test]
    fn parses_model_effort_tools_frontmatter() {
        let md = "---\nname: explorer\nmodel: ollama/qwen3.5:9b\neffort: low\ntools: read, grep, glob\n---\nYou explore.";
        let def = AgentDef::parse(md, "fallback");
        assert_eq!(def.model.as_deref(), Some("ollama/qwen3.5:9b"));
        assert_eq!(def.effort, Some(ka_protocol::Effort::Low));
        assert_eq!(
            def.tools,
            Some(vec![
                "read".to_string(),
                "grep".to_string(),
                "glob".to_string()
            ])
        );
        // effort-only re-arms the parent selector; unknown effort ignored
        let md = "---\neffort: MAX\n---\nBody.";
        let def = AgentDef::parse(md, "s");
        assert_eq!(def.effort, Some(ka_protocol::Effort::Max));
        let md = "---\neffort: bananas\n---\nBody.";
        let def = AgentDef::parse(md, "s");
        assert_eq!(def.effort, None);
        // model can carry its own @effort
        let md = "---\nmodel: openai/o3@high\n---\nBody.";
        let def = AgentDef::parse(md, "s");
        assert_eq!(def.model.as_deref(), Some("openai/o3@high"));
        assert_eq!(def.effort, None);
    }

    #[test]
    fn frontmatter_is_optional_with_defaults() {
        let def = AgentDef::parse("Just a body prompt.", "stem-name");
        assert_eq!(def.name, "stem-name");
        assert_eq!(def.description, "");
        assert_eq!(def.max_steps, 12);
        assert_eq!(def.system, "Just a body prompt.");
    }

    #[test]
    fn malformed_values_fall_back() {
        let md = "---\nmax-steps: bananas\nname: x\n---\nBody.";
        let def = AgentDef::parse(md, "s");
        assert_eq!(def.max_steps, 12, "unparseable max-steps keeps the default");
    }

    #[test]
    fn unterminated_frontmatter_treated_as_body() {
        let md = "---\nname: broken\nYou are still a prompt.";
        let def = AgentDef::parse(md, "stem");
        assert_eq!(def.name, "stem");
        assert!(def.system.contains("---"), "kept verbatim");
    }

    #[test]
    fn discovers_from_directories_with_precedence() {
        let dir = std::env::temp_dir().join(format!("ka-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = dir.join("proj/.ka/agents");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(dir.join("proj/.agents")).unwrap();
        std::fs::write(
            proj.join("reviewer.md"),
            "---\nname: reviewer\ndescription: project version\n---\nProject reviewer.",
        )
        .unwrap();
        std::fs::write(dir.join("proj/.agents/util.md"), "Utility agent.").unwrap();

        // project agent dirs are trust-gated like rules/skills: approve
        // the project in a redirected store before discovering
        crate::trust::test_support::with_trust_file(|_| {
            crate::trust::approve(&dir.join("proj"));
            let agents = AgentDef::discover(dir.join("proj").as_path());
            let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
            assert!(names.contains(&"reviewer"), "{names:?}");
            assert!(names.contains(&"util"), "{names:?}");
            let reviewer = agents.iter().find(|a| a.name == "reviewer").unwrap();
            assert_eq!(reviewer.description, "project version");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
