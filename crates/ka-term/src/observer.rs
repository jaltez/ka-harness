//! The observe-only TUI behind `ka attach` (roadmap 9.6). Renders a
//! live `ka serve` session over SSE: title bar (session · title),
//! streaming transcript rows, and a footer with presence chips
//! (`busy · N attached`) — the ka chrome vocabulary, one file, no
//! input path to the engine. Prompting stays on the server's HTTP API;
//! this surface exists to watch.

use crossterm::event::{Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ka_protocol::Event;
use ratatui::layout::Constraint;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use tokio::sync::mpsc;

/// One rendered transcript row (already display-shaped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub text: String,
    pub dim: bool,
    pub accent: Option<Color>,
}

/// Rows retained before the oldest are drained (unbounded growth guard).
const MAX_ROWS: usize = 10_000;

/// The observer's whole state — kept pure so the event → row mapping
/// is testable without a terminal.
#[derive(Debug, Default)]
pub struct ObserverState {
    pub rows: Vec<Row>,
    /// Assistant text still being streamed (flushed into rows when the
    /// next tool call / turn boundary arrives).
    pending: String,
    pub busy: bool,
    pub attached: usize,
    pub session: String,
    pub title: String,
    pub model: String,
    /// False once the stream closed (footer shows it).
    pub ended: bool,
}

impl ObserverState {
    pub fn new(session: &str) -> Self {
        Self {
            session: session.to_string(),
            ..Default::default()
        }
    }

    fn flush_pending(&mut self) {
        if !self.pending.is_empty() {
            let text = std::mem::take(&mut self.pending);
            for line in text.lines() {
                self.push(cap_width(line), false, None);
            }
        }
    }

    fn push(&mut self, text: String, dim: bool, accent: Option<Color>) {
        self.rows.push(Row { text, dim, accent });
        if self.rows.len() > MAX_ROWS {
            let excess = self.rows.len() - MAX_ROWS;
            self.rows.drain(..excess);
        }
    }
}

/// Fold one engine event into observer state (pure; testable).
pub fn apply_event(state: &mut ObserverState, evt: &Event) {
    match evt {
        Event::Replay { messages } => {
            state.rows.clear();
            for m in messages {
                if m.digest {
                    state.push("⋯ digest ⋯".to_string(), true, None);
                    continue;
                }
                match m.role.as_str() {
                    "user" => state.push(
                        format!("❯ {}", first_line(&m.content)),
                        false,
                        Some(Color::Blue),
                    ),
                    "assistant" => {
                        for line in m.content.lines() {
                            state.push(cap_width(line), false, None);
                        }
                        for call in &m.calls {
                            state.push(
                                format!(
                                    "  {} {}",
                                    crate::icons::tool_icon(&call.tool),
                                    call.detail
                                ),
                                true,
                                if call.is_error {
                                    Some(Color::Red)
                                } else {
                                    None
                                },
                            );
                            if let Some(result) = &call.result {
                                state.push(format!("    {result}"), true, None);
                            }
                        }
                    }
                    other => {
                        state.push(format!("[{other}] {}", first_line(&m.content)), true, None)
                    }
                }
            }
        }
        Event::SessionInfo { id } => state.session = id.clone(),
        Event::Title { title } if !title.is_empty() => state.title = title.clone(),
        Event::ModelChanged { selector } => state.model = selector.clone(),
        Event::TurnStarted { .. } => state.busy = true,
        Event::Idle => state.busy = false,
        Event::Presence { busy, attached } => {
            state.busy = *busy;
            state.attached = *attached;
        }
        Event::Delta {
            kind: ka_protocol::DeltaKind::Text(t),
        } => state.pending.push_str(t),
        Event::CallStarted { tool, detail, .. } => {
            state.flush_pending();
            state.push(
                format!("{} {}", crate::icons::tool_icon(tool), detail),
                true,
                None,
            );
        }
        Event::CallOutput {
            excerpt, is_error, ..
        } => {
            let head = first_line(excerpt);
            state.push(format!("  {head}"), true, is_error.then_some(Color::Red));
        }
        Event::Ask { questions, .. } => {
            state.flush_pending();
            if let Some(q) = questions.first() {
                state.push(
                    format!("❓ {} (the writer decides)", q.text),
                    false,
                    Some(Color::Yellow),
                );
            }
        }
        Event::TurnFinished { stop, .. } => {
            state.flush_pending();
            let (mark, label) = match stop {
                ka_protocol::Stop::Done => ("✓", "done"),
                ka_protocol::Stop::Aborted => ("◐", "aborted"),
                ka_protocol::Stop::Error => ("✗", "error"),
                ka_protocol::Stop::Length => ("·", "stopped at output limit"),
            };
            state.push(format!("{mark} turn {label}"), true, None);
        }
        Event::Note { message } => state.push(format!("· {message}"), true, None),
        Event::Error { message, .. } => {
            state.flush_pending();
            state.push(format!("✗ {message}"), false, Some(Color::Red));
        }
        Event::DigestStarted => state.push("⋯ digesting context…".to_string(), true, None),
        Event::ShellOutput { command, .. } => state.push(format!("! {command}"), true, None),
        _ => {}
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(cap_width)
        .unwrap_or_default()
}

/// Trim one line and cap it at 160 chars (grapheme-ish: chars), marking
/// truncation with an ellipsis — every transcript row passes through this.
fn cap_width(line: &str) -> String {
    let trimmed = line.trim();
    let mut chars = trimmed.chars();
    let capped: String = chars.by_ref().take(160).collect();
    if chars.next().is_some() {
        format!("{capped}…")
    } else {
        capped
    }
}

/// The read-only run loop: consume engine events, redraw, quit on
/// q/Esc/Ctrl-C. PgUp/PgDn (and ↑/↓) scroll; any new event re-pins to
/// the bottom (an observer wants the live tail).
pub async fn run(events: mpsc::Receiver<Event>, session: &str, addr: &str) -> std::io::Result<()> {
    let mut terminal = setup();
    let result = event_loop(events, session, addr, &mut terminal).await;
    restore(terminal)?;
    result
}

/// Pure scroll math: `scrollback` is the top visible row (`None` = pinned
/// to the live tail), `rows` the transcript length, `height` the full
/// terminal height (content is `height - 2` chrome rows). Up steps one
/// content page back (clamped at the top); Down steps forward and
/// re-pins to the tail (`None`) once the bottom is reached — at the
/// tail already, Down is a no-op.
fn scroll(scrollback: Option<usize>, rows: usize, height: usize, up: bool) -> Option<usize> {
    let content = height.saturating_sub(2).max(1);
    let bottom = rows.saturating_sub(content);
    if up {
        let next = scrollback.unwrap_or(bottom).saturating_sub(content);
        Some(next.min(bottom))
    } else {
        let top = scrollback?;
        let next = top.saturating_add(content);
        (next < bottom).then_some(next)
    }
}

async fn event_loop(
    mut events: mpsc::Receiver<Event>,
    session: &str,
    addr: &str,
    terminal: &mut ratatui::DefaultTerminal,
) -> std::io::Result<()> {
    let mut state = ObserverState::new(session);
    let mut term_events = crossterm::event::EventStream::new();
    let mut scrollback: Option<usize> = None;
    let mut redraw = true;
    loop {
        if redraw {
            draw(terminal, &state, addr, scrollback)?;
            redraw = false;
        }
        tokio::select! {
            maybe = events.recv() => match maybe {
                Some(evt) => {
                    apply_event(&mut state, &evt);
                    scrollback = None; // live tail
                    redraw = true;
                }
                None => {
                    state.ended = true;
                    draw(terminal, &state, addr, scrollback)?;
                    break;
                }
            },
            key = term_events.next() => {
                let Some(Ok(key)) = key else { break };
                match key {
                    TermEvent::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::PageUp | KeyCode::Up | KeyCode::PageDown | KeyCode::Down => {
                            let height =
                                terminal.size().map(|s| s.height as usize).unwrap_or(20);
                            scrollback = scroll(
                                scrollback,
                                state.rows.len(),
                                height,
                                matches!(k.code, KeyCode::PageUp | KeyCode::Up),
                            );
                            redraw = true;
                        }
                        _ => {}
                    },
                    TermEvent::Resize(_, _) => redraw = true,
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn setup() -> ratatui::DefaultTerminal {
    // ratatui::init mirrors the main TUI: raw mode + alternate screen +
    // a panic hook that restores the terminal; ratatui::restore undoes it
    ratatui::init()
}

fn restore(_terminal: ratatui::DefaultTerminal) -> std::io::Result<()> {
    ratatui::restore();
    Ok(())
}

fn draw(
    terminal: &mut ratatui::DefaultTerminal,
    state: &ObserverState,
    addr: &str,
    scrollback: Option<usize>,
) -> std::io::Result<()> {
    terminal
        .draw(|frame| {
            let area = frame.area();
            let chunks = ratatui::layout::Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);

            // title: session · title · model
            let title_text = if state.title.is_empty() {
                format!(" ka attach · {} @ {}", state.session, addr)
            } else {
                format!(
                    " ka attach · {} · {} @ {}",
                    state.session, state.title, addr
                )
            };
            frame.render_widget(
                Paragraph::new(Line::styled(
                    title_text,
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                chunks[0],
            );

            // transcript: a scroll window over the rows; None = live tail
            let height = chunks[1].height as usize;
            let visible: Vec<Line> = if state.rows.len() <= height {
                state.rows.iter().map(styled_row).collect::<Vec<_>>()
            } else {
                let start = match scrollback {
                    Some(top) => top.min(state.rows.len() - height),
                    None => state.rows.len() - height,
                };
                state.rows[start..start + height]
                    .iter()
                    .map(styled_row)
                    .collect::<Vec<_>>()
            };
            frame.render_widget(
                Paragraph::new(visible).wrap(Wrap { trim: false }),
                chunks[1],
            );

            // footer: presence chips + hints
            let busy = if state.busy { "busy" } else { "idle" };
            let mut footer = format!(
                " ◉ {busy} · {} {} attached",
                crate::icons::AGENTS,
                state.attached
            );
            if !state.model.is_empty() {
                footer.push_str(&format!(" · {}", state.model));
            }
            if state.ended {
                footer.push_str(" · stream ended");
            }
            footer.push_str("  (q quit · PgUp/PgDn scroll · read-only)");
            frame.render_widget(
                Paragraph::new(Line::styled(footer, Style::default().fg(Color::DarkGray))),
                chunks[2],
            );
        })
        .map(|_| ())
}

fn styled_row(row: &Row) -> Line<'static> {
    let mut style = Style::default();
    if row.dim {
        style = style.fg(Color::DarkGray);
    }
    if let Some(color) = row.accent {
        style = style.fg(color);
    }
    Line::styled(row.text.clone(), style)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn replay(messages: Vec<ka_protocol::ReplayedMessage>) -> Event {
        Event::Replay { messages }
    }

    #[test]
    fn events_fold_into_rows_and_presence() {
        let mut state = ObserverState::new("s7");
        apply_event(
            &mut state,
            &replay(vec![
                ka_protocol::ReplayedMessage {
                    role: "user".into(),
                    content: "fix the parser".into(),
                    digest: false,
                    thinking: None,
                    calls: vec![],
                },
                ka_protocol::ReplayedMessage {
                    role: "assistant".into(),
                    content: "done".into(),
                    digest: false,
                    thinking: None,
                    calls: vec![ka_protocol::ReplayedCall {
                        id: "c1".into(),
                        tool: "bash".into(),
                        detail: "cargo test".into(),
                        result: Some("ok".into()),
                        is_error: false,
                    }],
                },
            ]),
        );
        assert!(state.rows.iter().any(|r| r.text.contains("fix the parser")));
        assert!(state.rows.iter().any(|r| r.text.contains("cargo test")));
        // live streaming flushes on turn boundaries
        apply_event(
            &mut state,
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("partial answer".into()),
            },
        );
        apply_event(
            &mut state,
            &Event::TurnFinished {
                stop: ka_protocol::Stop::Done,
                usage: Default::default(),
            },
        );
        assert!(state.rows.iter().any(|r| r.text.contains("partial answer")));
        // presence chips drive the footer state
        apply_event(
            &mut state,
            &Event::Presence {
                busy: true,
                attached: 2,
            },
        );
        assert!(state.busy && state.attached == 2);
        apply_event(&mut state, &Event::Idle);
        assert!(!state.busy);
        // asks are visible but explicitly not answerable here
        apply_event(
            &mut state,
            &Event::Ask {
                id: ka_protocol::AskId("ask-1".into()),
                questions: vec![ka_protocol::AskQuestion {
                    text: "run `rm -rf /tmp/x`?".into(),
                    options: vec!["allow".into()],
                    detail: None,
                }],
            },
        );
        assert!(
            state
                .rows
                .iter()
                .any(|r| r.text.contains("the writer decides"))
        );
    }

    #[test]
    fn down_at_live_tail_is_a_no_op() {
        // already pinned to the newest rows: Down/PageDown must not jump
        // to the top of the transcript
        assert_eq!(scroll(None, 500, 24, false), None);
        assert_eq!(scroll(None, 10, 24, false), None); // shorter than a page too
    }

    #[test]
    fn up_from_tail_steps_a_page_and_down_re_pins() {
        // content height = 24 - 2 chrome rows = 22; bottom top-row = 478
        let up = scroll(None, 500, 24, true);
        assert_eq!(up, Some(456));
        // paging forward from there lands exactly on the tail again
        assert_eq!(scroll(up, 500, 24, false), None);
        // a partial step down clamps at the bottom rather than past it
        assert_eq!(scroll(Some(470), 500, 24, false), None);
        // up clamps at the top of the transcript
        assert_eq!(scroll(Some(5), 500, 24, true), Some(0));
        assert_eq!(scroll(Some(0), 500, 24, true), Some(0));
    }

    #[test]
    fn rows_are_width_capped_and_ring_bounded() {
        let mut state = ObserverState::new("s");
        apply_event(
            &mut state,
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("x".repeat(400)),
            },
        );
        apply_event(
            &mut state,
            &Event::TurnFinished {
                stop: ka_protocol::Stop::Done,
                usage: Default::default(),
            },
        );
        let capped = state
            .rows
            .iter()
            .find(|r| r.text.starts_with('x'))
            .map(|r| r.text.clone())
            .unwrap_or_default();
        assert_eq!(capped.chars().count(), 161); // 160 + ellipsis
        // the ring drains the oldest rows once past the cap
        for i in 0..(MAX_ROWS + 5) {
            state.push(format!("r{i}"), true, None);
        }
        assert_eq!(state.rows.len(), MAX_ROWS);
        assert_eq!(state.rows[0].text, "r5");
    }
}
