//! /tasks roster parsing (TUI-15, TUI-14). The engine pre-renders the
//! `/tasks` rows (`Event::Tasks { rows }`); ka-term never invents task
//! data, it only reads what those rows already carry:
//!
//! * background delegate tasks: `t-{id}  {state}  {elapsed}  {agent} — {head}${cost}`
//!   (the `$cost` tail appears only once the runner reported usage);
//! * bash jobs: ``job-{id}`  {state}  {cmd}``;
//! * DAP sessions: `dap {id}  {status}  breakpoints in {n} file(s)`;
//! * team members (EXT-12, display-only for ka-term): `team {name}  {state}  turns {n}  {head}`
//!   — the pill ignores them (member work surfaces as `team · ` notes).
//!
//! Feeds the status-line job pill (running counts) and the /tasks
//! Agent-Hub extras (per-row cost + a summed footer).

/// One parsed `t-…` roster row: the facts ka-term can act on.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskRow {
    /// Task id (`t-<id>`).
    pub id: u64,
    /// Still running.
    pub running: bool,
    /// Owning subagent name, when the row carries one.
    pub agent: Option<String>,
    /// Reported cost tail (`$0.1234`), when the row carries one.
    pub cost: Option<f64>,
}

/// Parse a `t-…` row. Other row shapes (jobs, dap, placeholders) are
/// `None`.
pub fn parse_task_row(row: &str) -> Option<TaskRow> {
    let rest = row.trim().strip_prefix("t-")?;
    let id_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let id: u64 = rest[..id_end].parse().ok()?;
    let rest = rest[id_end..].trim_start();
    let mut parts = rest.splitn(3, "  ");
    let state = parts.next().unwrap_or_default().trim();
    let _elapsed = parts.next().unwrap_or_default().trim();
    let tail = parts.next().unwrap_or_default().trim();
    let running = state == "running";
    let (agent, cost) = match tail.split_once(" — ") {
        Some((agent, head)) => (Some(agent.trim()), trailing_cost(head)),
        None => (None, trailing_cost(tail)),
    };
    Some(TaskRow {
        id,
        running,
        agent: agent.filter(|a| !a.is_empty()).map(|a| a.to_string()),
        cost,
    })
}

/// The `$N.NN` cost tail of a task row's headline, when present (the
/// engine appends it only once the runner's usage arrived).
fn trailing_cost(head: &str) -> Option<f64> {
    let idx = head.rfind('$')?;
    let amount = &head[idx + 1..];
    if amount.is_empty() || !amount.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    amount.parse().ok()
}

/// Running counts across a roster snapshot: `(tasks, jobs, dap)`.
pub fn pill_counts(rows: &[String]) -> (usize, usize, usize) {
    let mut tasks = 0;
    let mut jobs = 0;
    let mut dap = 0;
    for row in rows {
        let row = row.trim_start();
        if row.starts_with("t-") {
            if let Some(t) = parse_task_row(row) {
                tasks += usize::from(t.running);
            }
        } else if row.starts_with("job-") {
            if row.contains(" running ") {
                jobs += 1;
            }
        } else if row.starts_with("dap ") {
            dap += 1;
        }
    }
    (tasks, jobs, dap)
}

/// The status-line pill text, `None` when nothing is active (the
/// segment disappears rather than reading `0 tasks`).
pub fn pill_text(rows: &[String]) -> Option<String> {
    pill_from_counts(pill_counts(rows))
}

/// [`pill_text`] over pre-counted `(tasks, jobs, dap)`.
pub fn pill_from_counts(counts: (usize, usize, usize)) -> Option<String> {
    let (tasks, jobs, dap) = counts;
    let mut parts: Vec<String> = Vec::new();
    if tasks > 0 {
        parts.push(format!("{tasks} task{}", plural(tasks)));
    }
    if jobs > 0 {
        parts.push(format!("{jobs} job{}", plural(jobs)));
    }
    if dap > 0 {
        parts.push(format!("{dap} dap"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Agent-Hub footer for the /tasks modal: total reported cost and the
/// number of agents that reported any — `None` while no row carries a
/// cost (nothing invented, nothing shown).
pub fn cost_summary(rows: &[String]) -> Option<(f64, usize)> {
    let mut total = 0.0;
    let mut agents: Vec<String> = Vec::new();
    for row in rows {
        if let Some(t) = parse_task_row(row) {
            if let Some(cost) = t.cost {
                total += cost;
                if let Some(agent) = &t.agent {
                    if !agents.contains(agent) {
                        agents.push(agent.clone());
                    }
                }
            }
        }
    }
    (total > 0.0).then_some((total, agents.len()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn task_row() -> String {
        "t-3  running   12s  scout — summarize the diff$0.1234".to_string()
    }

    #[test]
    fn task_rows_parse_state_agent_and_cost() {
        let t = parse_task_row(&task_row()).expect("parses");
        assert_eq!(t.id, 3);
        assert!(t.running);
        assert_eq!(t.agent.as_deref(), Some("scout"));
        assert_eq!(t.cost, Some(0.1234));
        let done = parse_task_row("t-9  done      1m  scout — merged").expect("parses");
        assert!(!done.running);
        assert_eq!(done.cost, None);
    }

    #[test]
    fn non_task_rows_do_not_parse() {
        assert!(parse_task_row("job-1`  running  make").is_none());
        assert!(parse_task_row("dap 1  running  breakpoints in 2 file(s)").is_none());
        assert!(parse_task_row("no background tasks or jobs").is_none());
    }

    #[test]
    fn pill_counts_running_only() {
        let rows = vec![
            task_row(),
            "t-4  done      5s  task — x".to_string(),
            "job-1`  running  make".to_string(),
            "job-2`  exit 0   make".to_string(),
            "dap 7  running  breakpoints in 1 file(s)".to_string(),
        ];
        assert_eq!(pill_counts(&rows), (1, 1, 1));
        assert_eq!(pill_text(&rows).as_deref(), Some("1 task · 1 job · 1 dap"));
    }

    #[test]
    fn pill_hides_when_nothing_is_active() {
        assert_eq!(pill_text(&["t-4  done  5s  a — b".to_string()]), None);
        assert_eq!(pill_text(&[]), None);
    }

    #[test]
    fn cost_summary_sums_reported_costs_per_agent() {
        let rows = vec![
            task_row(),
            "t-5  running   2s  task — other$0.0100".to_string(),
            "t-6  running   2s  task — free".to_string(),
        ];
        let (total, agents) = cost_summary(&rows).expect("costs reported");
        assert!((total - 0.1334).abs() < 1e-9);
        assert_eq!(agents, 2);
        assert!(cost_summary(&["t-6  running   2s  task — free".to_string()]).is_none());
    }
}
