//! Conventions: AGENTS.md hierarchy discovery and SKILL.md progressive
//! disclosure. Pure filesystem discovery; the voice folds results into
//! the system prompt.

use std::path::{Path, PathBuf};

/// One discovered AGENTS.md layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsFile {
    /// Absolute path (for on-demand reads by the model).
    pub path: PathBuf,
    /// File content (root→cwd layers are concatenated by the caller).
    pub content: String,
}

/// Safe mode (`ka --safe-mode`, or `KA_SAFEMODE=1` in the
/// environment): all convention discovery returns empty. Built-in
/// tools, the config chain, rules, and auth stay untouched — a
/// troubleshooting floor, not a factory reset. The CLI flips the
/// process-global flag before spawning any runtime threads
/// (`std::env::set_var` is unsafe in edition 2024 and the workspace
/// forbids `unsafe`).
static BARE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Rule-file conventions disabled (`ka --ignore-rules`): process-global
/// like [`BARE`], flipped before any runtime threads spawn.
static RULES_DISABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Enable safe mode for this process (CLI `--safe-mode`).
pub fn set_bare_mode(on: bool) {
    BARE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Disable the rule-file conventions for this process (CLI
/// `--ignore-rules`): `discover_rules` yields nothing, so neither the
/// project `.ka/rules/*.md` (and `.agents`/`.claude` siblings) nor the
/// user rules dir contribute. Permission `[[rules]]` tables are
/// cleared by the caller (`apply_run_opts`); hardstops are NOT
/// affected — they are not rules.
pub fn set_rules_disabled(on: bool) {
    RULES_DISABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether rule-file conventions are disabled for this process.
pub fn rules_disabled() -> bool {
    RULES_DISABLED.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn bare_mode() -> bool {
    // the env half is read once: bare_mode is consulted on every hook
    // run and discovery call, and KA_SAFEMODE never changes mid-process
    static ENV_BARE: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("KA_SAFEMODE").is_ok_and(|v| v == "1"));
    BARE.load(std::sync::atomic::Ordering::Relaxed) || *ENV_BARE
}

/// One loaded memory file.
pub struct MemoryFile {
    /// Where it came from.
    pub path: PathBuf,
    /// Full content.
    pub content: String,
}

/// Memory tiers: project `MEMORY.md` first (root-anchored — the root
/// project, ungated like AGENTS.md — and where `/memory` acceptance
/// writes), then the user-level `~/.config/ka/MEMORY.md`. Missing
/// files are skipped.
pub fn discover_memory(cwd: &Path) -> Vec<MemoryFile> {
    if bare_mode() {
        return Vec::new();
    }
    let mut out = Vec::new();
    // project tier anchors at the root project, not the launch dir —
    // symmetric with where /memory acceptance writes it
    let project = crate::project_root(cwd).join("MEMORY.md");
    if let Ok(content) = std::fs::read_to_string(&project) {
        if !content.trim().is_empty() {
            out.push(MemoryFile {
                path: project,
                content,
            });
        }
    }
    let user = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".config/ka/MEMORY.md"))
        .ok();
    if let Some(user) = user {
        if let Ok(content) = std::fs::read_to_string(&user) {
            if !content.trim().is_empty() {
                out.push(MemoryFile {
                    path: user,
                    content,
                });
            }
        }
    }
    out
}

pub fn discover_agents(cwd: &Path) -> Vec<AgentsFile> {
    discover_agents_with(cwd, &[])
}

/// [`discover_agents`] with configured fallback filenames
/// (`project_doc_fallback_filenames`): per directory, the first file
/// found in this order becomes that directory's layer —
/// `AGENTS.override.md` (replaces the directory's instructions, pi
/// semantics), `AGENTS.md`, `CLAUDE.md`, then the configured fallback
/// names in order. Empty/whitespace files never count as found.
pub fn discover_agents_with(cwd: &Path, fallbacks: &[String]) -> Vec<AgentsFile> {
    if bare_mode() {
        return Vec::new();
    }
    let mut chain: Vec<PathBuf> = vec![cwd.to_path_buf()];
    let mut cur = cwd.to_path_buf();
    let home = std::env::var("HOME").map(PathBuf::from).ok();
    while let Some(parent) = cur.parent() {
        if parent == cur {
            break;
        }
        cur = parent.to_path_buf();
        chain.push(cur.clone());
        if home.as_ref() == Some(&cur) || cur == Path::new("/") {
            break;
        }
    }
    let mut found = Vec::new();
    for dir in chain.iter().rev() {
        let mut names: Vec<&str> = vec!["AGENTS.override.md", "AGENTS.md", "CLAUDE.md"];
        names.extend(fallbacks.iter().map(String::as_str));
        for name in names {
            let candidate = dir.join(name);
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                if !content.trim().is_empty() {
                    found.push(AgentsFile {
                        path: candidate,
                        content,
                    });
                    break; // one file per directory, override > AGENTS.md
                }
            }
        }
    }
    // cap: keep the nearest 4 layers (deepest = most specific)
    if found.len() > 4 {
        let skip = found.len() - 4;
        found.drain(..skip);
    }
    found
}

/// One discovered `.ka/rules/*.md` file: glob-scoped project rules.
pub struct RuleFile {
    /// Where it came from.
    pub path: PathBuf,
    /// The markdown body (frontmatter stripped).
    pub content: String,
    /// `paths:` globs from frontmatter — the rule activates once any
    /// matching file has been read in the session (None = always on).
    pub globs: Option<Vec<String>>,
}

/// Discover rule files: project `.ka/rules` (trust-gated, like skills),
/// plus `.agents/rules`, `.claude/rules`, and the user
/// `~/.config/ka/rules`. Name-sorted, capped at 12, project wins on
/// name collisions.
pub fn discover_rules(cwd: &Path) -> Vec<RuleFile> {
    if bare_mode() || rules_disabled() {
        return Vec::new();
    }
    let trusted = crate::trust::project_trusted(cwd);
    let mut roots: Vec<PathBuf> = Vec::new();
    if trusted {
        // project scope anchors at the root project, not the launch dir
        let root = crate::project_root(cwd);
        roots.extend([
            root.join(".ka/rules"),
            root.join(".agents/rules"),
            root.join(".claude/rules"),
        ]);
    }
    if let Ok(home) = std::env::var("HOME") {
        roots.push(PathBuf::from(home).join(".config/ka/rules"));
    }
    let mut rules: Vec<RuleFile> = Vec::new();
    for root in roots {
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
            let (globs, body) = split_rule_frontmatter(&text);
            if body.trim().is_empty() {
                continue;
            }
            if rules.iter().any(|r| r.path.file_stem() == path.file_stem()) {
                continue; // first root wins (project > user)
            }
            rules.push(RuleFile {
                path,
                content: body.trim().to_string(),
                globs,
            });
        }
    }
    rules.sort_by_key(|r| {
        r.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    rules.truncate(12);
    rules
}

/// Split `paths:` globs off a rule file's frontmatter; the rest is body.
fn split_rule_frontmatter(text: &str) -> (Option<Vec<String>>, String) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, text.to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (None, text.to_string());
    };
    let mut globs: Option<Vec<String>> = None;
    for line in rest[..end].lines() {
        if let Some(v) = line.strip_prefix("paths:") {
            let list: Vec<String> = v
                .split(',')
                .map(str::trim)
                .filter(|g| !g.is_empty())
                .map(str::to_string)
                .collect();
            if !list.is_empty() {
                globs = Some(list);
            }
        }
    }
    (globs, rest[end + 4..].to_string())
}

/// One discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Skill name (directory name).
    pub name: String,
    /// Absolute SKILL.md path (the model reads this on demand).
    pub path: PathBuf,
    /// One-line description from frontmatter.
    pub description: String,
    /// EXT-07 `disable-model-invocation: true`: the skill never enters
    /// the prompt's skill listing — only an explicit `/skill:<name>`
    /// may load it.
    pub disable_model_invocation: bool,
    /// EXT-07 `paths:` globs: the skill activates only once a file the
    /// session has read matches one of them (matched against the full
    /// path and the file name, like rule files). None = always on.
    pub paths: Option<Vec<String>>,
    /// EXT-07 `max_tokens:`: the body cap when the skill is injected,
    /// with a truncation note. None = uncapped.
    pub max_tokens: Option<usize>,
}

/// The SKILL.md frontmatter ka reads (no YAML dep, line-based like the
/// rule-file parser).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SkillFrontmatter {
    pub description: Option<String>,
    pub disable_model_invocation: bool,
    pub paths: Option<Vec<String>>,
    pub max_tokens: Option<usize>,
}

/// Parse a SKILL.md's `---` frontmatter block: `description`,
/// `disable-model-invocation` (bool), `paths` (comma/space-separated
/// globs), `max_tokens` (integer). Unknown keys are tolerated
/// (agentskills.io convention); an unterminated block is no
/// frontmatter.
pub fn parse_skill_frontmatter(content: &str) -> (SkillFrontmatter, String) {
    let mut fm = SkillFrontmatter::default();
    let Some(rest) = content.strip_prefix("---\n") else {
        return (fm, content.to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (fm, content.to_string());
    };
    for line in rest[..end].lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "description" => {
                let v = value.trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    fm.description = Some(v.to_string());
                }
            }
            "disable-model-invocation" | "disable_model_invocation" => {
                fm.disable_model_invocation = value.eq_ignore_ascii_case("true");
            }
            "paths" => {
                let globs: Vec<String> = value
                    .trim_matches('[')
                    .trim_matches(']')
                    .split([',', ' '])
                    .map(str::trim)
                    .filter(|g| !g.is_empty())
                    .map(str::to_string)
                    .collect();
                if !globs.is_empty() {
                    fm.paths = Some(globs);
                }
            }
            "max_tokens" | "max-tokens" => {
                if let Ok(n) = value.trim().parse::<usize>() {
                    fm.max_tokens = Some(n);
                }
            }
            _ => {}
        }
    }
    (
        fm,
        rest[end + 4..]
            .trim_start_matches('\n')
            .trim_end()
            .to_string(),
    )
}

/// Discover SKILL.md skills across ka-native and ecosystem directories.
/// Progressive disclosure: only name+description+path reach the prompt.
pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    if bare_mode() {
        return Vec::new();
    }
    let project_trusted = crate::trust::project_trusted(cwd);
    let home = std::env::var("HOME").map(PathBuf::from).ok();
    // project scope anchors at the root project, not the launch dir
    let root = crate::project_root(cwd);
    let project = vec![
        root.join(".ka/skills"),
        root.join(".agents/skills"),
        root.join(".claude/skills"),
    ];
    let user = match &home {
        Some(h) => vec![
            h.join(".config/ka/skills"),
            h.join(".agents/skills"),
            h.join(".claude/skills"),
        ],
        None => Vec::new(),
    };
    discover_skills_scoped(project, user, project_trusted)
}

/// [`discover_skills`] with explicit roots and trust decision: untrusted
/// projects contribute no roots, user scope always loads (tests).
/// Precedence survives the catalog cap: project skills fill the
/// catalog first and user scope only tops up the remainder — a rich
/// user scope (30+ alphabetically-early skills) must never push
/// trusted-project skills out of the 20-entry catalog.
pub fn discover_skills_scoped(
    project: Vec<PathBuf>,
    user: Vec<PathBuf>,
    project_trusted: bool,
) -> Vec<Skill> {
    let project = if project_trusted { project } else { Vec::new() };
    let mut skills = discover_skills_in(project);
    if skills.len() < SKILL_CATALOG_CAP {
        let taken: std::collections::HashSet<String> =
            skills.iter().map(|s| s.name.clone()).collect();
        for skill in discover_skills_in(user) {
            if skills.len() >= SKILL_CATALOG_CAP {
                break;
            }
            if !taken.contains(skill.name.as_str()) {
                skills.push(skill);
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
    }
    skills
}

/// Merge pre-gated project roots with always-on user roots; earlier roots
/// shadow later ones, so project skills win over user skills of the same
/// name. Tests: [`discover_skills`] derives `project` from the trust store.
pub fn discover_in(project: Vec<PathBuf>, user: Vec<PathBuf>) -> Vec<Skill> {
    discover_skills_in(project.into_iter().chain(user).collect())
}

/// Skill discovery against explicit roots (tests).
pub fn discover_skills_in(roots: Vec<PathBuf>) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let skill_md = dir.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !seen.insert(name.clone()) {
                continue; // first root wins (project > user)
            }
            let Ok(content) = std::fs::read_to_string(&skill_md) else {
                continue;
            };
            let (fm, _body) = parse_skill_frontmatter(&content);
            let description = fm
                .description
                .unwrap_or_else(|| "(no description)".to_string());
            skills.push(Skill {
                name,
                path: skill_md,
                description,
                disable_model_invocation: fm.disable_model_invocation,
                paths: fm.paths,
                max_tokens: fm.max_tokens,
            });
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills.truncate(SKILL_CATALOG_CAP);
    skills
}

/// Catalog cap for skill discovery: the model sees at most this many
/// skills per resolution (conventions doc: a curated catalog, not a
/// directory dump).
const SKILL_CATALOG_CAP: usize = 20;
/// Whether a project style resolves (EXT-09): trusted-project
/// `.ka/styles/<name>.md` first, then the user
/// `~/.config/ka/styles/<name>.md`. Both must hold prose identity text
/// (`.md`), styles being `.ka/` customizations are dropped entirely in
/// safe mode.
pub fn resolve_style(name: &str, cwd: &Path) -> Option<PathBuf> {
    if bare_mode() || name.is_empty() {
        return None;
    }
    let trusted = crate::trust::project_trusted(cwd);
    let home = std::env::var("HOME").map(PathBuf::from).ok();
    resolve_style_in(name, cwd, trusted, home.as_deref())
}

/// [`resolve_style`] with explicit trust and home (tests).
pub fn resolve_style_in(
    name: &str,
    cwd: &Path,
    project_trusted: bool,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return None; // style names are bare file stems
    }
    if project_trusted {
        let project = crate::project_root(cwd)
            .join(".ka/styles")
            .join(format!("{name}.md"));
        if project.is_file() {
            return Some(project);
        }
    }
    let user = home.map(|h| h.join(".config/ka/styles").join(format!("{name}.md")));
    match user {
        Some(path) if path.is_file() => Some(path),
        _ => None,
    }
}

/// The style names visible to `/style` (EXT-09): trusted-project
/// `.ka/styles/*.md` stems first, then user `~/.config/ka/styles/*.md`
/// stems, deduplicated (project wins), sorted.
pub fn available_styles(cwd: &Path) -> Vec<String> {
    if bare_mode() {
        return Vec::new();
    }
    let trusted = crate::trust::project_trusted(cwd);
    let home = std::env::var("HOME").map(PathBuf::from).ok();
    available_styles_in(cwd, trusted, home.as_deref())
}

/// [`available_styles`] with explicit trust and home (tests).
pub fn available_styles_in(cwd: &Path, project_trusted: bool, home: Option<&Path>) -> Vec<String> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if project_trusted {
        roots.push(crate::project_root(cwd).join(".ka/styles"));
    }
    if let Some(h) = home {
        roots.push(h.join(".config/ka/styles"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            if let Some(stem) = path.file_stem() {
                seen.insert(stem.to_string_lossy().into_owned());
            }
        }
    }
    seen.into_iter().collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn temp_tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-conv-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn agents_walk_root_to_cwd_prefers_agents_md() {
        let root = temp_tree("agents");
        let mid = root.join("pkg");
        let deep = mid.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(mid.join("CLAUDE.md"), "mid rules (compat)").unwrap();
        std::fs::write(deep.join("AGENTS.md"), "deep rules").unwrap();

        let found = discover_agents(&deep);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].content, "root rules");
        assert_eq!(found[1].content, "mid rules (compat)");
        assert_eq!(found[2].content, "deep rules");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn override_md_replaces_the_directory_layer() {
        let root = temp_tree("override");
        let deep = root.join("sub");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(deep.join("AGENTS.md"), "deep base").unwrap();
        std::fs::write(deep.join("AGENTS.override.md"), "deep override").unwrap();

        // the override replaces its directory's AGENTS.md; the parent
        // layer survives (override scope is exactly one directory)
        let found = discover_agents(&deep);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].content, "root rules");
        assert_eq!(found[1].content, "deep override");
        assert!(found[1].path.ends_with("AGENTS.override.md"));

        // an override alone also counts as the directory's layer
        std::fs::remove_file(deep.join("AGENTS.md")).unwrap();
        let found = discover_agents(&deep);
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].content, "deep override");

        // the override beats the CLAUDE.md compat fallback too
        std::fs::remove_file(deep.join("AGENTS.override.md")).unwrap();
        std::fs::write(deep.join("CLAUDE.md"), "deep compat").unwrap();
        std::fs::write(deep.join("AGENTS.override.md"), "deep override").unwrap();
        let found = discover_agents(&deep);
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].content, "deep override");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fallback_filenames_apply_only_without_agents_md() {
        let root = temp_tree("fallbacks");
        let mid = root.join("pkg");
        let deep = mid.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(mid.join("AGENTS.md"), "mid agents").unwrap();
        std::fs::write(mid.join("CONTEXT.md"), "mid fallback").unwrap();
        std::fs::write(deep.join("CONTEXT.md"), "deep fallback").unwrap();

        // with fallbacks: the deep directory contributes its CONTEXT.md
        let found = discover_agents_with(&deep, &["CONTEXT.md".to_string()]);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].content, "mid agents");
        assert_eq!(found[1].content, "deep fallback");
        assert!(found[1].path.ends_with("CONTEXT.md"));

        // without fallbacks: the deep directory contributes nothing
        let found = discover_agents(&deep);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].content, "mid agents");

        // AGENTS.md still beats a configured fallback in the same dir
        std::fs::write(deep.join("AGENTS.md"), "deep agents").unwrap();
        let found = discover_agents_with(&deep, &["CONTEXT.md".to_string()]);
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].content, "deep agents");

        // fallback names are tried in the configured order
        std::fs::remove_file(deep.join("AGENTS.md")).unwrap();
        std::fs::write(deep.join("NOTES.md"), "deep notes").unwrap();
        let found =
            discover_agents_with(&deep, &["NOTES.md".to_string(), "CONTEXT.md".to_string()]);
        assert_eq!(
            found[1].content, "deep notes",
            "first configured fallback wins"
        );

        // fallbacks lose to the override too
        std::fs::write(deep.join("AGENTS.override.md"), "deep override").unwrap();
        let found = discover_agents_with(&deep, &["NOTES.md".to_string()]);
        assert_eq!(found[1].content, "deep override");

        // empty fallback files never count as found
        std::fs::remove_file(deep.join("AGENTS.override.md")).unwrap();
        std::fs::remove_file(deep.join("NOTES.md")).unwrap();
        std::fs::write(deep.join("CONTEXT.md"), "   \n").unwrap();
        let found = discover_agents_with(&deep, &["CONTEXT.md".to_string()]);
        assert_eq!(
            found.len(),
            1,
            "whitespace-only fallback skipped: {found:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bare_mode_defaults_off() {
        // safe mode is strictly opt-in: no test, layer, or startup path
        // may leave the process-global flag set for the normal run
        assert!(!bare_mode());
    }

    #[test]
    fn skills_discover_with_description() {
        let root = temp_tree("skills");
        let proj = root.join(".ka/skills/deploy");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("SKILL.md"),
            "---\nname: deploy\ndescription: How we ship\n---\nbody",
        )
        .unwrap();
        let user = root.join(".agents/skills/deploy");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join("SKILL.md"), "---\ndescription: shadowed\n---\n").unwrap();

        let skills = discover_skills_in(vec![root.join(".ka/skills"), root.join(".agents/skills")]);
        assert_eq!(skills.len(), 1, "project root wins over user: {skills:?}");
        assert_eq!(skills[0].description, "How we ship");
        assert!(skills[0].path.starts_with(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn user_scope_flood_cannot_displace_project_skills() {
        // regression: the catalog cap sorted alphabetically after the
        // flat merge, so 20+ alphabetically-early user skills pushed a
        // trusted-project skill out of the catalog entirely
        let root = temp_tree("skillflood");
        let proj = root.join("proj/.ka/skills/ship");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("SKILL.md"),
            "---\ndescription: How we ship\n---\nbody",
        )
        .unwrap();
        let user = root.join("user/skills");
        for name in [
            "aa", "ab", "ac", "ad", "ae", "af", "ag", "ah", "ai", "aj", "ak", "al", "am", "an",
            "ao", "ap", "aq", "ar", "as", "at", "au",
        ] {
            let dir = user.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), "---\ndescription: x\n---\n").unwrap();
        }
        let skills = discover_skills_scoped(vec![root.join("proj/.ka/skills")], vec![user], true);
        assert!(
            skills.iter().any(|s| s.name == "ship"),
            "project skill survives a flooded user scope: {:?}",
            skills.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(skills.len() <= SKILL_CATALOG_CAP);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skill_frontmatter_parses_ext07_fields() {
        let (fm, _body) = parse_skill_frontmatter(
            "---\nname: x\ndescription: \"quoted desc\"\ndisable-model-invocation: true\npaths: src/**, *.md\nmax_tokens: 400\n---\nbody",
        );
        assert_eq!(fm.description.as_deref(), Some("quoted desc"));
        assert!(fm.disable_model_invocation);
        assert_eq!(
            fm.paths,
            Some(vec!["src/**".to_string(), "*.md".to_string()])
        );
        assert_eq!(fm.max_tokens, Some(400));

        // tolerant spellings: snake_case aliases, bracketed lists
        let (fm, body) = parse_skill_frontmatter(
            "---\ndisable_model_invocation: true\npaths: [a.md]\nmax-tokens: 12\n---\nthe body",
        );
        assert!(fm.disable_model_invocation);
        assert_eq!(fm.paths, Some(vec!["a.md".to_string()]));
        assert_eq!(fm.max_tokens, Some(12));
        assert_eq!(body, "the body");

        // absent frontmatter / unknown keys: defaults, body intact
        let (fm, body) = parse_skill_frontmatter("no frontmatter");
        assert_eq!(fm, SkillFrontmatter::default());
        assert_eq!(body, "no frontmatter");
        let (fm, _) = parse_skill_frontmatter("---\ncapabilities: [x]\n---\n");
        assert_eq!(fm, SkillFrontmatter::default());
    }

    #[test]
    fn rules_parse_frontmatter_and_scope() {
        let (globs, body) =
            split_rule_frontmatter("---\npaths: **/*.ts, tsconfig.json\n---\nUse pnpm.");
        assert_eq!(
            globs,
            Some(vec!["**/*.ts".to_string(), "tsconfig.json".to_string()])
        );
        assert_eq!(body.trim(), "Use pnpm.");
        // no frontmatter → always active
        let (globs, body) = split_rule_frontmatter("Always on.");
        assert_eq!(globs, None);
        assert_eq!(body.trim(), "Always on.");
        // frontmatter without paths → always active
        let (globs, _) = split_rule_frontmatter("---\nname: no-scope\n---\nBody.");
        assert_eq!(globs, None);
    }

    #[test]
    fn rules_discover_capped_sorted_and_skip_empty() {
        let root = temp_tree("rules");
        let dir = root.join(".ka/rules");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b-styles.md"), "Rule B.").unwrap();
        std::fs::write(dir.join("a-ts.md"), "---\npaths: *.ts\n---\nRule A.").unwrap();
        std::fs::write(dir.join("empty.md"), "   \n").unwrap();
        crate::trust::test_support::with_trust_file(|_| {
            crate::trust::approve(&root);
            let rules = discover_rules(&root);
            let names: Vec<String> = rules
                .iter()
                .map(|r| {
                    r.path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect();
            assert_eq!(
                names,
                vec!["a-ts".to_string(), "b-styles".to_string()],
                "{names:?}"
            );
            assert_eq!(rules[0].globs, Some(vec!["*.ts".to_string()]));
            assert_eq!(rules[1].content, "Rule B.");
            let _ = std::fs::remove_dir_all(&root);
        });
    }

    #[test]
    fn untrusted_project_skills_absent_user_scope_present() {
        let root = temp_tree("gated-untrusted");
        for dir in [
            root.join("proj/.ka/skills/ship"),
            root.join("user/skills/shared"),
        ] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), "---\ndescription: d\n---\n").unwrap();
        }
        crate::trust::test_support::with_trust_file(|_| {
            // untrusted via the real store: the project skill is hidden
            // (user-scope skills from the real HOME may still appear)
            let skills = discover_skills(&root.join("proj"));
            assert!(
                skills.iter().all(|s| s.name != "ship"),
                "untrusted project skill must be absent: {:?}",
                skills.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
            // user scope is never gated
            let skills = discover_skills_scoped(
                vec![root.join("proj/.ka/skills")],
                vec![root.join("user/skills")],
                false,
            );
            let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(
                names,
                vec!["shared"],
                "project hidden, user kept: {names:?}"
            );
        });
    }

    #[test]
    fn trusted_project_skills_load_and_shadow_user() {
        let root = temp_tree("gated-trusted");
        for (dir, desc) in [
            (root.join("proj/.ka/skills/deploy"), "How we ship"),
            (root.join("user/skills/deploy"), "shadowed"),
            (root.join("user/skills/lint"), "user lint"),
        ] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\ndescription: {desc}\n---\n"),
            )
            .unwrap();
        }
        let proj = root.join("proj");
        crate::trust::test_support::with_trust_file(|_| {
            // approve via the shared trust path, exactly like the CLI does
            crate::trust::approve(&proj);
            let skills = discover_skills(&proj);
            let by_name: std::collections::HashMap<&str, &str> = skills
                .iter()
                .map(|s| (s.name.as_str(), s.description.as_str()))
                .collect();
            // user roots still load through discover_skills' HOME derivation
            let scoped = discover_skills_scoped(
                vec![proj.join(".ka/skills")],
                vec![root.join("user/skills")],
                crate::trust::project_trusted(&proj),
            );
            let by_name_scoped: std::collections::HashMap<String, String> = scoped
                .into_iter()
                .map(|s| (s.name, s.description))
                .collect();
            assert_eq!(
                by_name_scoped.get("deploy").map(String::as_str),
                Some("How we ship"),
                "project shadows user"
            );
            assert_eq!(
                by_name_scoped.get("lint").map(String::as_str),
                Some("user lint"),
                "user scope still loads"
            );
            // and the real store approved above: project deploy is present
            assert_eq!(
                by_name.get("deploy").map(|s| &**s),
                Some("How we ship"),
                "approved project contributes its skills: {by_name:?}"
            );
        });
    }
}
