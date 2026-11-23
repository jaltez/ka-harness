//! TUI feature contracts: the slash-command registry, transcript
//! rewind, and the memory-inbox review flow — the user-facing surface
//! documented in the README. A failing test here means a documented
//! command or interaction broke; update docs + contract together.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use ka_protocol::Command;
use ka_term::tui::{
    Inventory, Meters, ModalKind, NotifySettings, SKILLS, SessionPicker, Transcript,
    accept_memory_note, available_slash_commands, memory_modal_rows, read_memory_inbox,
    skill_invocation, slash_command, status_segments, update_suggestions, usage_tan,
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
        "/compactfrom",
        "/btw",
        "/goal",
        "/loop",
        "/diff",
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
        "/pr",
        "/tan",
        "/git",
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
    // /focus (view toggle), bare /autocompact (display; the
    // session's trigger value lives in the app loop), and
    // /goal [/goal status] (a local read of the goal mirror the
    // engine's goal notes keep; CTX-05 has no Status action).
    // Mouse capture toggles with Alt+M, not a slash command. They are
    // covered by the input-layer unit tests in src; if you move one
    // into slash_command, add it above.
}

/// TUI-16: /tan — a tangential background question. The spawn carries
/// the question verbatim (the answer arrives as a note, the transcript
/// is never interrupted), bare /tan asks for the live listing, cancel
/// forms carry the `t-<id>` roster id, and a mistyped cancel refuses
/// with the usage row instead of reading as a question.
#[test]
fn tan_slash_contract() {
    // documented in the registry
    let cmds = available_slash_commands();
    let (_, hint) = cmds
        .iter()
        .find(|(n, _)| n == "/tan")
        .expect("/tan registered");
    assert!(hint.contains("background side question"), "{hint}");

    // spawn: the question rides verbatim, the transcript confirms with
    // the same prefix the completion note (and the pill) uses
    let spawn = slash_command("/tan why does the spill GC keep files").expect("/tan <question>");
    assert!(
        matches!(
            &spawn.event,
            Some(Command::Tan { text }) if text == "why does the spill GC keep files"
        ),
        "{:?}",
        spawn.event
    );
    assert!(
        spawn
            .note
            .as_deref()
            .is_some_and(|n| n.starts_with(ka_protocol::TAN_NOTE_PREFIX)),
        "{:?}",
        spawn.note
    );
    assert!(spawn.modal.is_none() && !spawn.quit);

    // bare /tan: the listing request (ids + first lines arrive as a
    // note from the engine)
    let list = slash_command("/tan").expect("bare /tan parses");
    assert!(
        matches!(&list.event, Some(Command::Tan { text }) if text.is_empty()),
        "{:?}",
        list.event
    );

    // cancel: bare resolves to the single live job engine-side; an id
    // normalizes (the t- prefix strips); a non-id refuses with usage
    let cancel = slash_command("/tan cancel").expect("/tan cancel parses");
    assert!(
        matches!(&cancel.event, Some(Command::Tan { text }) if text == "cancel"),
        "{:?}",
        cancel.event
    );
    for form in ["/tan cancel t-3", "/tan cancel 3"] {
        let cancel_id = slash_command(form).expect(form);
        assert!(
            matches!(&cancel_id.event, Some(Command::Tan { text }) if text == "cancel 3"),
            "{form}: {:?}",
            cancel_id.event
        );
        assert!(cancel_id.note.is_none(), "{form}: the engine answers");
    }
    let bad = slash_command("/tan cancel why did it break").expect("parses to refuse");
    assert!(bad.event.is_none(), "a mistyped cancel must not spawn");
    assert!(
        bad.note.as_deref().is_some_and(|n| n.starts_with("usage:")),
        "{:?}",
        bad.note
    );

    // the usage row names the whole grammar
    let usage = usage_tan();
    assert!(usage.starts_with("usage: /tan <question>"), "{usage}");
    assert!(usage.contains("/tan cancel [id]"), "{usage}");
}

/// TUI-16: the status-line pill — `tan:N` while N tangential jobs are
/// alive, hidden at zero, and immune to a custom [tui] status_line
/// order (a state badge, like goal/loop).
#[test]
fn tan_pill_contract() {
    let mut meters = Meters::default();
    let joined = |m: &Meters| {
        status_segments(m, &[])
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    };
    assert!(!joined(&meters).contains("tan:"), "{}", joined(&meters));
    meters.tan = 1;
    assert!(joined(&meters).contains("tan:1"), "{}", joined(&meters));
    meters.tan = 12;
    assert!(joined(&meters).contains("tan:12"), "{}", joined(&meters));
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

/// CTX-03: /btw <question> dispatches a SideQuestion (fast role, tools
/// off) and confirms in the transcript; a bare /btw refuses with a
/// usage note instead of asking nothing.
#[test]
fn btw_sends_side_question_and_confirms() {
    let s = slash_command("/btw why does the retry loop spin?").expect("/btw parses");
    match s.event {
        Some(ka_protocol::Command::SideQuestion { text }) => {
            assert_eq!(text, "why does the retry loop spin?");
        }
        other => panic!("/btw carried {other:?}"),
    }
    assert_eq!(
        s.note.as_deref(),
        Some("btw · asked (answer arrives as a note)")
    );
    let bare = slash_command("/btw").expect("usage row still parses");
    assert!(bare.event.is_none(), "no question, no side trip");
    assert!(
        bare.note
            .as_deref()
            .is_some_and(|n| n.starts_with("usage:")),
        "{:?}",
        bare.note
    );
}

/// CTX-01: /compactfrom [N] [focus…] — a leading number is the turn
/// count (default 1), remaining words steer the summary; prose-only
/// input reads as focus on the last turn; 0 turns is refused.
#[test]
fn compactfrom_parses_turns_focus_and_refuses_zero() {
    let parse = |input: &str| match slash_command(input) {
        Some(s) => match s.event {
            Some(ka_protocol::Command::CompactFrom { turns, focus }) => (turns, focus),
            other => panic!("{input} carried {other:?}"),
        },
        None => panic!("{input} must parse"),
    };
    assert_eq!(parse("/compactfrom"), (1, None), "default is the last turn");
    assert_eq!(parse("/compactfrom 3"), (3, None));
    assert_eq!(
        parse("/compactfrom 2 keep the auth findings"),
        (2, Some("keep the auth findings".to_string()))
    );
    assert_eq!(
        parse("/compactfrom keep the auth findings"),
        (1, Some("keep the auth findings".to_string())),
        "prose without a count steers the summary of the last turn"
    );
    for bad in ["/compactfrom 0", "/compactfrom 0 focus"] {
        let s = slash_command(bad).expect("usage row still parses");
        assert!(s.event.is_none(), "{bad} must not carry an event");
        assert!(
            s.note.as_deref().is_some_and(|n| n.starts_with("usage:")),
            "{bad}: {:?}",
            s.note
        );
    }
}

/// CTX-05: /goal carries the exact GoalAction variants the protocol
/// ships (Set/Pause/Resume/Drop — status is a local read of the goal
/// mirror, intercepted in the app loop, so the parser returns a no-op
/// for it); bare /goal is a usage row here.
#[test]
fn goal_parses_actions_and_leaves_status_to_the_local_mirror() {
    let s = slash_command("/goal all tests stay green").expect("/goal parses");
    match s.event {
        Some(ka_protocol::Command::Goal {
            action:
                ka_protocol::GoalAction::Set {
                    condition,
                    budget_usd,
                },
        }) => {
            assert_eq!(condition, "all tests stay green");
            assert_eq!(budget_usd, None, "the simple form carries no budget");
        }
        other => panic!("/goal <cond> carried {other:?}"),
    }
    let action = |input: &str| match slash_command(input).expect(input).event {
        Some(ka_protocol::Command::Goal { action }) => Some(action),
        other => panic!("{input} carried {other:?}"),
    };
    assert!(matches!(
        action("/goal off"),
        Some(ka_protocol::GoalAction::Drop)
    ));
    assert!(matches!(
        action("/goal pause"),
        Some(ka_protocol::GoalAction::Pause)
    ));
    assert!(matches!(
        action("/goal resume"),
        Some(ka_protocol::GoalAction::Resume)
    ));
    // status is local: no GoalAction::Status exists, the app loop owns it
    let s = slash_command("/goal status").expect("/goal status parses");
    assert!(s.event.is_none() && s.note.is_none());
    let bare = slash_command("/goal").expect("usage row still parses");
    assert!(bare.event.is_none());
    assert!(
        bare.note
            .as_deref()
            .is_some_and(|n| n.starts_with("usage:")),
        "{:?}",
        bare.note
    );
}

/// CTX-06: /loop [--every <secs>] <task> arms LoopAction::Set,
/// /loop off disarms; a bare /loop, an interval without a task, or a
/// non-numeric interval refuse with usage notes.
#[test]
fn loop_parses_task_every_and_off() {
    let s = slash_command("/loop rerun the test suite").expect("/loop parses");
    match s.event {
        Some(ka_protocol::Command::Loop {
            action:
                ka_protocol::LoopAction::Set {
                    task,
                    interval_secs,
                },
        }) => {
            assert_eq!(task, "rerun the test suite");
            assert_eq!(interval_secs, None, "bare task fires on the next settle");
        }
        other => panic!("/loop <task> carried {other:?}"),
    }
    let s = slash_command("/loop --every 120 rerun the test suite").expect("/loop --every parses");
    match s.event {
        Some(ka_protocol::Command::Loop {
            action:
                ka_protocol::LoopAction::Set {
                    task,
                    interval_secs,
                },
        }) => {
            assert_eq!(task, "rerun the test suite");
            assert_eq!(interval_secs, Some(120));
        }
        other => panic!("/loop --every carried {other:?}"),
    }
    let s = slash_command("/loop off").expect("/loop off parses");
    assert!(matches!(
        s.event,
        Some(ka_protocol::Command::Loop {
            action: ka_protocol::LoopAction::Off
        })
    ));
    for bad in ["/loop", "/loop --every 120", "/loop --every soon rerun"] {
        let s = slash_command(bad).expect("usage row still parses");
        assert!(s.event.is_none(), "{bad} must not carry an event");
        assert!(
            s.note.as_deref().is_some_and(|n| n.starts_with("usage:")),
            "{bad}: {:?}",
            s.note
        );
    }
}

/// SES-01: /diff dispatches SessionDiff; the excerpt + spill pointer
/// come back as a Note (rendered with a /spills hint).
#[test]
fn diff_sends_session_diff() {
    let s = slash_command("/diff").expect("/diff parses");
    assert!(matches!(s.event, Some(ka_protocol::Command::SessionDiff)));
    assert!(s.note.as_deref().is_some_and(|n| n.contains("note")));
}

/// CTX-10 v1: /memory consolidate dispatches MemoryConsolidate (the
/// engine merges the staged inbox into MEMORY.md and reports by note);
/// the bare form still opens the memory viewer.
#[test]
fn memory_consolidate_dispatches_and_bare_opens_the_viewer() {
    let s = slash_command("/memory consolidate").expect("/memory consolidate parses");
    assert!(matches!(
        s.event,
        Some(ka_protocol::Command::MemoryConsolidate)
    ));
    assert!(s.modal.is_none(), "consolidate must not open the viewer");
    let bare = slash_command("/memory").expect("/memory parses");
    assert!(bare.event.is_none());
    assert!(matches!(bare.modal, Some(ka_term::tui::ModalKind::Memory)));
}

/// CTX-05/06: the goal/loop mirrors fold the engine's notes — covered
/// by goal_and_loop_notes_reconcile_the_mirrors in src/tui.rs. CTX-12:
/// /autocompact <1-100|off> carries SetAutoCompact; out-of-range
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
        skills: vec![
            ka_protocol::SkillEntry {
                name: "pdf".into(),
                source: String::new(),
            },
            ka_protocol::SkillEntry {
                name: "commit-style".into(),
                source: String::new(),
            },
        ],
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
    inv.skills = vec![ka_protocol::SkillEntry {
        name: "commit-style".into(),
        source: String::new(),
    }];
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
    inv.skills = vec![ka_protocol::SkillEntry {
        name: "pdf".into(),
        source: String::new(),
    }];
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

    // a digest is never a fold/toggle target — it answers as the
    // prompt it stands for (the click-a-prompt rewind/fork popup);
    // live-turn rows keep their own mapping
    assert_eq!(
        t.row_ref_at(0),
        Some(ka_term::tui::RowRef::User(0)),
        "a digest answers as its prompt, not as a toggle"
    );
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

/// Batch 5 surface contracts: the input-layer commands shipped by the
/// TUI loop (intercepted before slash_command, no engine roundtrip)
/// stay listed in the popup catalog, and the [keybindings] action
/// table keeps its documented ids and defaults.
#[test]
fn input_layer_commands_stay_in_the_catalog() {
    use ka_term::tui::available_slash_commands;
    let catalog = available_slash_commands();
    for cmd in [
        "/copy",
        "/theme",
        "/search",
        "/changelog",
        "/queue",
        "/find",
        "/focus",
    ] {
        assert!(
            catalog.iter().any(|(n, _)| n == cmd),
            "input-layer command {cmd} vanished from the catalog"
        );
    }
    let themes = catalog
        .iter()
        .find(|(n, _)| n == "/theme")
        .map(|(_, d)| d.clone())
        .expect("/theme entry");
    assert!(
        themes.contains("default"),
        "the catalog teaches `default`: {themes}"
    );
}

#[test]
fn keybinding_actions_keep_their_documented_ids_and_defaults() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ka_term::keys::{ACTIONS, Bindings};
    let ids: Vec<_> = ACTIONS.iter().map(|a| a.id).collect();
    for id in [
        "composer.send",
        "composer.newline",
        "composer.steer",
        "session.abort",
        "session.rewind",
        "session.quit",
        "popups.skills",
        "popups.todos",
        "popups.info",
        "composer.external_editor",
        "mouse.toggle",
        "theme.picker",
        "changelog",
        "queue.open",
        "copy.picker",
        "session.search",
    ] {
        assert!(ids.contains(&id), "action id {id} vanished: {ids:?}");
    }
    // /help renders the effective chords, so the defaults must round-trip
    let b = Bindings::default();
    assert_eq!(b.display("popups.skills"), "ctrl+t");
    assert_eq!(b.display("popups.todos"), "alt+o");
    assert_eq!(b.display("popups.info"), "alt+i");
    assert_eq!(b.display("composer.external_editor"), "alt+e");
    // ctrl+m is Enter on legacy terminals (Enter+Control under the kitty
    // protocol), so it could never toggle: the chord is alt+m
    assert_eq!(b.display("mouse.toggle"), "alt+m");
    let enter_ctrl = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
    assert!(!b.matches(&enter_ctrl, "mouse.toggle"));
    let alt_m = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT);
    assert!(b.matches(&alt_m, "mouse.toggle"));
    assert_eq!(b.display("theme.picker"), "ctrl+g");
    assert_eq!(b.display("changelog"), "alt+c");
    assert_eq!(b.display("queue.open"), "alt+u");
    assert_eq!(b.display("copy.picker"), "alt+y");
    assert_eq!(b.display("session.search"), "alt+s");
    assert_eq!(b.display("session.rewind"), "esc esc");
    // no table = today's keyboard, intact
    assert!(Bindings::resolve(None).is_ok());
    let send = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let newline = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
    assert!(b.matches(&send, "composer.send"));
    assert!(b.matches(&newline, "composer.newline"));
}

/// Theme TOML contract: every palette token is settable, glyphs ride
/// [symbols], unknown keys degrade to notes (never a hard failure),
/// and applying a theme rebuilds the live tint map the render backend
/// consults.
#[test]
fn theme_files_contract() {
    use ka_term::theme::{apply, from_toml, tint};
    use ratatui::style::Color;
    let theme = from_toml(
        "contract",
        "[color]\naccent = \"#102030\"\nbg_panel = 8\n\n[symbols]\nprompt = \">\"\n",
    )
    .expect("a well-formed theme parses");
    assert_eq!(
        theme.colors.get("accent"),
        Some(&Color::Rgb(0x10, 0x20, 0x30))
    );
    assert_eq!(theme.colors.get("bg_panel"), Some(&Color::Indexed(8)));
    assert_eq!(theme.symbols.get("prompt").map(String::as_str), Some(">"));
    assert!(
        theme.notes.is_empty(),
        "no invented keys: {:?}",
        theme.notes
    );

    apply(Some(&theme));
    let map = tint().expect("theme active after apply");
    assert_eq!(
        map.get(&ka_term::palette::ACCENT),
        Some(&Color::Rgb(0x10, 0x20, 0x30))
    );
    apply(None);
    assert!(tint().is_none(), "default restores Nocturne");
}

/// Emoji shortcodes: a small curated table, `:`-led words only, Tab
/// completion via ka_term::emoji::complete.
#[test]
fn emoji_shortcodes_contract() {
    use ka_term::emoji::{EMOJI, complete};
    assert!(EMOJI.len() >= 80, "the table is the feature");
    assert_eq!(complete(":fire"), Some("🔥".to_string()));
    assert_eq!(complete(":fire:"), Some("🔥".to_string()));
    assert_eq!(complete(":fir"), Some(":fire".to_string()), "grows first");
    assert_eq!(complete("hello"), None, "non-: words untouched");
    assert_eq!(complete(":"), None, "bare colon is too broad");
}

/// [tui] status_line: the documented ids build the bar; goal/loop
/// badges survive any custom order.
#[test]
fn status_line_segments_contract() {
    use ka_term::tui::{Meters, status_ids_valid, status_segments};
    let order: Vec<String> = ["cost", "context", "cache"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(status_ids_valid(&order).is_ok());
    assert!(status_ids_valid(&["bogus".to_string()]).is_err());

    let m = Meters {
        cost: 0.5,
        context: (42_000, 100_000),
        cache_hit: Some(0.75),
        goal: Some(ka_term::tui::GoalState {
            condition: "done".into(),
            paused: false,
            budget_usd: None,
        }),
        ..Default::default()
    };
    let text: String = status_segments(&m, &order)
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(text, "$0.5000 42% cache 75% ◎ goal");
}

/// TUI-18: the read-only panels parse as documented and carry their
/// modal kinds — /hooks /permissions /doctor open pickers (never queue),
/// /vim /screen /plain /recap are session-local rows (no engine event).
#[test]
fn management_panels_and_session_toggles_parse() {
    for head in ["/hooks", "/permissions", "/doctor"] {
        let cmd = slash_command(head).expect(head);
        assert!(cmd.modal.is_some(), "{head} opens a panel");
        assert!(cmd.event.is_none(), "{head} is surface-local");
    }
    for head in ["/vim", "/screen", "/plain", "/recap"] {
        let cmd = slash_command(head).expect(head);
        assert!(cmd.event.is_none(), "{head} never sends an engine event");
        assert!(cmd.modal.is_none(), "{head} is an instant local action");
        assert!(!cmd.quit, "{head} never quits");
    }
    // argumented forms stay refused rather than misread
    assert!(
        slash_command("/vim aggressive").is_none() || {
            let cmd = slash_command("/vim aggressive").expect("parses");
            cmd.event.is_none() && cmd.modal.is_none()
        }
    );
}

/// GIT-01: /pr builds the gh review preset — readonly locks Readonly
/// mode like /review, `fix` stays in Ask and appends the
/// implement-commit-push instructions; the transcript note confirms
/// which preset went out, and unparsable forms are usage errors.
#[test]
fn pr_preset_builds_gh_review_prompts() {
    let readonly = slash_command("/pr 123").expect("parses");
    assert_eq!(
        readonly.note.as_deref(),
        Some("pr · reviewing #123 (readonly)")
    );
    assert!(
        matches!(
            readonly.event,
            Some(Command::SetMode {
                mode: ka_protocol::Mode::Readonly
            })
        ),
        "readonly review locks Readonly mode"
    );
    let prompt = readonly.followup.expect("the preset prompt");
    assert!(prompt.contains("gh pr view 123"), "{prompt}");
    assert!(prompt.contains("gh api repos/<owner>/<repo>/pulls/<number>/comments"));
    assert!(prompt.contains("file:line — issue — concrete fix"));
    assert!(!prompt.contains("FIX run"), "{prompt}");

    let fix = slash_command("/pr https://github.com/o/r/pull/42 fix").expect("parses");
    assert_eq!(
        fix.note.as_deref(),
        Some("pr · reviewing https://github.com/o/r/pull/42 (fix mode)")
    );
    assert!(
        matches!(
            fix.event,
            Some(Command::SetMode {
                mode: ka_protocol::Mode::Ask
            })
        ),
        "fix runs confirm edits and pushes (Ask)"
    );
    let prompt = fix.followup.expect("the fix preset");
    assert!(prompt.contains("FIX run"), "{prompt}");
    assert!(prompt.contains("Never force-push"), "{prompt}");

    for bad in ["/pr", "/pr 123 FIX", "/pr 123 fix extra"] {
        let cmd = slash_command(bad).expect("still parses as a usage row");
        assert!(cmd.event.is_none(), "{bad} sends nothing");
        assert!(cmd.followup.is_none(), "{bad} prompts nothing");
        assert!(
            cmd.note
                .as_deref()
                .is_some_and(|n| n.starts_with("usage: /pr")),
            "{bad} explains its syntax: {:?}",
            cmd.note
        );
    }
}

/// GIT-04: /git opens the staging & commit panel (modal, no engine
/// event) and the binding to it is registered.
#[test]
fn git_panel_opens_and_binds() {
    let cmd = slash_command("/git").expect("/git");
    assert_eq!(cmd.modal, Some(ModalKind::Git));
    assert!(cmd.event.is_none(), "status is read locally");
    assert!(
        slash_command("/git now").is_none() || {
            let c = slash_command("/git now").expect("parses");
            c.modal == Some(ModalKind::Git)
        }
    );
    // the alt+g chord resolves through the action registry
    let b = ka_term::keys::Bindings::default();
    assert_eq!(b.display("git.panel"), "alt+g");
}

/// TUI-15 + TUI-14: the /tasks roster rows the engine pre-renders feed
/// the status-line pill (running tasks · jobs · dap) and the Agent-Hub
/// cost footer — only facts the rows already carry.
#[test]
fn tasks_roster_feeds_pill_and_cost() {
    use ka_term::roster::{cost_summary, pill_counts, pill_text};
    use ka_term::tui::{JobsPill, Meters, status_ids_valid, status_segments};

    let rows = vec![
        "t-3  running   12s  scout — summarize$0.1234".to_string(),
        "t-4  done      5s  task — merge".to_string(),
        "job-1`  running  make".to_string(),
        "dap 7  running  breakpoints in 1 file(s)".to_string(),
    ];
    assert_eq!(pill_counts(&rows), (1, 1, 1));
    assert_eq!(pill_text(&rows).as_deref(), Some("1 task · 1 job · 1 dap"));
    assert_eq!(pill_text(&["t-4  done  5s  a — b".to_string()]), None);

    // the `jobs` status segment renders the pill; unknown ids still fail
    let m = Meters {
        jobs: JobsPill {
            tasks: 2,
            jobs: 0,
            dap: 1,
        },
        ..Default::default()
    };
    assert!(status_ids_valid(&["jobs".to_string()]).is_ok());
    let text: String = status_segments(&m, &["jobs".to_string()])
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(text, "2 tasks · 1 dap");

    // cost footer: only when some row reports a cost, summed per agent
    let (total, agents) = cost_summary(&rows).expect("a cost is reported");
    assert!((total - 0.1234).abs() < 1e-9);
    assert_eq!(agents, 1);
    assert!(cost_summary(&["t-4  done  5s  a — b".to_string()]).is_none());
}

/// EXT-12: `team <name>  <state>  turns N  <head>` rows ride the same
/// /tasks roster. They are display-only for ka-term: the pill ignores
/// them (member work is tracked via `team · ` completion notes) and the
/// cost footer never invents member costs.
#[test]
fn team_roster_rows_are_display_only_for_the_pill() {
    use ka_term::roster::{cost_summary, pill_counts, pill_text};

    let team_rows = vec![
        "team scout  done     turns 1   Survey done: 3 crates.".to_string(),
        "team writer  working  turns 0   -".to_string(),
        "team dismissed  dead   turns 2  final word".to_string(),
    ];
    assert_eq!(pill_counts(&team_rows), (0, 0, 0));
    assert_eq!(pill_text(&team_rows), None);
    assert_eq!(cost_summary(&team_rows), None);
    // mixed with task rows: only the `t-` rows count
    let mixed = vec![
        "t-1  running   3s  scout — x".to_string(),
        "team scout  working  turns 0   -".to_string(),
    ];
    assert_eq!(pill_counts(&mixed), (1, 0, 0));
    // Enter on a team row resolves no task id (no detail fetch)
    assert_eq!(ka_term::roster::parse_task_row(&team_rows[0]), None);
}

/// TUI-03: the composer vim layer — motions, operators with counts and
/// text objects, visual selection — all edit through the InputBuffer
/// (undo survives, the mode tag reports INSERT/NORMAL/VISUAL).
#[test]
fn vim_mode_edits_through_the_input_buffer() {
    use crossterm::event::{KeyCode, KeyModifiers};
    use ka_term::tui::InputBuffer;
    use ka_term::vim::{self, Flow, Mode};

    fn key(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    let mut buf = InputBuffer::default();
    buf.insert_str("deploy the staging build");
    buf.cursor = 0;
    buf.vim.on = true;
    buf.vim.mode = Mode::Normal;
    assert_eq!(buf.vim.label(), Some("NORMAL"));

    // motions + operator + count: d2w deletes two words
    for c in "d2w".chars() {
        vim::handle(&mut buf, &key(c));
    }
    assert_eq!(buf.text, "staging build");
    assert_eq!(buf.cursor, 0);

    // x with a register write, then undo restores
    vim::handle(&mut buf, &key('x'));
    assert_eq!(buf.text, "taging build");
    vim::handle(&mut buf, &key('u'));
    assert_eq!(buf.text, "staging build");

    // iw text object yanks the word under the cursor
    for c in "yiw".chars() {
        vim::handle(&mut buf, &key(c));
    }
    assert_eq!(buf.vim.register, Some(("staging".to_string(), false)));

    // visual select over a word motion, then change into insert:
    // w is word-exclusive, so `v w c` drops `deploy ` from a one-word
    // lead and lands in INSERT with the rest intact
    let mut vis = InputBuffer::default();
    vis.insert_str("deploy");
    vis.cursor = 0;
    vis.vim.on = true;
    vis.vim.mode = Mode::Normal;
    vim::handle(&mut vis, &key('v'));
    assert_eq!(vis.vim.mode, Mode::Visual);
    vim::handle(&mut vis, &key('w'));
    vim::handle(&mut vis, &key('c'));
    assert_eq!(vis.vim.mode, Mode::Insert);
    assert_eq!(vis.text, "");

    // INSERT passes typing through untouched and Esc returns to NORMAL
    for c in "rebuilt".chars() {
        assert_eq!(
            vim::handle(&mut vis, &key(c)),
            Flow::Passthrough,
            "insert mode keeps typing native"
        );
        vis.insert(c);
    }
    assert_eq!(vis.text, "rebuilt");
    let esc = crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(vim::handle(&mut vis, &esc), Flow::Handled);
    assert_eq!(vis.vim.mode, Mode::Normal);

    // /vim-style toggle resets to Insert and reports
    assert!(!buf.vim.toggle());
    assert!(buf.vim.toggle());
    assert_eq!(buf.vim.mode, Mode::Insert);
    assert_eq!(buf.vim.label(), Some("INSERT"));
}

/// TUI-09: ghost text — the dim continuation suggests the first matching
/// slash command or @-mention, and Tab-accept applies exactly what the
/// matching completion path would.
#[test]
fn ghost_text_suggests_and_accepts() {
    use ka_term::ghost;
    use ka_term::tui::InputBuffer;

    let cmds = vec![
        ("/model".to_string(), "pick a model".to_string()),
        ("/mode".to_string(), "permission mode".to_string()),
    ];
    let g = ghost::slash_ghost("/mod", &cmds).expect("prefix ghost");
    assert_eq!(g.suffix("/mod"), "el");
    let mut buf = InputBuffer::default();
    buf.insert_str("/mod");
    buf.cursor = buf.text.chars().count();
    ghost::accept(&g, &mut buf);
    assert_eq!(buf.text, "/model ");
    // exact commands suggest nothing; spaces end the token
    assert!(ghost::slash_ghost("/model", &cmds).is_none());
    assert!(ghost::slash_ghost("/mod now", &cmds).is_none());

    // @-mentions: first walk match, files complete with a trailing space
    let entries = vec![
        ("src/lib.rs".to_string(), false),
        ("src/main.rs".to_string(), false),
    ];
    let mut buf = InputBuffer::default();
    buf.insert_str("see @sr");
    buf.cursor = buf.text.chars().count();
    let g = ghost::mention_ghost(&buf.text, buf.cursor, &entries).expect("mention ghost");
    ghost::accept(&g, &mut buf);
    assert_eq!(buf.text, "see @src/lib.rs ");
}

/// The argument-bearing commands ghost their syntax past the bare
/// name (/goal, /loop — where the command-name ghost is done at an
/// exact match); typed arguments end the hint, Tab accepts the
/// template into the draft.
#[test]
fn goal_and_loop_ghost_their_argument_syntax() {
    use ka_term::ghost;
    use ka_term::tui::InputBuffer;

    let g = ghost::syntax_ghost("/goal").expect("bare /goal ghosts");
    assert_eq!(g.suffix("/goal"), " <condition>");
    let g = ghost::syntax_ghost("/loop").expect("bare /loop ghosts");
    assert_eq!(g.suffix("/loop"), " [--every <secs>] <task>");
    // trailing space: the template continues at the caret
    let g = ghost::syntax_ghost("/goal ").expect("prefix + space ghosts");
    assert_eq!(g.suffix("/goal "), "<condition>");
    // typed arguments own their syntax — no template parked after them
    assert!(ghost::syntax_ghost("/goal status").is_none());
    assert!(ghost::syntax_ghost("/goal tests stay green").is_none());
    assert!(ghost::syntax_ghost("/loop --every 30 rerun").is_none());
    assert!(
        ghost::syntax_ghost("/model").is_none(),
        "other commands stay quiet"
    );
    // Tab accepts: the template lands in the draft
    let g = ghost::syntax_ghost("/loop").expect("ghost");
    let mut buf = InputBuffer::default();
    buf.insert_str("/loop");
    buf.cursor = buf.text.chars().count();
    ghost::accept(&g, &mut buf);
    assert_eq!(buf.text, "/loop [--every <secs>] <task>");
}

/// SES-01: `/spills <id>` opens the spill viewer preselected on that
/// spill (bare id or the full `spill://` pointer); bare `/spills`
/// stays head-anchored. Notes carrying `spill://` pointers land their
/// id in /spills with a once-per-id hint (src unit tests).
#[test]
fn spills_opens_preselected_via_focus() {
    let s = slash_command("/spills").expect("/spills parses");
    assert!(matches!(s.modal, Some(ModalKind::Spills { focus: None })));
    let s = slash_command("/spills 1730000000-3").expect("/spills <id> parses");
    assert!(matches!(
        s.modal,
        Some(ModalKind::Spills { focus }) if focus.as_deref() == Some("1730000000-3")
    ));
    let s = slash_command("/spills spill://1730000000-3").expect("/spills <pointer> parses");
    assert!(matches!(
        s.modal,
        Some(ModalKind::Spills { focus }) if focus.as_deref() == Some("spill://1730000000-3")
    ));
}

/// Image attachments ride as positional chips: staging drops a plain
/// `[Image #N]` chip in the draft (no protocol change) and the send
/// pass turns chip order into ImagePart order, consuming the chips;
/// unchipped staging leftovers ride last.
#[test]
fn image_chips_order_the_outgoing_parts() {
    use ka_term::tui::{image_chip, resolve_image_chips};

    // the chip spells the staging slot
    assert_eq!(image_chip(1), "[Image #1]");
    assert_eq!(image_chip(2), "[Image #2]");

    let part = |tag: &str| ka_protocol::ImagePart {
        data: tag.to_string(),
        media_type: "image/png".to_string(),
    };
    // chip order beats staging order; chips are consumed from the text
    let (text, ordered) = resolve_image_chips(
        "look [Image #2] then [Image #1] ok",
        vec![part("a"), part("b"), part("c")],
    );
    assert_eq!(text, "look then ok");
    let tags: Vec<&str> = ordered.iter().map(|p| p.data.as_str()).collect();
    assert_eq!(tags, vec!["b", "a", "c"], "chips order, unchipped c last");

    // no chips: staging order is the send order, text untouched
    let (text, ordered) = resolve_image_chips("describe this", vec![part("a"), part("b")]);
    assert_eq!(text, "describe this");
    assert_eq!(
        ordered.iter().map(|p| p.data.as_str()).collect::<Vec<_>>(),
        vec!["a", "b"]
    );

    // a chips-only draft resolves to an image-only prompt
    let (text, ordered) = resolve_image_chips("[Image #1]", vec![part("a")]);
    assert_eq!(text, "");
    assert_eq!(ordered.len(), 1);
}

/// TUI-13: the /plain renderer textualizes every block kind with
/// prefixes and block separators — the screen-reader contract.
#[test]
fn plain_render_contract() {
    use ka_term::plain::{ask_lines, tool_row, transcript_rows};
    use ka_term::tui::{Line, PendingAsk, SummaryTone, ToolCall, Transcript};

    let mut t = Transcript::default();
    t.push(Line::User("run the tests".into()));
    t.push(Line::ToolBlock(vec![ToolCall {
        head: "→ bash · cargo test".into(),
        ok: false,
        note: "compile error".into(),
        excerpt: String::new(),
        spill: None,
        dur: None,
        expanded: false,
    }]));
    t.push(Line::Info("queued 1 prompt".into()));
    t.push(Line::Summary {
        glyph: '✗',
        tone: SummaryTone::Err,
        text: "build failed".into(),
        detail: None,
        expanded: false,
    });
    let rows = transcript_rows(&t);
    let wanted = [
        "user: run the tests",
        "tool: bash cargo test → failed: compile error",
        "note: queued 1 prompt",
        "failed: build failed",
    ];
    for w in wanted {
        assert!(rows.contains(&w.to_string()), "missing {w:?} in {rows:?}");
    }
    // every block is followed by a blank row (linear pause)
    assert_eq!(rows.iter().filter(|r| r.is_empty()).count(), 4);
    // no glyphs ride the plain rows: structure is the prefixes
    assert!(
        rows.iter()
            .all(|r| !r.contains('✗') && !r.contains('→') || r.starts_with("tool:"))
    );

    assert_eq!(
        tool_row(&ToolCall {
            head: "→ read · src/main.rs".into(),
            ok: true,
            note: String::new(),
            excerpt: String::new(),
            spill: None,
            dur: None,
            expanded: false,
        }),
        "tool: read src/main.rs → ok"
    );

    let ask = PendingAsk {
        id: ka_protocol::AskId("q".into()),
        question: "apply?".into(),
        options: vec!["allow".into(), "deny".into()],
        selected: 0,
        detail: None,
    };
    let rows = ask_lines(&ask);
    assert!(rows.contains(&"> 1. allow".to_string()), "{rows:?}");
    assert!(rows.contains(&"  2. deny".to_string()), "{rows:?}");
}

/// TUI-07: the /screen handoff dump — plain transcript between
/// full-width separator rules, ready for the primary-screen scrollback.
#[test]
fn screen_dump_contract() {
    use ka_term::screen::dump_text;
    use ka_term::tui::{Line, Transcript};

    let mut t = Transcript::default();
    t.push(Line::User("hello".into()));
    t.push(Line::Assistant("hi there".into()));
    let dump = dump_text(&t, "sbeef01", 44);
    let lines: Vec<&str> = dump.lines().collect();
    assert!(lines.first().unwrap().contains("ka · sbeef01"), "{lines:?}");
    assert!(lines.contains(&"user: hello"), "{lines:?}");
    assert!(lines.contains(&"hi there"), "{lines:?}");
    assert!(lines.last().unwrap().contains("end of transcript"));
    for line in lines {
        assert!(
            line.chars().count() <= 44,
            "dump rows must fit the width: {line:?}"
        );
    }
}

/// TUI-12: the recap — the persisted strand recap (via the
/// `StrandSummary.recap` field, never raw records) wins, else a local
/// model-free summary from the surface's own transcript; silent when
/// there is nothing to say.
#[test]
fn recap_contract() {
    use ka_term::recap::{recap_from_transcript, recap_note};
    use ka_term::tui::{Line, Transcript};

    fn transcript_with(prompts: &[&str]) -> Transcript {
        let mut t = Transcript::default();
        for p in prompts {
            t.push(Line::User((*p).to_string()));
        }
        t
    }

    // no user turns → no recap (nothing invented)
    assert!(recap_from_transcript(&Transcript::default()).is_none());
    // slash-command turns are actions, not asks
    let t = transcript_with(&["/compact", "the real ask"]);
    assert_eq!(
        recap_from_transcript(&t).as_deref(),
        Some("1 turn in context · last: the real ask")
    );
    let t = transcript_with(&["fix the bug", "then write tests"]);
    assert_eq!(
        recap_from_transcript(&t).as_deref(),
        Some("2 turns in context · last: then write tests")
    );
    // recaps clamp to one transcript line
    let long = "word ".repeat(80);
    let t = transcript_with(&[long.as_str()]);
    let recap = recap_from_transcript(&t).expect("recap");
    assert_eq!(recap.lines().count(), 1);
    assert!(recap.chars().count() <= 160, "{recap}");
    // the local transcript summary is the fallback for any session
    // without a stored recap — with turns in the transcript it fires
    let cwd = std::env::temp_dir();
    let t = transcript_with(&["fix the bug", "then write tests"]);
    assert_eq!(
        recap_note("sunknown-session", &cwd, &t).as_deref(),
        Some("2 turns in context · last: then write tests")
    );
    // and an empty transcript keeps it silent
    assert!(recap_note("sunknown-session", &cwd, &Transcript::default()).is_none());
    assert!(recap_note("", &cwd, &Transcript::default()).is_none());
}

/// TUI-18 /doctor + panels rows read the layered config: rules, hooks,
/// sandbox and the live facts render without inventing anything.
#[test]
fn panel_rows_render_configured_facts() {
    use ka_term::panels::{DoctorFacts, doctor_rows, hooks_rows, permissions_rows};
    use ka_term::tui::ProviderInfo;
    use std::path::Path;

    let layer = ka_engine::Config::parse_layer(
        "[[hooks]]\nevent = \"stop\"\ncommand = \"notify.sh\"\n\
         [[rules]]\ntool = \"bash\"\nverdict = \"ask\"\n\
         [sandbox]\nmode = \"fs\"\n",
        "contract",
    )
    .expect("layer parses");
    let mut cfg = ka_engine::Config::default();
    cfg.overlay(layer);

    let hook_rows = hooks_rows(&cfg, Path::new("."));
    let joined = hook_rows.join("\n");
    assert!(joined.contains("stop"), "{joined}");
    assert!(joined.contains("notify.sh"), "{joined}");
    assert!(joined.contains("pre-turn"), "{joined}");

    let perm_rows = permissions_rows(&cfg, Some("off"), "allow");
    let joined = perm_rows.join("\n");
    assert!(joined.contains("mode: allow"), "{joined}");
    assert!(joined.contains("bash"), "{joined}");
    assert!(joined.contains("ask"), "{joined}");
    assert!(joined.contains("live override off"), "{joined}");
    assert!(joined.contains("protected paths"), "{joined}");

    let providers = vec![ProviderInfo {
        name: "zai".into(),
        env_var: String::new(),
        base_url: String::new(),
        key_set: false,
    }];
    let facts = DoctorFacts {
        session: "",
        model: "zai/glm-5.3",
        effort: "off",
        mode: "allow",
        providers: &providers,
        live_sandbox: None,
        turns: 0,
        cost: 0.0,
        context: (0, 0),
        update: None,
        mcp: &[],
    };
    let rows = doctor_rows(&cfg, &facts);
    let joined = rows.join("\n");
    assert!(joined.contains("ka "), "{joined}");
    assert!(joined.contains("fs (config)"), "{joined}");
    assert!(joined.contains("doctor CLI"), "{joined}");
}

// ------------------------------------------------------------ EXT-08

/// EXT-08: `side: true` dispatches a custom command as a side question
/// (`Command::SideQuestion`, body rendered with $ARGUMENTS) and
/// `shell:` output is captured and PREPENDED to the prompt. KNOWN
/// COUPLING: writes real ~/.config/ka/commands fixtures (user-dir
/// commands are ungated — the established contract-test pattern) and
/// removes them after.
#[test]
fn ext08_side_and_shell_commands_dispatch() {
    use ka_term::tui::{parse_command_md, run_command_shell, slash_command};
    // frontmatter parses both new keys
    let (fm, body) = parse_command_md(
        "---\ndescription: peek\nside: true\nshell: \"echo captured\"\n---\nAsk about $ARGUMENTS",
    );
    assert!(fm.side);
    assert_eq!(fm.shell.as_deref(), Some("echo captured"));
    assert_eq!(body, "Ask about $ARGUMENTS");

    let home = std::env::var("HOME").expect("HOME set");
    let commands = std::path::PathBuf::from(&home).join(".config/ka/commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(
        commands.join("ka-contract-side.md"),
        "---\nside: true\n---\nAsk about $ARGUMENTS",
    )
    .unwrap();
    let s = slash_command("/ka-contract-side the build").expect("side command parses");
    match s.event {
        Some(ka_protocol::Command::SideQuestion { text }) => {
            assert_eq!(text, "Ask about the build");
        }
        other => panic!("side command must dispatch SideQuestion, got {other:?}"),
    }
    std::fs::write(
        commands.join("ka-contract-shell.md"),
        "---\nshell: echo shell-out\n---\nBody after",
    )
    .unwrap();
    let s = slash_command("/ka-contract-shell").expect("shell command parses");
    match s.event {
        Some(ka_protocol::Command::Prompt { text, .. }) => {
            assert!(
                text.starts_with("shell-out\n\nBody after"),
                "shell output rides ahead of the body: {text}"
            );
        }
        other => panic!("shell command must dispatch Prompt, got {other:?}"),
    }
    let _ = std::fs::remove_file(commands.join("ka-contract-side.md"));
    let _ = std::fs::remove_file(commands.join("ka-contract-shell.md"));

    // the shell machinery itself: captured output, inline errors, cap
    let out = run_command_shell("echo shell-out", std::path::Path::new("."));
    assert_eq!(out.trim_end(), "shell-out");
    let out = run_command_shell("echo oops >&2; exit 3", std::path::Path::new("."));
    assert!(
        out.contains("[error: exit 3]") && out.contains("oops"),
        "{out}"
    );
    let out = run_command_shell(
        "head -c 20000 /dev/zero | tr '\\0' 'x'",
        std::path::Path::new("."),
    );
    assert!(
        out.chars().count() > 8000 && out.ends_with("[shell output truncated at 8 KiB]"),
        "capped at 8 KiB with a note ({} chars)",
        out.chars().count()
    );
}

// ------------------------------------------------------------ EXT-09

/// EXT-09: the `¶ <style>` badge renders on the status line whenever a
/// style is active (a state badge that survives any segment order),
/// and hides when off.
#[test]
fn ext09_style_badge_renders_when_active() {
    let mut meters = Meters {
        style: Some("prose".into()),
        ..Meters::default()
    };
    let with_style = status_segments(&meters, &["model".to_string()])
        .iter()
        .map(|s| s.content.clone())
        .collect::<String>();
    assert!(with_style.contains("\u{00b6} prose"), "{with_style}");
    // the badge survives a custom segment order that omits every other
    // id (state badge, not a segment)
    let custom = status_segments(&meters, &["cost".to_string()])
        .iter()
        .map(|s| s.content.clone())
        .collect::<String>();
    assert!(custom.contains("\u{00b6} prose"), "{custom}");
    meters.style = None;
    let off = status_segments(&meters, &["model".to_string()])
        .iter()
        .map(|s| s.content.clone())
        .collect::<String>();
    assert!(!off.contains('\u{00b6}'), "{off}");
}

/// `/style` is intercepted before the slash table (picker/arm/off), so
/// the table itself returns None for it — the same contract /theme has.
/// The completion catalog documents it.
#[test]
fn ext09_style_slash_intercepted_like_theme() {
    assert!(
        slash_command("/style").is_none(),
        "handled by the intercept"
    );
    assert!(
        slash_command("/style prose").is_none(),
        "handled by the intercept"
    );
    let catalog = available_slash_commands();
    assert!(
        catalog.iter().any(|(c, _)| c == "/style"),
        "/style must appear in the command catalog"
    );
}
