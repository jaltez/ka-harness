//! `ka schedule add|list|remove` (AUTO-03): manage crontab entries that
//! invoke `ka run` headlessly. Every entry is one crontab line whose
//! command part is `ka run --print text "<prompt>"` and whose tail
//! carries the ownership marker `# ka-schedule <name>` — the marker is
//! what `list` filters on and what `remove <name>` matches, so entries
//! the user added by hand (or edits later) are never touched.
//!
//! Nothing is installed unless `--install` is passed: by default the
//! commands only print (add: the exact crontab line to paste; remove:
//! the resulting full crontab) so a cautious `crontab -e` workflow stays
//! the primary path. `--install` pipes the merged crontab into
//! `crontab -` in one write — the user's existing lines are preserved
//! verbatim, never reordered.

use std::io::Write;
use std::process::ExitCode;

/// The marker that tags a crontab line as ka-managed.
pub(crate) const MARKER: &str = "# ka-schedule ";

/// `ka schedule <cmd>`
#[derive(clap::Subcommand, Clone, Debug)]
pub enum ScheduleCmd {
    /// Build the crontab line for one ka run (print it, or install it)
    Add {
        /// Five-field cron expression (e.g. "30 9 * * 1-5")
        cron: String,
        /// Prompt text handed to `ka run --print text`
        prompt: String,
        /// Entry name for the `# ka-schedule` marker (default: a slug
        /// of the prompt); must be unique across the crontab
        #[arg(long, value_name = "NAME")]
        session_name: Option<String>,
        /// Write the merged crontab via `crontab -` (default: print only)
        #[arg(long)]
        install: bool,
    },
    /// Print the crontab lines carrying the ka-schedule marker
    List,
    /// Remove the ka-schedule entry named NAME (print the resulting
    /// crontab, or install it with --install)
    Remove {
        /// The name after the `# ka-schedule` marker (see `ka schedule list`)
        name: String,
        /// Write the resulting crontab via `crontab -` (default: print only)
        #[arg(long)]
        install: bool,
    },
}

/// One ka-managed entry parsed back out of a crontab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub name: String,
    pub line: String,
}

/// Run `ka schedule <cmd>`.
pub fn run(cmd: ScheduleCmd) -> Result<ExitCode, String> {
    match cmd {
        ScheduleCmd::Add {
            cron,
            prompt,
            session_name,
            install,
        } => {
            let name = match session_name {
                Some(n) => n,
                None => slug(&prompt),
            };
            let line = schedule_line(&cron, &prompt, &name)?;
            let current = read_crontab()?;
            if parse_entries(&current).iter().any(|e| e.name == name) {
                return Err(format!(
                    "a ka-schedule entry named {name:?} already exists — \
                     pick another name or remove the old one first"
                ));
            }
            if install {
                // a user crontab without a trailing newline would glue the
                // new entry onto the last existing line — normalize first
                let mut merged = current;
                if !merged.is_empty() && !merged.ends_with('\n') {
                    merged.push('\n');
                }
                merged.push_str(&line);
                merged.push('\n');
                install_crontab(&merged)?;
                println!("installed: {line}");
            } else {
                println!("add this line to your crontab (or re-run with --install):");
                println!("{line}");
            }
            Ok(ExitCode::SUCCESS)
        }
        ScheduleCmd::List => {
            let current = read_crontab()?;
            let entries = parse_entries(&current);
            if entries.is_empty() {
                println!("(no ka-schedule entries in the crontab)");
            } else {
                for e in &entries {
                    println!("{}  [{}]", e.line, e.name);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        ScheduleCmd::Remove { name, install } => {
            let current = read_crontab()?;
            let (rest, removed) = remove_named(&current, &name);
            if removed == 0 {
                return Err(format!(
                    "no ka-schedule entry named {name:?} in the crontab \
                     (see `ka schedule list`)"
                ));
            }
            if install {
                install_crontab(&rest)?;
                println!("removed {removed} ka-schedule entry named {name:?}");
            } else {
                println!("resulting crontab without {name:?} (re-run with --install to apply):");
                print!("{rest}");
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Build one crontab line: `<cron> ka run --print text -- "<prompt>" # ka-schedule <name>`.
/// Validation is strict where cron or the shell would silently mangle
/// input: five space-separated cron fields using only `[0-9*/,-]`, no
/// newline anywhere (a crontab line IS a line), no `%`/quotes/whitespace
/// in name, and the prompt's cron/shell specials (`"`, `\`, `` ` ``,
/// `$`, `%`) escaped for the double-quoted command string (`%` is
/// newline-to-stdin in cron unless backslashed).
pub(crate) fn schedule_line(cron: &str, prompt: &str, name: &str) -> Result<String, String> {
    if cron.contains(['\n', '\r']) {
        return Err(format!(
            "cron expression {cron:?} must be a single line (no newlines)"
        ));
    }
    let fields: Vec<&str> = cron.split(' ').filter(|s| !s.is_empty()).collect();
    if fields.len() != 5
        || cron
            .chars()
            .any(|c| c != ' ' && !matches!(c, '0'..='9' | '*' | '/' | ',' | '-'))
    {
        return Err(format!(
            "cron expression {cron:?} must have exactly 5 space-separated fields \
             using only [0-9*/,-] (minute hour day month weekday)"
        ));
    }
    if name.is_empty()
        || name.contains(char::is_whitespace)
        || name.contains(['"', '%', '\n', '\r'])
    {
        return Err(format!(
            "entry name {name:?} must be non-empty, whitespace-free, quote-free, and '%'-free"
        ));
    }
    if prompt.contains('\n') || prompt.contains('\r') {
        return Err("prompt must be a single line (no newlines) — a crontab \
                    entry cannot span lines"
            .to_string());
    }
    Ok(format!(
        "{} ka run --print text -- \"{}\" {}{}",
        fields.join(" "),
        shell_quote_prompt(prompt),
        MARKER,
        name
    ))
}

/// Escape `prompt` for one double-quoted shell string inside a crontab:
/// backslash first, then the shell specials (`"`, `` ` ``, `$`), then
/// cron's `%` (which cron folds to a newline unless backslashed — the
/// backslash itself is consumed by cron, so the shell sees a bare `%`).
fn shell_quote_prompt(prompt: &str) -> String {
    prompt
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$")
}

/// Every crontab line with a `# ka-schedule <name>` marker, in order.
/// Lines without the marker are ignored entirely (never parsed, never
/// reported): ka only manages what it tagged.
pub(crate) fn parse_entries(crontab: &str) -> Vec<Entry> {
    crontab
        .lines()
        .filter_map(|line| {
            let idx = line.rfind(MARKER)?;
            let name = line[idx + MARKER.len()..].trim();
            (!name.is_empty()).then(|| Entry {
                name: name.to_string(),
                line: line.to_string(),
            })
        })
        .collect()
}

/// Drop every ka-schedule line named `name`; returns the remaining
/// crontab (original newline style preserved for kept lines) and how
/// many lines went away.
pub(crate) fn remove_named(crontab: &str, name: &str) -> (String, usize) {
    let is_named = |line: &str| {
        line.rfind(MARKER)
            .is_some_and(|i| line[i + MARKER.len()..].trim() == name)
    };
    let mut rest = String::with_capacity(crontab.len());
    let mut removed = 0;
    let mut first = true;
    for line in crontab.lines() {
        if is_named(line) {
            removed += 1;
            continue;
        }
        if !first {
            rest.push('\n');
        }
        rest.push_str(line);
        first = false;
    }
    // a crontab ends with a newline unless the removed line was last and
    // the source did not end with one — keep it re-installable as-is
    if !rest.is_empty() && crontab.ends_with('\n') {
        rest.push('\n');
    }
    (rest, removed)
}

/// A filesystem/crontab-safe slug from the prompt: lowercase ASCII
/// alphanumerics as dashes, collapsed, trimmed — `fix flaky tests!` →
/// `fix-flaky-tests`. Falls back to `ka` for prompts with no usable
/// characters (e.g. non-Latin scripts are transliterated away here on
/// purpose: cron marker names stay ASCII for easy shell handling).
fn slug(prompt: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = true;
    for c in prompt.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    let cut = trimmed.chars().take(48).collect::<String>();
    let cut = cut.trim_end_matches('-').to_string();
    if cut.is_empty() {
        "ka".to_string()
    } else {
        cut
    }
}

/// The current crontab. A missing crontab (cron's "no crontab for" fail)
/// is an empty one — the add path then installs fresh; other failures
/// (no crontab binary at all) are real errors.
fn read_crontab() -> Result<String, String> {
    let out = std::process::Command::new("crontab").arg("-l").output();
    match out {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains("no crontab for") {
                Ok(String::new())
            } else {
                Err(format!("crontab -l: {}", err.trim()))
            }
        }
        Err(e) => Err(format!("crontab: {e} — is a cron daemon installed?")),
    }
}

/// Replace the whole crontab with `text` via `crontab -`.
fn install_crontab(text: &str) -> Result<(), String> {
    let mut child = std::process::Command::new("crontab")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("crontab -: {e}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "crontab -: no stdin pipe".to_string())?
        .write_all(text.as_bytes())
        .map_err(|e| format!("crontab -: {e}"))?;
    let status = child.wait().map_err(|e| format!("crontab -: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("crontab - rejected the new crontab (nothing installed)".to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn line_shape_is_cron_command_marker() {
        let line = schedule_line("30 9 * * 1-5", "fix the flaky tests", "daily").unwrap();
        assert_eq!(
            line,
            "30 9 * * 1-5 ka run --print text -- \"fix the flaky tests\" # ka-schedule daily"
        );
    }

    #[test]
    fn prompt_specials_are_escaped_for_cron_and_shell() {
        let line = schedule_line("* * * * *", "say \"hi\" $USER `id` 50% a\\b", "n").unwrap();
        assert_eq!(
            line,
            "* * * * * ka run --print text -- \"say \\\"hi\\\" \\$USER \\`id\\` 50\\% a\\\\b\" # ka-schedule n"
        );
    }

    #[test]
    fn cron_must_have_five_fields_and_clean_names() {
        assert!(schedule_line("30 9 * *", "p", "n").is_err());
        assert!(schedule_line("30 9 * * * *", "p", "n").is_err());
        assert!(schedule_line("* * * * *", "p", "").is_err());
        assert!(schedule_line("* * * * *", "p", "two words").is_err());
        assert!(schedule_line("* * * * *", "p", "q\"uote").is_err());
        assert!(schedule_line("* * * * *", "two\nlines", "n").is_err());
    }

    #[test]
    fn list_filters_by_marker_only() {
        let crontab = "PATH=/usr/bin\n\
                       0 0 * * * backup-home\n\
                       30 9 * * 1-5 ka run --print text \"standup\" # ka-schedule standup\n\
                       0 * * * * ka run --print text \"hourly sweep\" # ka-schedule hourly\n";
        let entries = parse_entries(crontab);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["standup", "hourly"]);
        assert!(entries[0].line.contains("standup\""));
    }

    #[test]
    fn remove_drops_only_the_named_line() {
        let crontab = "PATH=/usr/bin\n\
                       30 9 * * 1-5 ka run --print text \"standup\" # ka-schedule standup\n\
                       0 * * * * ka run --print text \"hourly sweep\" # ka-schedule hourly\n";
        let (rest, n) = remove_named(crontab, "hourly");
        assert_eq!(n, 1);
        assert!(rest.contains("# ka-schedule standup"));
        assert!(!rest.contains("hourly sweep"));
        assert!(rest.starts_with("PATH=/usr/bin\n"));
        // the untouched entry survives a byte-exact roundtrip
        assert_eq!(
            parse_entries(&rest),
            parse_entries("30 9 * * 1-5 ka run --print text \"standup\" # ka-schedule standup\n")
        );
        // unknown name removes nothing
        assert_eq!(remove_named(crontab, "ghost").1, 0);
    }

    #[test]
    fn add_without_session_name_slugs_the_prompt() {
        let line = schedule_line(
            "0 12 * * *",
            "Tidy the CHANGELOG!",
            &slug("Tidy the CHANGELOG!"),
        )
        .unwrap();
        assert!(line.ends_with("# ka-schedule tidy-the-changelog"), "{line}");
        assert_eq!(slug("   ---  "), "ka");
        assert_eq!(slug("Ünïcödé drops"), "n-c-d-drops");
        assert_eq!(slug("a-b--c"), "a-b-c");
    }

    #[test]
    fn marker_in_prompt_cannot_spoof_entry_name() {
        let line = match schedule_line("0 9 * * *", "check # ka-schedule spoofed", "real") {
            Ok(l) => l,
            Err(e) => panic!("expected valid line: {e}"),
        };
        let entries = parse_entries(&line);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "real");
        let (rest, removed) = remove_named(&line, "spoofed");
        assert_eq!(removed, 0);
        assert_eq!(rest, line);
        let (rest, removed) = remove_named(&line, "real");
        assert_eq!(removed, 1);
        assert!(rest.is_empty());
    }
}
