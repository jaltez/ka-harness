//! `ka install` (roadmap 9.5, generalizing 8.4's `ka skill`): user-scope
//! lifecycle for every installable customization kind — skills,
//! agents, commands, rules. Sources: a git URL (shallow clone;
//! git is already an allowed child) or a local path. No registry, no
//! manifests, no version machinery; the user dirs are global, so
//! running the install IS the trust act, and discovery still gates
//! project-scope content on the project's trust decision.
//! `ka skill install|list|remove` remains as an alias for the skill
//! kind.

use std::path::{Path, PathBuf};

use crate::{InstallCmd, SkillCmd};

/// The installable customization kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Skill,
    Agent,
    Command,
    Rule,
}

impl Kind {
    fn parse(text: &str) -> Result<Self, String> {
        match text {
            "skill" | "skills" => Ok(Kind::Skill),
            "agent" | "agents" => Ok(Kind::Agent),
            "command" | "commands" => Ok(Kind::Command),
            "rule" | "rules" => Ok(Kind::Rule),
            other => Err(format!(
                "unknown kind {other:?} (expected skill | agent | command | rule)"
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::Skill => "skill",
            Kind::Agent => "agent",
            Kind::Command => "command",
            Kind::Rule => "rule",
        }
    }

    /// The file extension this kind installs as (skills are dirs).
    fn extension(self) -> &'static str {
        "md"
    }

    fn all() -> [Kind; 4] {
        [Kind::Skill, Kind::Agent, Kind::Command, Kind::Rule]
    }
}

/// The user customization root (`$HOME/.config/ka`).
fn user_root() -> Result<PathBuf, String> {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "HOME is not set — cannot locate the user config dir")?;
    Ok(home.join(".config/ka"))
}

pub fn run_skill(cmd: SkillCmd) -> Result<std::process::ExitCode, String> {
    let root = user_root()?;
    match cmd {
        SkillCmd::Install { source, force } => install(&root, Kind::Skill, &source, force),
        SkillCmd::List => list(&root, Some(Kind::Skill)),
        SkillCmd::Remove { name } => remove(&root, Kind::Skill, &name),
    }
}

pub fn run_install(cmd: InstallCmd) -> Result<std::process::ExitCode, String> {
    let root = user_root()?;
    match cmd {
        InstallCmd::Install {
            kind,
            source,
            force,
        } => install(&root, Kind::parse(&kind)?, &source, force),
        InstallCmd::List { kind } => match kind {
            Some(text) => list(&root, Some(Kind::parse(&text)?)),
            None => list(&root, None),
        },
        InstallCmd::Remove { kind, name } => remove(&root, Kind::parse(&kind)?, &name),
    }
}

/// The skill name a git URL installs under: its last path segment,
/// minus a trailing `.git` and any leading/trailing non-name junk.
fn name_from_git_url(url: &str) -> Result<String, String> {
    let last = url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("cannot derive a skill name from {url:?}"))?;
    // strip a trailing fragment/query (`git@…:repo.git#x`, `…?t=1`)
    let last = last.split(['#', '?']).next().unwrap_or(last);
    let name = last.strip_suffix(".git").unwrap_or(last);
    let name = name.trim_matches(|c: char| !c.is_alphanumeric());
    if name.is_empty() {
        return Err(format!("cannot derive a skill name from {url:?}"));
    }
    Ok(name.to_string())
}

/// Install one kind from a git URL or local path.
fn install(
    root: &Path,
    kind: Kind,
    source: &str,
    force: bool,
) -> Result<std::process::ExitCode, String> {
    let is_git = source.starts_with("https://")
        || source.starts_with("http://")
        || source.starts_with("git@");
    let mut staging: Option<PathBuf> = None;
    let src: PathBuf = if is_git {
        // Clone into a unique staging root (pid + nanos) so concurrent
        // installs of the same repo and leftover dirs from killed runs
        // cannot collide; the repo dir inside keeps the repo's name so
        // a root-layout SKILL.md still installs under that name.
        let name = name_from_git_url(source)?;
        let staging_root = std::env::temp_dir().join(format!(
            "ka-install-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let tmp = staging_root.join(&name);
        let status = std::process::Command::new("git")
            .args(["clone", "--depth", "1"])
            .arg(source)
            .arg(&tmp)
            .status()
            .map_err(|e| format!("git clone: {e}"))?;
        if !status.success() {
            let _ = std::fs::remove_dir_all(&staging_root);
            return Err(format!("git clone {source} failed"));
        }
        staging = Some(staging_root);
        tmp
    } else {
        PathBuf::from(source)
    };
    let result = install_from(root, kind, &src, force);
    if is_git {
        let _ = staging.as_ref().map(std::fs::remove_dir_all);
    }
    result
}

/// Install from an already-local path: a file installs directly; a
/// directory is searched for exactly one candidate of the kind (a
/// skill's SKILL.md dir, or a root file with the kind's extension).
fn install_from(
    root: &Path,
    kind: Kind,
    src: &Path,
    force: bool,
) -> Result<std::process::ExitCode, String> {
    if src.is_file() && kind == Kind::Skill {
        return Err(
            "skills install from a directory carrying SKILL.md, not a bare file".to_string(),
        );
    }
    let (source_file, target): (Option<PathBuf>, PathBuf) = if src.is_file() {
        let name = src
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| format!("{} has no file name", src.display()))?;
        (Some(src.to_path_buf()), target_for(root, kind, &name))
    } else {
        match kind {
            Kind::Skill => {
                let dir = locate_skill_dir(src)?;
                let name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .ok_or_else(|| format!("{} has no directory name", dir.display()))?;
                (None, target_for(root, Kind::Skill, &name))
            }
            _ => {
                let file = locate_kind_file(src, kind)?;
                let name = file
                    .file_stem()
                    .map(|n| n.to_string_lossy().into_owned())
                    .ok_or_else(|| format!("{} has no file name", file.display()))?;
                (Some(file), target_for(root, kind, &name))
            }
        }
    };
    if target.exists() {
        if !force {
            return Err(format!(
                "{target_name} is already installed — pass --force to overwrite",
                target_name = target.display()
            ));
        }
        if target.is_dir() {
            std::fs::remove_dir_all(&target).map_err(|e| format!("remove old: {e}"))?;
        } else {
            std::fs::remove_file(&target).map_err(|e| format!("remove old: {e}"))?;
        }
    }
    match source_file {
        Some(file) => {
            if let Some(dir) = target.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
            }
            std::fs::copy(&file, &target)
                .map_err(|e| format!("copy {} → {}: {e}", file.display(), target.display()))?;
        }
        None => copy_dir(src, &target)?,
    }
    println!("installed {} → {}", kind.label(), target.display());
    if kind == Kind::Skill {
        let description = read_description(&target.join("SKILL.md")).unwrap_or_default();
        if !description.is_empty() {
            println!("  {description}");
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// Where a kind installs for `name`.
fn target_for(root: &Path, kind: Kind, name: &str) -> PathBuf {
    match kind {
        Kind::Skill => root.join("skills").join(name),
        Kind::Agent => root.join("agents").join(format!("{name}.md")),
        Kind::Command => root.join("commands").join(format!("{name}.md")),
        Kind::Rule => root.join("rules").join(format!("{name}.md")),
    }
}

/// List installed customizations (one kind, or all).
fn list(root: &Path, kind: Option<Kind>) -> Result<std::process::ExitCode, String> {
    let kinds = match kind {
        Some(k) => vec![k],
        None => Kind::all().to_vec(),
    };
    let mut any = false;
    for k in kinds {
        match k {
            Kind::Skill => {
                let dir = root.join("skills");
                for s in ka_engine::conventions::discover_skills_in(vec![dir]) {
                    any = true;
                    println!("skill {} — {}", s.name, s.description);
                }
            }
            _ => {
                let dir = target_for(root, k, "x")
                    .parent()
                    .map(Path::to_path_buf)
                    .ok_or("no target dir")?;
                let ext = k.extension();
                let mut names: Vec<String> = std::fs::read_dir(&dir)
                    .map(|rd| {
                        rd.flatten()
                            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                            .filter(|e| {
                                !e.file_name().to_string_lossy().starts_with('.')
                                    && e.path().extension().map(|x| x == ext).unwrap_or(false)
                            })
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                names.sort();
                for name in names {
                    any = true;
                    println!("{} {}", k.label(), name);
                }
            }
        }
    }
    if !any {
        println!("nothing installed (ka install <skill|agent|command|rule> <git-url|path>)");
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// Remove an installed customization by name.
fn remove(root: &Path, kind: Kind, name: &str) -> Result<std::process::ExitCode, String> {
    if name.trim().is_empty() {
        return Err("name must not be empty".to_string());
    }
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err("name must be bare (no path separators)".to_string());
    }
    let target = target_for(root, kind, name);
    if target.is_dir() {
        std::fs::remove_dir_all(&target).map_err(|e| format!("remove {name}: {e}"))?;
    } else {
        std::fs::remove_file(&target).map_err(|e| format!("remove {name}: {e}"))?;
    }
    println!("removed {} {name}", kind.label());
    Ok(std::process::ExitCode::SUCCESS)
}

/// The skill directory behind a source path: the path itself when it
/// carries SKILL.md, else its single child that does (a repo rooted one
/// level above the skill). Two or more candidates refuse — ambiguous.
fn locate_skill_dir(src: &Path) -> Result<PathBuf, String> {
    if src.join("SKILL.md").is_file() {
        return Ok(src.to_path_buf());
    }
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(src)
        .map_err(|e| format!("read {}: {e}", src.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("SKILL.md").is_file())
        .collect();
    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => Err(format!(
            "{} carries no SKILL.md (skills are directories with a SKILL.md)",
            src.display()
        )),
        _ => {
            let names: Vec<String> = candidates
                .iter()
                .map(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect();
            Err(format!(
                "{} carries several skills ({}); point at one directory",
                src.display(),
                names.join(", ")
            ))
        }
    }
}

/// The single root-level file of a kind's extension inside a source
/// directory (a repo carrying exactly one agent/command/rule).
fn locate_kind_file(src: &Path, kind: Kind) -> Result<PathBuf, String> {
    let ext = kind.extension();
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(src)
        .map_err(|e| format!("read {}: {e}", src.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().map(|x| x == ext).unwrap_or(false))
        .collect();
    candidates.sort();
    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => Err(format!(
            "{} carries no .{ext} file (a {} installs from one {} file)",
            src.display(),
            kind.label(),
            ext
        )),
        _ => Err(format!(
            "{} carries several .{ext} files; point at one file",
            src.display()
        )),
    }
}

/// Recursive directory copy (no deps; skills are small). Symlink
/// entries are skipped — never copy through them.
fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    for entry in std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| format!("read {}: {e}", src.display()))?;
        if entry.file_type().map(|t| t.is_symlink()).unwrap_or(true) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| format!("copy {}: {e}", from.display()))?;
        }
    }
    Ok(())
}

/// The `description:` line of a SKILL.md, if any.
fn read_description(skill_md: &Path) -> Option<String> {
    let text = std::fs::read_to_string(skill_md).ok()?;
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    for line in rest[..end].lines() {
        if let Some(v) = line.strip_prefix("description:") {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-skills-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn install_list_remove_round_trip() {
        let root = temp_dir("root");
        let src = temp_dir("src");
        let skill = src.join("my-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: my-skill\ndescription: does the thing\nlicense: MIT\n---\nbody",
        )
        .unwrap();
        std::fs::write(skill.join("helper.txt"), "extra file").unwrap();

        let target = root.join("skills/my-skill");

        // locate: direct dir with SKILL.md
        assert_eq!(locate_skill_dir(&skill).unwrap(), skill);
        // repo-rooted one level up: the child carrying SKILL.md wins
        assert_eq!(locate_skill_dir(&src).unwrap(), skill);

        let _ = install(&root, Kind::Skill, skill.to_str().unwrap(), false).unwrap();
        assert!(target.join("SKILL.md").is_file());
        assert!(target.join("helper.txt").is_file());

        // re-install without --force refuses
        assert!(install(&root, Kind::Skill, skill.to_str().unwrap(), false).is_err());
        // with --force it overwrites
        let _ = install(&root, Kind::Skill, skill.to_str().unwrap(), true).unwrap();

        // list sees it (discovery over the root's skills dir)
        let skills = ka_engine::conventions::discover_skills_in(vec![root.join("skills")]);
        assert!(skills.iter().any(|s| s.name == "my-skill"));

        let _ = remove(&root, Kind::Skill, "my-skill").unwrap();
        assert!(!target.exists());
        // traversal-safe remove
        assert!(remove(&root, Kind::Skill, "../evil").is_err());
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_refuses_empty_and_whitespace_names() {
        let root = temp_dir("empty-name");
        std::fs::create_dir_all(root.join("skills/dummy")).unwrap();
        for bad in ["", "   ", "\t"] {
            assert!(remove(&root, Kind::Skill, bad).is_err(), "{bad:?}");
        }
        // the skills tree survives the refused removes (no wipe of the kind root)
        assert!(root.join("skills/dummy").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn root_layout_source_installs_under_its_own_name() {
        // a repo-rooted source: SKILL.md at the top, dir named like the repo
        let root = temp_dir("layout-root");
        let repo = temp_dir("repo-src").join("ka-cool-skill");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join("SKILL.md"),
            "---\nname: ka-cool-skill\ndescription: root layout\n---\nbody",
        )
        .unwrap();
        assert_eq!(
            name_from_git_url("git@host:group/repo.git").unwrap(),
            "repo"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(repo.parent().unwrap());
    }

    #[test]
    fn every_kind_installs_lists_and_removes() {
        let root = temp_dir("kinds-root");
        let src = temp_dir("kinds");
        // file kinds: one source file per kind
        std::fs::write(src.join("my-agent.md"), "---\nname: my-agent\n---\nbody").unwrap();
        std::fs::write(src.join("my-command.md"), "body").unwrap();
        std::fs::write(src.join("my-rule.md"), "body").unwrap();

        // direct-file install
        let _ = install(
            &root,
            Kind::Agent,
            src.join("my-agent.md").to_str().unwrap(),
            false,
        )
        .unwrap();
        assert!(root.join("agents/my-agent.md").is_file());

        // dotted name installs and removes symmetrically (my.agent.md → my.agent)
        std::fs::write(src.join("my.agent.md"), "body").unwrap();
        let _ = install(
            &root,
            Kind::Agent,
            src.join("my.agent.md").to_str().unwrap(),
            false,
        )
        .unwrap();
        assert!(root.join("agents/my.agent.md").is_file());
        let _ = remove(&root, Kind::Agent, "my.agent").unwrap();
        assert!(!root.join("agents/my.agent.md").exists());

        // directory install with exactly one candidate of the kind
        let rule_src = temp_dir("rule-src");
        std::fs::write(rule_src.join("my-rule.md"), "body").unwrap();
        let _ = install(&root, Kind::Rule, rule_src.to_str().unwrap(), false).unwrap();
        assert!(root.join("rules/my-rule.md").is_file());

        // a bare file is not a valid skill source
        assert!(
            install(
                &root,
                Kind::Skill,
                rule_src.join("my-rule.md").to_str().unwrap(),
                false
            )
            .is_err()
        );

        // ambiguity refuses (two .md files for the agent kind)
        let two = temp_dir("two-mds");
        std::fs::write(two.join("a.md"), "x").unwrap();
        std::fs::write(two.join("b.md"), "x").unwrap();
        assert!(install(&root, Kind::Agent, two.to_str().unwrap(), false).is_err());
        // zero candidates refuses
        let zero = temp_dir("zero");
        std::fs::create_dir_all(&zero).unwrap();
        assert!(install(&root, Kind::Command, zero.to_str().unwrap(), false).is_err());
        // unknown kind refuses
        assert!(Kind::parse("plugin").is_err());
        assert!(Kind::parse("theme").is_err());

        // list runs over the isolated root (extension/dotfile filtering)
        let _ = list(&root, None).unwrap();

        // remove takes bare names only: empty names and path
        // separators/dot-dot are refused outright, and an extension in
        // the name just fails to match a target (target_for appends it)
        assert!(remove(&root, Kind::Agent, "my-agent.md").is_err());
        let _ = remove(&root, Kind::Agent, "my-agent").unwrap();
        assert!(!root.join("agents/my-agent.md").exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&src);
    }
}
