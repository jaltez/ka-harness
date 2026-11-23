//! `ka recipe <name>` (AUTO-04): run a named step list stored in the
//! project's `.ka/recipes/<name>.toml`. A recipe is strict TOML:
//!
//! ```toml
//! description = "What this recipe is for"
//! [vars]
//! scope = "src/"
//! [[steps]]
//! prompt = "Review {{scope}} for ..."
//! ```
//!
//! Execution spawns ONE session (`spawn_full`) and sends the steps as
//! sequential prompts, draining the event stream to `Idle` between
//! steps so each turn fully settles before the next prompt lands.
//! `{{var}}` placeholders are substituted from `--var k=v` overrides
//! layered over the recipe's `[vars]` defaults; a placeholder with no
//! value anywhere is a hard error (recipes must not run half-rendered).
//! `--dry-run` prints the rendered prompts and exits without spawning
//! anything.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;

use serde::Deserialize;

/// One step: the prompt template to send.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Step {
    pub prompt: String,
}

/// The strict recipe document. Unknown keys anywhere are parse errors —
/// a typo'd `var:` must not silently become a no-op.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Recipe {
    /// Optional human description (printed by the runner).
    #[serde(default)]
    pub description: Option<String>,
    /// Placeholder defaults; `--var k=v` overrides these.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    /// At least one step (an empty recipe is a mistake, not a no-op).
    pub steps: Vec<Step>,
}

/// Parse a recipe document strictly.
pub(crate) fn parse_recipe(text: &str) -> Result<Recipe, String> {
    let recipe: Recipe = toml::from_str(text).map_err(|e| format!("invalid recipe TOML: {e}"))?;
    if recipe.steps.is_empty() {
        return Err("recipe has no steps (steps must be a non-empty list of \
                    { prompt = ... } tables)"
            .to_string());
    }
    Ok(recipe)
}

/// Load `.ka/recipes/<name>.toml` under repo `root`. The name is a bare
/// file stem — path separators are rejected so a recipe name can never
/// escape the recipes directory.
pub(crate) fn load_recipe(root: &Path, name: &str) -> Result<Recipe, String> {
    validate_name(name)?;
    let path = root
        .join(".ka")
        .join("recipes")
        .join(format!("{name}.toml"));
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_recipe(&text)
}

/// Recipe file names: bare stems only (no traversal, no hidden files).
fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "recipe name {name:?} must be a bare stem of letters, digits, '-' or '_' \
             (it maps to .ka/recipes/<name>.toml)"
        ))
    }
}

/// Resolve the final var table: recipe defaults overlaid with the
/// command-line `k=v` pairs. Unknown override keys and placeholders
/// that resolve to nothing are both errors (listed, all at once).
pub(crate) fn resolve_vars(
    recipe: &Recipe,
    overrides: &[(String, String)],
) -> Result<BTreeMap<String, String>, String> {
    let mut vars = recipe.vars.clone();
    let mut bad_overrides: Vec<String> = Vec::new();
    for (k, v) in overrides {
        if !recipe.vars.contains_key(k) {
            bad_overrides.push(k.clone());
        } else {
            vars.insert(k.clone(), v.clone());
        }
    }
    if !bad_overrides.is_empty() {
        let known: Vec<&String> = recipe.vars.keys().collect();
        return Err(format!(
            "--var unknown key(s): {} — the recipe declares {}",
            bad_overrides.join(", "),
            if known.is_empty() {
                "none".to_string()
            } else {
                known
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
    }
    // every placeholder across steps must resolve; report them together
    let mut missing: Vec<String> = Vec::new();
    for step in &recipe.steps {
        for name in placeholder_names(&step.prompt) {
            if !vars.contains_key(&name) && !missing.contains(&name) {
                missing.push(name);
            }
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "unresolved placeholder(s): {} — set them with --var k=v or a \
             [vars] default",
            missing.join(", ")
        ));
    }
    Ok(vars)
}

/// Render one prompt: every `{{ name }}` becomes the var's value.
/// Unknown names never reach here ([`resolve_vars`] gated them), but the
/// renderer still refuses them — the two halves stay independently safe.
pub(crate) fn render(prompt: &str, vars: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(prompt.len());
    let mut rest = prompt;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err("unterminated {{ placeholder (missing }})".to_string());
        };
        let name = after[..end].trim();
        let value = vars
            .get(name)
            .ok_or_else(|| format!("unresolved placeholder {{{{{name}}}}}"))?;
        out.push_str(value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Distinct placeholder names in one template.
fn placeholder_names(prompt: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = prompt;
    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start + 2..].find("}}") else {
            break;
        };
        let name = rest[start + 2..start + 2 + end].trim().to_string();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
        rest = &rest[start + 2 + end + 2..];
    }
    names
}

/// Render every step in order (the dry-run surface: no session, no model).
pub(crate) fn render_steps(
    recipe: &Recipe,
    overrides: &[(String, String)],
) -> Result<Vec<String>, String> {
    let vars = resolve_vars(recipe, overrides)?;
    recipe
        .steps
        .iter()
        .map(|s| render(&s.prompt, &vars))
        .collect()
}

/// Run `ka recipe <name> --var k=v ... [--dry-run]`.
pub(crate) async fn run(
    name: &str,
    overrides: &[(String, String)],
    dry_run: bool,
) -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let root = ka_engine::project_root(&cwd);
    let recipe = load_recipe(&root, name)?;
    let rendered = render_steps(&recipe, overrides)?;
    if let Some(desc) = &recipe.description {
        println!("{desc}");
    }
    if dry_run {
        println!("dry run — {} step(s), nothing executed:", rendered.len());
        for (i, prompt) in rendered.iter().enumerate() {
            println!("--- step {} ---", i + 1);
            println!("{prompt}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    execute_steps(&rendered).await
}

/// One session, sequential prompts, each turn fully drained to `Idle`
/// before the next prompt is sent (the same event loop `ka run` uses:
/// asks auto-deny, text accumulates, hard errors abort the recipe).
async fn execute_steps(rendered: &[String]) -> Result<ExitCode, String> {
    let trust = crate::trust_for_cwd(false);
    crate::warn_untrusted_conventions(
        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    );
    let cfg = crate::load_config(
        &[],
        None,
        None,
        crate::trusted_project_cwd(trust),
        &crate::FeatureFlags::default(),
        false,
        None,
    )?;
    let catalog = crate::build_catalog(&[], true).await?;
    let mut handle = ka_engine::spawn_full(cfg, catalog, ka_engine::StrandChoice::New);
    for (i, prompt) in rendered.iter().enumerate() {
        println!("--- step {}/{} ---", i + 1, rendered.len());
        handle
            .commands
            .send(ka_protocol::Command::Prompt {
                text: prompt.clone(),
                schema: None,
                images: Vec::new(),
                allowed_tools: None,
                model: None,
                skills: Vec::new(),
            })
            .await
            .map_err(|_| "engine closed before prompt".to_string())?;
        let mut final_text = String::new();
        let mut idle_seen = false;
        let mut failed: Option<String> = None;
        while let Some(event) = handle.events.recv().await {
            if let ka_protocol::Event::Ask { id, questions } = &event {
                handle
                    .commands
                    .send(ka_protocol::Command::Answer {
                        question: id.clone(),
                        choice: crate::deny_choice(questions),
                    })
                    .await
                    .map_err(|_| "engine closed during ask")?;
            }
            match &event {
                ka_protocol::Event::Delta {
                    kind: ka_protocol::DeltaKind::Text(text),
                } => final_text.push_str(text),
                ka_protocol::Event::Error {
                    message, retryable, ..
                } => {
                    eprintln!("ka: {message}");
                    if !*retryable {
                        failed = Some(message.clone());
                    }
                }
                ka_protocol::Event::Idle => {
                    idle_seen = true;
                    break;
                }
                _ => {}
            }
        }
        if let Some(message) = failed {
            return Err(format!("step {} failed: {message}", i + 1));
        }
        if !idle_seen {
            return Err(format!(
                "step {}: the engine ended without settling (no Idle)",
                i + 1
            ));
        }
        if !final_text.is_empty() {
            println!("{final_text}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const SAMPLE: &str = r#"
description = "Release prep"
[vars]
scope = "crates/"
[[steps]]
prompt = "Review {{scope}} for TODOs"
[[steps]]
prompt = "Summarize changes for the changelog"
"#;

    #[test]
    fn strict_parse_accepts_the_documented_shape() {
        let r = parse_recipe(SAMPLE).unwrap();
        assert_eq!(r.description.as_deref(), Some("Release prep"));
        assert_eq!(r.vars.get("scope").map(String::as_str), Some("crates/"));
    }

    #[test]
    fn unknown_keys_are_rejected_everywhere() {
        assert!(
            parse_recipe("foo = 1\n[[steps]]\nprompt = \"x\"\n")
                .unwrap_err()
                .contains("unknown field")
        );
        assert!(
            parse_recipe("[[steps]]\nprompt = \"x\"\nextra = 1\n")
                .unwrap_err()
                .contains("unknown field")
        );
    }

    #[test]
    fn empty_recipe_is_an_error() {
        // missing `steps` is a parse error; an explicitly empty list is
        // the runner's "no steps" error
        assert!(parse_recipe("").is_err());
        assert!(
            parse_recipe("steps = []\n")
                .unwrap_err()
                .contains("no steps")
        );
    }

    #[test]
    fn substitution_overrides_defaults_and_keeps_step_order() {
        let r = parse_recipe(SAMPLE).unwrap();
        let out = render_steps(&r, &[("scope".to_string(), "src/".to_string())]).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], "Review src/ for TODOs");
        assert_eq!(out[1], "Summarize changes for the changelog");
    }

    #[test]
    fn missing_and_unknown_vars_are_listed() {
        let r = parse_recipe("[[steps]]\nprompt = \"check {{missing}} and {{absent}}\"\n").unwrap();
        let err = resolve_vars(&r, &[]).unwrap_err();
        assert!(err.contains("missing") && err.contains("absent"), "{err}");
        let full = parse_recipe(SAMPLE).unwrap();
        let err = resolve_vars(&full, &[("nope".to_string(), "v".to_string())]).unwrap_err();
        assert!(err.contains("unknown key(s): nope"), "{err}");
    }

    #[test]
    fn renderer_refuses_unknown_and_unterminated_placeholders() {
        let mut vars = BTreeMap::new();
        vars.insert("a".to_string(), "1".to_string());
        assert_eq!(render("x{{ a }}y", &vars).unwrap(), "x1y");
        assert_eq!(render("{{ a }}{{a}}", &vars).unwrap(), "11");
        assert!(render("{{ b }}", &vars).unwrap_err().contains("unresolved"));
        assert!(render("{{ a", &vars).unwrap_err().contains("unterminated"));
    }

    #[test]
    fn dry_run_never_spawns_a_session() {
        // render_steps IS the dry-run path: pure, no engine, no network
        let r = parse_recipe(SAMPLE).unwrap();
        let out = render_steps(&r, &[]).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out[0].starts_with("Review crates/"));
    }

    #[test]
    fn names_cannot_escape_the_recipes_dir() {
        assert!(validate_name("release-prep").is_ok());
        assert!(validate_name("two words").is_err());
        assert!(validate_name("../etc/passwd").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("").is_err());
    }

    #[test]
    fn loads_from_the_project_recipes_dir() {
        let root = std::env::temp_dir().join(format!("ka-recipe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".ka/recipes")).unwrap();
        std::fs::write(root.join(".ka/recipes/demo.toml"), SAMPLE).unwrap();
        let r = load_recipe(&root, "demo").unwrap();
        assert_eq!(r.steps.len(), 2);
        assert!(load_recipe(&root, "missing").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
