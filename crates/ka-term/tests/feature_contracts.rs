//! TUI feature contracts: the slash-command registry, transcript
//! rewind, and the memory-inbox review flow — the user-facing surface
//! documented in the README. A failing test here means a documented
//! command or interaction broke; update docs + contract together.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use ka_protocol::Command;
use ka_term::tui::{
    Inventory, ModalKind, NotifySettings, SKILLS, SessionPicker, Transcript, accept_memory_note,
    memory_modal_rows, read_memory_inbox, skill_invocation, slash_command, update_suggestions,
    write_memory_inbox,
};

/// Every built-in slash command documented in the README parses. A
/// command removed or renamed without updating this list is a breaking
/// UX change. (The list is a superset of the README's — it pins every
/// builtin the TUI ships, documented or not.)
#[test]
fn every_documented_slash_command_parses() {
    // handled by slash_command dispatch (the modal/event layer)
    let documented = [
        "/quit",
        "/exit",
        "/model",
        "/provider",
        "/mode",
        "/plan",
        "/thinking",
        "/debug",
        "/build",
        "/review",
        "/tasks",
        "/approve",
        "/rewind",
        "/fork",
        "/checkpoint",
        "/restore",
        "/compact",
        "/shake",
        "/autocompact",
        "/session",
        "/resume",
        "/new",
        "/undo",
        "/spills",
        "/export",
        "/help",
        "/settings",
        "/usage",
        "/context",
        "/key",
        "/mcp",
        "/prompt",
        "/tree",
        "/memory",
    ];
    let mut missing = Vec::new();
    for cmd in documented {
        if slash_command(cmd).is_none() {
            missing.push(cmd);
        }
    }
    assert!(
        missing.is_empty(),
        "documented slash commands no longer parse: {missing:?}"
    );
    // input-layer commands are intercepted before slash_command (local
    // transcript/copy state, no engine roundtrip): /find /retry
    // /copy /clip, and the /skill:<name> family (validated
    // against the inventory, dispatched as a scoped Prompt — see
    // skill_invocation_scopes_the_turn), plus the local faces of
    // /focus (view toggle) and bare /autocompact (display; the
    // session's trigger value lives in the app loop). Mouse capture
    // toggles with Ctrl+M, not a slash command. They are covered by
    // the input-layer unit tests in src; if you move one into
    // slash_command, add it above.
}

/// CTX-02: /shake parses bare (stale-output-only trim) and with modes
/// that ADD to it; unknown words are refused with a usage note rather
/// than silently read as a bare shake.
#[test]
fn shake_parses_modes_and_refuses_unknown_words() {
    let bare = slash_command("/shake").expect("/shake");
    assert!(matches!(
        bare.event,
        Some(ka_protocol::Command::Shake {
            thinking: false,
            images: false
        })
    ));
    assert!(bare.note.is_some(), "the transcript confirms the shake");

    let modes = |args: &str| match slash_command(&format!("/shake {args}")) {
        Some(s) => match s.event {
            Some(ka_protocol::Command::Shake { thinking, images }) => (thinking, images),
            other => panic!("/shake {args} carried {other:?}"),
        },
        None => panic!("/shake {args} must parse"),
    };
    assert_eq!(modes("thinking"), (true, false));
    assert_eq!(modes("images"), (false, true));
    assert_eq!(modes("thinking images"), (true, true));

    let bad = slash_command("/shake bogus").expect("usage row still parses");
    assert!(bad.event.is_none(), "an unknown mode must not shake");
    assert!(
        bad.note.as_deref().is_some_and(|n| n.starts_with("usage:")),
        "{:?}",
        bad.note
    );
}

/// CTX-12: /autocompact <1-100|off> carries SetAutoCompact; out-of-range
/// or non-numeric values refuse with a usage note; the bare form is a
/// display-only parse (the app loop prints the session's value).
#[test]
fn autocompact_parses_percent_off_and_usage() {
    let s = slash_command("/autocompact 80").expect("/autocompact N");
    assert!(matches!(
        s.event,
        Some(ka_protocol::Command::SetAutoCompact { pct: Some(80) })
    ));
    let s = slash_command("/autocompact off").expect("/autocompact off");
    assert!(matches!(
        s.event,
        Some(ka_protocol::Command::SetAutoCompact { pct: None })
    ));
    for bad in ["/autocompact 0", "/autocompact 101", "/autocompact abc"] {
        let s = slash_command(bad).expect("usage row still parses");
        assert!(s.event.is_none(), "{bad} must not carry an event");
        assert!(
            s.note.as_deref().is_some_and(|n| n.starts_with("usage:")),
            "{bad}: {:?}",
            s.note
        );
    }
    let bare = slash_command("/autocompact").expect("/autocompact");
    assert!(bare.event.is_none(), "bare /autocompact only displays");
}

/// Argument-bearing forms parse the way the docs show.
#[test]
fn slash_commands_accept_documented_arguments() {
    // /model with a selector
    let s = slash_command("/model anthropic/claude-sonnet-5@high").expect("/model <sel>");
    assert!(
        matches!(s.event, Some(ka_protocol::Command::SetModel { selector })
        if selector == "anthropic/claude-sonnet-5@high")
    );
    // /rewind with a count
    let s = slash_command("/rewind 3").expect("/rewind N");
    assert!(matches!(
        s.event,
        Some(ka_protocol::Command::Rewind { turns: 3 })
    ));
    // /thinking with a level (session-scoped, like /mode <tier>)
    let s = slash_command("/thinking high").expect("/thinking <level>");
    assert!(
        matches!(s.event, Some(ka_protocol::Command::SetEffort { level })
            if level == ka_protocol::Effort::High)
    );
    // /tasks carries the snapshot command
    let s = slash_command("/tasks").expect("/tasks");
    assert!(matches!(s.event, Some(ka_protocol::Command::ListTasks)));
    // unknown commands do not parse
    assert!(slash_command("/definitely-not-a-command").is_none());
}

/// The picker-backed commands open modals (mode picker, session picker,
/// settings panel, memory view) rather than emitting engine commands.
#[test]
fn modal_slash_commands_open_pick() {
    for cmd in [
        "/mode",
        "/thinking",
        "/session",
        "/settings",
        "/memory",
        "/help",
    ] {
        let s = slash_command(cmd).unwrap_or_else(|| panic!("{cmd}"));
        assert!(
            s.modal.is_some(),
            "{cmd} must open a modal (event: {:?}, quit: {})",
            s.event,
            s.quit
        );
    }
    assert!(matches!(
        slash_command("/memory").unwrap().modal,
        Some(ModalKind::Memory)
    ));
}

/// Transcript rewind: drops everything from the nth-from-last user
/// message, returns the dropped prompt, refuses impossible cuts.
#[test]
fn transcript_rewind_user_contract() {
    let mut t = Transcript::default();
    t.push(ka_term::tui::Line::User("one".into()));
    t.push(ka_term::tui::Line::Assistant("a1".into()));
    t.push(ka_term::tui::Line::User("two".into()));
    t.push(ka_term::tui::Line::Assistant("a2".into()));
    let before = t.total_rows();

    // too few user messages → refused, nothing changes
    assert_eq!(t.rewind_user(3), None);
    assert_eq!(t.total_rows(), before);

    // rewind to the 2nd-from-last user message ("one"): everything from
    // that row onward is dropped, and the dropped prompt comes back
    let dropped = t.rewind_user(2).expect("two user messages exist");
    assert_eq!(dropped, "one");
    let entries = t.entries();
    assert!(
        entries
            .iter()
            .all(|l| !matches!(l, ka_term::tui::Line::User(u) if u == "one" || u == "two")),
        "both user rows and everything after are gone: {entries:?}"
    );

    // dropping the most recent user message only
    let mut t = Transcript::default();
    t.push(ka_term::tui::Line::User("keep".into()));
    t.push(ka_term::tui::Line::User("drop".into()));
    let dropped = t.rewind_user(1).expect("one user message");
    assert_eq!(dropped, "drop");
    assert_eq!(t.entries().len(), 1, "only the first user row survives");
}

/// Memory inbox: staging persists, accept appends to the project
/// MEMORY.md and drains the inbox, discard just drains.
#[test]
fn memory_inbox_roundtrip_and_accept() {
    let dir = std::env::temp_dir().join(format!("ka-term-inbox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // staged notes round-trip
    let staged = vec![
        "- [1700000000] prefer parking_lot locks".to_string(),
        "- [1700000001] never commit ka-harness-release.key".to_string(),
    ];
    write_memory_inbox(&dir, &staged);
    assert_eq!(read_memory_inbox(&dir), staged, "staged notes round-trip");
    assert!(dir.join(".ka/memory/inbox.md").exists());

    // the modal rows show the memory tiers alongside the inbox
    let rows = memory_modal_rows(&dir);
    assert!(rows.is_empty() || rows.iter().all(|r| !r.is_empty()));

    // accept appends the note to ./MEMORY.md (text only, no stamp);
    // the caller (the TUI key handler) then drains the inbox
    let line = staged[0].clone();
    accept_memory_note(&dir, &line, false).unwrap();
    let memory = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    assert!(memory.contains("prefer parking_lot locks"), "{memory:?}");
    assert!(
        !memory.contains("1700000000"),
        "stamps are stripped: {memory:?}"
    );
    write_memory_inbox(&dir, &staged[1..]);
    assert_eq!(read_memory_inbox(&dir), vec![staged[1].clone()]);

    // discard drains without writing memory
    write_memory_inbox(&dir, &[staged[1].clone()]);
    accept_memory_note(&dir, "not-a-stamped-line", false).unwrap_or_else(|e| {
        // unstaged-shaped lines are tolerated (whole line becomes the note)
        let _ = e;
    });
    write_memory_inbox(&dir, &[]);
    assert!(
        !dir.join(".ka/memory/inbox.md").exists(),
        "empty inbox is removed"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Memory flows anchor at the root project: a session launched in a
/// subdirectory of a git repo stages, reads, drains, and accepts into
/// the repo root's `.ka/memory/inbox.md` and `MEMORY.md`.
#[test]
fn memory_inbox_anchors_at_git_root() {
    let root = std::env::temp_dir().join(format!("ka-term-inbox-root-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let deep = root.join("deep").join("nested");
    std::fs::create_dir_all(&deep).unwrap();

    // the engine stages at the root; the TUI reads the same file
    std::fs::create_dir_all(root.join(".ka/memory")).unwrap();
    std::fs::write(
        root.join(".ka/memory/inbox.md"),
        "- [1700000000] prefer parking_lot locks\n",
    )
    .unwrap();
    let staged = read_memory_inbox(&deep);
    assert_eq!(
        staged,
        vec!["- [1700000000] prefer parking_lot locks".to_string()],
        "subdir launch reads the root inbox"
    );

    // acceptance writes the project MEMORY.md at the root
    accept_memory_note(&deep, &staged[0], false).unwrap();
    let memory = std::fs::read_to_string(root.join("MEMORY.md")).unwrap();
    assert!(memory.contains("prefer parking_lot locks"), "{memory:?}");
    assert!(!deep.join("MEMORY.md").exists(), "no memory in the subdir");

    // draining removes the root inbox, not a subdir copy
    write_memory_inbox(&deep, &[]);
    assert!(!root.join(".ka/memory/inbox.md").exists());

    let _ = std::fs::remove_dir_all(&root);
}

/// Notification settings defaults: the bell is on unless disabled, and
/// the notify command is optional.
#[test]
fn notify_settings_contract() {
    // the struct default is inert; the on-by-default contract lives in
    // the config layer the CLI resolves before run()
    let s = NotifySettings::default();
    assert!(s.command.is_none());
    assert!(
        ka_engine::config::Config::default().effective_bell(),
        "[tui] bell defaults to on (terminals mute by user choice)"
    );
}

/// Phase 9.7 contract: custom commands carry frontmatter —
/// `argument-hint` shows in the popup, `allowed-tools` scopes the
/// turn's toolset, `model` overrides the turn's model (never
/// persisted). Documented in the README's slash-commands paragraph.
#[test]
fn custom_command_frontmatter_scopes_the_turn() {
    use ka_term::tui::{parse_command_md, slash_command};

    // the pure parser: every documented key, hyphen or underscore
    let (fm, body) = parse_command_md(
        "---\ndescription: audit the tree\nargument-hint: path\nallowed-tools: read, grep glob\nmodel: zai/glm-5.3@low\n---\nAudit $ARGUMENTS\n",
    );
    assert_eq!(fm.description, "audit the tree");
    assert_eq!(fm.argument_hint, "path");
    assert_eq!(fm.allowed_tools, vec!["read", "grep", "glob"]);
    assert_eq!(fm.model.as_deref(), Some("zai/glm-5.3@low"));
    assert_eq!(body, "Audit $ARGUMENTS");
    // no frontmatter: body passes through, description falls back
    let (fm, body) = parse_command_md("Just a body\n");
    assert!(fm.allowed_tools.is_empty() && fm.model.is_none());
    assert_eq!(body, "Just a body");
    assert_eq!(fm.description, "Just a body");

    // the wire: a user-dir command with frontmatter dispatches a Prompt
    // carrying both scopes
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return; // nothing to install against
    }
    // KNOWN COUPLING: the user-commands scan reads the real
    // ~/.config/ka (no injectable root at the tui.rs source); the
    // fixture uses a distinctive stem and removes both file and dir
    // so nothing of the user's is touched or left behind
    let dir = std::path::PathBuf::from(&home).join(".config/ka/commands");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("ka-contract-frontmatter.md");
    std::fs::write(
        &file,
        "---\nallowed-tools: read, grep\nmodel: test/m\n---\nLook at $ARGUMENTS\n",
    )
    .unwrap();
    let slash = slash_command("/ka-contract-frontmatter audit.md");
    std::fs::remove_file(&file).unwrap();
    let _ = std::fs::remove_dir(&dir);
    let slash = slash.expect("user-dir command dispatches");
    match slash.event {
        Some(ka_protocol::Command::Prompt {
            allowed_tools,
            model,
            text,
            ..
        }) => {
            assert_eq!(
                allowed_tools.as_deref(),
                Some(["read".to_string(), "grep".to_string()].as_slice()),
                "allowed-tools rides the prompt"
            );
            assert_eq!(model.as_deref(), Some("test/m"), "model rides the prompt");
            assert_eq!(text, "Look at audit.md");
        }
        other => panic!("expected a scoped prompt, got {other:?}"),
    }
}

/// `/skill:<name> [args]` dispatches a Prompt scoped to that skill
/// (the engine injects the SKILL.md body into the turn's system
/// prompt); args become the prompt text, a bare invocation gets a
/// deterministic fallback, and unknown/disabled skills refuse with a
/// message naming the toggle — never a hollow turn.
#[test]
fn skill_invocation_scopes_the_turn() {
    let mut inv = Inventory {
        skills: vec!["pdf".to_string(), "commit-style".to_string()],
        ..Default::default()
    };

    // with args: the args are the prompt, the name rides `skills`
    let cmd = skill_invocation("/skill:pdf merge a.pdf b.pdf", &inv)
        .expect("a /skill: line")
        .expect("pdf is offered");
    match cmd {
        Command::Prompt { text, skills, .. } => {
            assert_eq!(text, "merge a.pdf b.pdf");
            assert_eq!(skills, vec!["pdf".to_string()]);
        }
        other => panic!("expected a scoped prompt, got {other:?}"),
    }

    // bare: a minimal deterministic prompt, not an empty one
    let cmd = skill_invocation("/skill:commit-style", &inv)
        .unwrap()
        .unwrap();
    match cmd {
        Command::Prompt { text, skills, .. } => {
            assert_eq!(text, "Use the commit-style skill.");
            assert_eq!(skills, vec!["commit-style".to_string()]);
        }
        other => panic!("expected a scoped prompt, got {other:?}"),
    }

    // hyphenated names parse (no space in the name, rest is args)
    let cmd = skill_invocation("/skill:commit-style  conventional", &inv)
        .unwrap()
        .unwrap();
    assert!(matches!(
        cmd,
        Command::Prompt { ref text, ref skills, .. }
            if text == "conventional" && skills == &vec!["commit-style".to_string()]
    ));

    // unknown skill refuses loudly
    let err = skill_invocation("/skill:nope do things", &inv)
        .unwrap()
        .unwrap_err();
    assert!(err.contains("no such skill: nope"), "{err}");

    // disabled skill names the toggle (shared spec grammar). The
    // engine's inventory already dropped it from `skills` (emit_inventory
    // filters); `disabled` explains why.
    inv.skills = vec!["commit-style".to_string()];
    inv.disabled = vec!["skill:pdf".to_string()];
    let err = skill_invocation("/skill:pdf x", &inv).unwrap().unwrap_err();
    assert!(err.contains("skill:pdf"), "names the toggle: {err}");

    // whole tier off names the tier (no skills listed at all)
    inv.skills = Vec::new();
    inv.disabled = vec!["skills".to_string()];
    let err = skill_invocation("/skill:pdf x", &inv).unwrap().unwrap_err();
    assert!(err.contains("/features on skills"), "{err}");

    // a missing name is a usage error, not a fall-through
    let err = skill_invocation("/skill:", &inv).unwrap().unwrap_err();
    assert!(err.contains("/skill:<name>"), "{err}");

    // bare `/skill` teaches the form and lists the offer (like bare
    // /agents); the space form is taught too, never silently invoked
    inv.skills = vec!["pdf".to_string()];
    inv.disabled = Vec::new();
    let err = skill_invocation("/skill", &inv).unwrap().unwrap_err();
    assert!(
        err.contains("/skill:<name>") && err.contains("pdf"),
        "teaches + lists: {err}"
    );
    let err = skill_invocation("/skill pdf", &inv).unwrap().unwrap_err();
    assert!(err.contains("/skill:<name>"), "space form taught: {err}");
    // nothing discovered: an honest empty note
    inv.skills = Vec::new();
    let err = skill_invocation("/skill", &inv).unwrap().unwrap_err();
    assert!(err.contains("no skills"), "{err}");

    // anything else is not a skill invocation (slash dispatch handles it)
    assert!(skill_invocation("/model", &inv).is_none());
    assert!(skill_invocation("plain text", &inv).is_none());
}

/// The slash popup completes the /skill family in stages: `/sk…`
/// surfaces the `/skill` starter, the inventory's `/skill:<name>`
/// entries wait for the colon.
#[test]
fn skill_popup_completes_from_inventory() {
    *SKILLS.write().unwrap() = vec!["pdf".to_string(), "commit-style".to_string()];
    // /sk → just the starter (the names wait for the colon)
    // (from "/sk" on the starter is the only /s* match; "/s" alone
    // also offers /sandbox /settings /session /spills)
    for probe in ["/sk", "/skil", "/skill"] {
        let popup =
            update_suggestions(probe).unwrap_or_else(|| panic!("{probe} shows the starter"));
        let names: Vec<&str> = popup.items.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["/skill"], "{probe}: {names:?}");
    }
    // past the colon: the inventory names, starter gone
    let popup = update_suggestions("/skill:").expect("offers the inventory skills");
    let names: Vec<&str> = popup.items.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec!["/skill:pdf", "/skill:commit-style"],
        "{names:?}"
    );
    // prefix filter narrows as the user types the name
    let popup = update_suggestions("/skill:com").expect("prefix narrows");
    let names: Vec<&str> = popup.items.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["/skill:commit-style"], "{names:?}");
    // unrelated prefixes stay quiet
    assert!(update_suggestions("/sx").is_none());
    *SKILLS.write().unwrap() = Vec::new();
}

/// CTX-11: /focus collapses everything before the turn in progress to
/// one `· <first words>` digest row per turn; the current turn renders
/// in full. Toggling (or Esc in the app) restores the exact full view —
/// no entry is ever lost by the view. Session-scoped: nothing persists.
#[test]
fn focus_collapses_finished_turns_and_restores() {
    use ka_term::tui::Line;
    let mut t = Transcript::default();
    t.set_width(80);
    t.push(Line::User("first prompt about the parser".into()));
    t.push(Line::Assistant("reply one".into()));
    t.push(Line::Thought("musing".into()));
    t.push(Line::User("second prompt".into()));
    t.push(Line::Assistant("reply two".into()));
    let full = t.total_rows();

    t.set_focus(true);
    let focused = t.total_rows();
    assert!(
        focused < full,
        "focus must collapse something ({focused} vs {full})"
    );
    // row 0 is turn one's digest; the rest is the live turn, whole
    let digest = t.row(0).expect("digest row first");
    let text: String = digest.spans.iter().map(|s| s.content.to_string()).collect();
    assert!(text.starts_with("· "), "digest row: {text:?}");
    assert!(text.contains("first prompt"), "digest: {text:?}");
    assert!(t.row(focused).is_none(), "no rows past the focused view");

    // digest rows carry no click target; live-turn rows still map
    assert_eq!(t.row_ref_at(0), None, "a digest is not a toggle target");
    assert!(t.row_ref_at(1).is_some(), "live rows keep their mapping");

    // the ▲▼ jump anchors: turn one on its digest, turn two on its rows
    let jumps = t.user_entry_rows();
    assert_eq!(jumps.len(), 2, "one anchor per turn: {jumps:?}");
    assert_eq!(jumps[0], 0, "turn one anchors on its digest row");

    // search reports in focus rows and finds the collapsed prompt
    let (entry, at) = t.find_from(0, "parser").expect("digest text is searchable");
    assert_eq!(at, 0, "the hit lands on the digest row");

    // nothing hidden is lost: unfocus restores the exact view
    t.set_focus(false);
    assert_eq!(t.total_rows(), full);
    assert_eq!(t.entries().len(), 5);
    let _ = (entry, jumps);
}

/// SES-02+08: the session picker matches the memorable name in the live
/// filter, floats pinned strands (★) above the recency order, and shows
/// a stored recap (truncated) on the detail line.
#[test]
fn session_picker_pins_names_and_recaps() {
    let summary = |id: &str, title: &str| ka_strand::StrandSummary {
        path: std::path::PathBuf::from(format!("/tmp/{id}.jsonl")),
        id: id.to_string(),
        ts: "2026-01-01T00:00:00Z".to_string(),
        title: title.to_string(),
        messages: 2,
        cost: 0.0,
        tokens: 0,
        parent: None,
        name: None,
        pinned: false,
        recap: None,
    };
    let mut pinned = summary("s1-old", "pinned work");
    pinned.pinned = true;
    pinned.recap = Some("a very long recap that keeps going and should be cut off".into());
    let mut named = summary("s2-mid", "untitled work");
    named.name = Some("fox".into());
    let newest = summary("s3-new", "brand new session");

    let picker = SessionPicker {
        // engine order: newest first — the pin must still float up
        sessions: vec![newest, named, pinned],
        selected: 0,
        filter: String::new(),
        current: None,
    };
    let rows = picker.rows();
    assert!(
        rows[1].0.contains("★ ") && rows[1].0.contains("pinned work"),
        "pinned floats first with a star: {:?}",
        rows[1].0
    );
    assert!(picker.pick().is_none(), "row 0 stays new-session");
    let mut first = picker.clone();
    first.selected = 1;
    assert_eq!(
        first.pick().as_deref(),
        Some("s1-old"),
        "Enter targets the pinned row"
    );
    // the recap rides the detail line, truncated to one line
    assert!(
        rows[1].1.contains("a very long recap"),
        "recap shown: {:?}",
        rows[1].1
    );
    assert!(
        !rows[1].1.contains("cut off"),
        "long recaps truncate: {:?}",
        rows[1].1
    );

    // the live filter matches title, memorable name and id
    let mut by_name = picker.clone();
    by_name.filter_typed("FOX");
    assert_eq!(
        by_name.pick().as_deref(),
        Some("s2-mid"),
        "name match is case-insensitive"
    );
    let mut by_id = picker.clone();
    by_id.filter_typed("s3-new");
    assert_eq!(by_id.pick().as_deref(), Some("s3-new"));
    let mut miss = picker.clone();
    miss.filter_typed("zzz");
    assert_eq!(miss.pick(), None, "no match leaves only new-session");
}
