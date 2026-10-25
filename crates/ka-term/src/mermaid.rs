//! Mermaid subset renderer for the transcript: ```` ```mermaid ````
//! fences draw as box-drawing diagrams instead of a code panel. Zero
//! dependencies (the footprint contract): a small hand parser covers
//! the diagrams assistants actually emit — flowcharts (`graph` /
//! `flowchart`, any direction), `sequenceDiagram` (participants,
//! messages, self calls, notes, loop/alt/opt rails),
//! `stateDiagram-v2` and `quadrantChart` (a plotted plane with
//! quadrant labels) — and anything else returns [`None`] so the
//! caller keeps the source panel.
//!
//! Everything paints onto one fixed-width character grid at the
//! caller's fold width: node labels truncate (rows never overflow), and
//! a diagram that cannot fit even with minimal labels returns [`None`].
//! Lines, borders and arrowheads ride [`RULE`] (decoration), node and
//! message labels [`FG`], edge labels and notes [`META`], `[*]`
//! start/end markers [`ACCENT`].
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

type Cell = (char, Style);

const RULE_STYLE: Style = Style::new().fg(crate::palette::RULE);
const LABEL_STYLE: Style = Style::new().fg(crate::palette::FG);
const BOLD_STYLE: Style = Style::new()
    .fg(crate::palette::FG)
    .add_modifier(Modifier::BOLD);
const META_STYLE: Style = Style::new().fg(crate::palette::META);
const ACCENT_STYLE: Style = Style::new().fg(crate::palette::ACCENT);

/// One character grid. Painting is bounds-checked and never panics;
/// `rows()` trims trailing blanks so diagrams sit on the canvas.
struct Grid {
    w: usize,
    h: usize,
    cells: Vec<Cell>,
}

impl Grid {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            cells: vec![(' ', Style::new()); w * h],
        }
    }

    fn put(&mut self, x: usize, y: usize, ch: char, style: Style) {
        if x < self.w && y < self.h {
            self.cells[y * self.w + x] = (ch, style);
        }
    }

    fn text(&mut self, x: usize, y: usize, s: &str, style: Style) {
        let mut cx = x;
        for c in s.chars() {
            self.put(cx, y, c, style);
            cx += c.width().unwrap_or(0);
        }
    }

    /// The grid as styled rows: same-style runs merge into spans,
    /// unstyled spaces stay transparent, fully blank rows carry no
    /// spans (true canvas blanks).
    fn rows(self) -> Vec<TuiLine<'static>> {
        let w = self.w;
        (0..self.h)
            .map(|y| {
                // interior unstyled cells must still occupy a column
                // (a span join that drops them glues the row's words
                // together); only trailing ones fall off
                let last = (0..w)
                    .rfind(|x| self.cells[y * w + x].1 != Style::new())
                    .map_or(0, |x| x + 1);
                let mut spans: Vec<Span<'static>> = Vec::new();
                let mut run = String::new();
                let mut run_style = Style::new();
                for x in 0..last {
                    let (ch, style) = self.cells[y * w + x];
                    let ch = if style == Style::new() { ' ' } else { ch };
                    if style != run_style && !run.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut run), run_style));
                    }
                    run_style = style;
                    run.push(ch);
                }
                if !run.is_empty() {
                    spans.push(Span::styled(run, run_style));
                }
                TuiLine::from(spans)
            })
            .collect()
    }

    /// Whether row `y` has no styled cell (a lifeline candidate).
    fn row_is_blank(&self, y: usize) -> bool {
        (0..self.w).all(|x| self.cells[y * self.w + x].1 == Style::new())
    }
}

/// Truncate to at most `max` display columns, marking a cut with `…`.
fn trunc_chars(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw + 1 > max {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}

// ── flowchart parsing ─────────────────────────────────────────────

/// Node shape, from the bracket the source used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `[label]`.
    Box,
    /// `(label)` / `([label])`.
    Round,
    /// `{label}` — decision.
    Diamond,
    /// `((label))` — circle (and `[*]`).
    Circle,
    /// `[[label]]`.
    Bold,
    /// bare id.
    Plain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
    id: String,
    label: String,
    shape: Shape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Edge {
    src: usize,
    dst: usize,
    label: Option<String>,
    dashed: bool,
    arrow: bool,
    /// `~~~` — constrains ranks, draws nothing.
    invisible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Flow {
    lr: bool,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

impl Flow {
    fn node(&mut self, id: &str, label: Option<String>, shape: Shape) -> usize {
        if let Some(i) = self.nodes.iter().position(|n| n.id == id) {
            if let Some(l) = label {
                self.nodes[i].label = l;
                self.nodes[i].shape = shape;
            }
            return i;
        }
        self.nodes.push(Node {
            id: id.to_string(),
            label: label.unwrap_or_else(|| id.to_string()),
            shape,
        });
        self.nodes.len() - 1
    }
}

/// The arrow between two node tokens, with its optional `|label|`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Arrow {
    text: String,
    label: Option<String>,
}

/// Arrows longest-first so `-->` wins over `--`. `~~~` links are
/// invisible: they order ranks but draw nothing.
const ARROWS: &[&str] = &["-.->", "==>", "-->", "---", "-.-", "~~~", "==", "->", "--"];

/// Split a flowchart line into node tokens (`Ok`) and arrows (`Err`).
fn scan_flow_line(line: &str) -> Vec<Result<String, Arrow>> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut token = String::new();
    let mut i = 0;
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        if let Some(a) = ARROWS.iter().find(|a| rest.starts_with(**a)) {
            if !token.trim().is_empty() {
                out.push(Ok(std::mem::take(&mut token)));
            } else {
                token.clear();
            }
            let mut arrow = Arrow {
                text: (*a).to_string(),
                label: None,
            };
            i += a.chars().count();
            // optional `|label|`
            let mut j = i;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            if j < chars.len() && chars[j] == '|' {
                let mut l = String::new();
                j += 1;
                while j < chars.len() && chars[j] != '|' {
                    l.push(chars[j]);
                    j += 1;
                }
                arrow.label = Some(clean_label(&l)).filter(|l| !l.is_empty());
                i = j + 1;
            }
            out.push(Err(arrow));
            continue;
        }
        token.push(chars[i]);
        i += 1;
    }
    if !token.trim().is_empty() {
        out.push(Ok(token));
    }
    out
}

/// Parse `id[shape]` / `id` into (id, label, shape).
fn parse_node_token(token: &str) -> (String, Option<String>, Shape) {
    let token = token.trim();
    if token == "[*]" {
        return ("[*]".to_string(), None, Shape::Circle);
    }
    let Some(open) = token.find(['[', '(', '{']) else {
        return (token.to_string(), None, Shape::Plain);
    };
    // a bracket at column 0 means the whole token is bracketed
    let (id, rest) = if open == 0 {
        (String::new(), token)
    } else {
        (token[..open].trim().to_string(), &token[open..])
    };
    let (shape, inner) =
        if let Some(i) = rest.strip_prefix("([[").and_then(|r| r.strip_suffix("]])")) {
            (Shape::Round, i)
        } else if let Some(i) = rest.strip_prefix("[[").and_then(|r| r.strip_suffix("]]")) {
            (Shape::Bold, i)
        } else if let Some(i) = rest.strip_prefix("((").and_then(|r| r.strip_suffix("))")) {
            (Shape::Circle, i)
        } else if let Some(i) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            (Shape::Box, i)
        } else if let Some(i) = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
            (Shape::Round, i)
        } else if let Some(i) = rest.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
            (Shape::Diamond, i)
        } else {
            return (id, None, Shape::Plain);
        };
    let label = clean_label(inner);
    (id, Some(label).filter(|l| !l.is_empty()), shape)
}

/// A label as one drawable line: `<br/>` variants join with a ` · `
/// separator, quotes drop, whitespace collapses.
fn clean_label(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s.trim();
    while let Some(i) = rest.find('<') {
        let tail = &rest[i..];
        let br = ["<br/>", "<br />", "<br>", "<BR/>", "<BR>"]
            .iter()
            .find(|b| tail.starts_with(**b));
        out.push_str(&rest[..i]);
        if let Some(b) = br {
            out.push_str(" · ");
            rest = &tail[b.len()..];
        } else {
            out.push('<');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out.trim()
        .trim_matches('"')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `50\t`, `50:`, `50.` prefix from an echoed numbered file (a digit
/// run must be followed by a hard separator — a plain space is not
/// enough, it would eat valid ids).
fn strip_line_number(line: &str) -> &str {
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    let sep = line[digits..].chars().next();
    if digits > 0 && matches!(sep, Some('\t' | ':' | '.')) {
        &line[digits + 1..]
    } else {
        line
    }
}

/// Rendering directives we silently ignore (not failures).
fn flow_ignore_line(line: &str) -> bool {
    matches!(
        line.split_whitespace().next().unwrap_or(""),
        "classDef" | "class" | "style" | "click" | "linkStyle" | "direction"
    )
}

/// Parse a flowchart/state body. `None` for unsupported constructs
/// (subgraphs) or a body with no nodes.
fn parse_flow(source: &str, lr: bool) -> Option<Flow> {
    let mut flow = Flow {
        lr,
        nodes: Vec::new(),
        edges: Vec::new(),
    };
    for raw in source.lines() {
        // models echoing numbered files emit `50\tsequenceDiagram`-style
        // prefixes; strip them before anything else looks at the line
        let line = strip_line_number(raw);
        let line = line.trim().trim_end_matches(';').trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if line == "end" {
            return None; // a subgraph (or block) we do not draw
        }
        if flow_ignore_line(line) {
            continue;
        }
        if line.starts_with("subgraph") {
            return None;
        }
        if let Some(rest) = line.strip_prefix("state ") {
            let (label, id) = rest.split_once(" as ")?;
            flow.node(
                id.trim(),
                Some(label.trim().trim_matches('"').to_string()),
                Shape::Round,
            );
            continue;
        }
        // `A --> B: description` (stateDiagram transition notes): the
        // colon tail labels the line's last edge
        let (line, tail) = match line.rsplit_once(':') {
            Some((left, tail)) if scan_flow_line(left).iter().any(|t| t.is_err()) => {
                let t = clean_label(tail);
                (left, (!t.is_empty()).then_some(t))
            }
            _ => (line, None),
        };
        let tokens = scan_flow_line(line);
        if tokens.is_empty() {
            continue;
        }
        let mut prev: Option<usize> = None;
        let mut pending: Option<Arrow> = None;
        for token in tokens {
            match token {
                Ok(tok) => {
                    let (id, label, shape) = parse_node_token(&tok);
                    let idx = if id == "[*]" {
                        // `[*]` marks start (edge source) and end (edge
                        // target); both draw as the accent dot
                        let key = if pending.is_some() {
                            "•end•"
                        } else {
                            "•start•"
                        };
                        flow.node(key, Some("•".into()), Shape::Circle)
                    } else {
                        flow.node(&id, label, shape)
                    };
                    if let (Some(src), Some(a)) = (prev.take(), pending.take()) {
                        // the colon tail wins only when the arrow did
                        // not carry its own |label|
                        let label = a.label.clone().or_else(|| tail.clone());
                        flow.edges.push(Edge {
                            src,
                            dst: idx,
                            label,
                            dashed: a.text.contains("-."),
                            arrow: a.text.contains('>'),
                            invisible: a.text.contains('~'),
                        });
                    }
                    prev = Some(idx);
                }
                Err(a) => pending = Some(a),
            }
        }
    }
    (!flow.nodes.is_empty()).then_some(flow)
}

// ── flowchart drawing ─────────────────────────────────────────────

/// Longest-path ranks. Relaxation is capped at the node count, so a
/// cycle grows ranks until the cap instead of looping forever.
fn ranks(flow: &Flow) -> Vec<Vec<usize>> {
    let n = flow.nodes.len();
    let mut rank = vec![0usize; n];
    for _ in 0..n {
        let mut changed = false;
        for e in &flow.edges {
            if rank[e.dst] < rank[e.src].saturating_add(1) {
                rank[e.dst] = rank[e.src] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let max = rank.iter().copied().max().unwrap_or(0);
    let mut by_rank: Vec<Vec<usize>> = vec![Vec::new(); max + 1];
    for (i, r) in rank.iter().enumerate() {
        by_rank[*r].push(i);
    }
    // cycle relaxation can open gaps; empty ranks are pure air
    by_rank.retain(|r| !r.is_empty());
    by_rank
}

/// A node's one-row chip (`[ label ]`, `( label )`, `< label >`, or a
/// bold bare label) and its center offset inside the chip.
fn chip(node: &Node, budget: usize) -> (String, usize) {
    let chrome = match node.shape {
        Shape::Plain | Shape::Bold => 2,
        _ => 4,
    };
    let label = trunc_chars(&node.label, budget.saturating_sub(chrome).max(1));
    match node.shape {
        Shape::Plain | Shape::Bold => (format!(" {label} "), label.width() / 2),
        Shape::Box => (format!("[ {label} ]"), 2 + label.width() / 2),
        Shape::Round | Shape::Circle => (format!("( {label} )"), 2 + label.width() / 2),
        Shape::Diamond => (format!("< {label} >"), 2 + label.width() / 2),
    }
}

fn chip_style(node: &Node) -> Style {
    match node.shape {
        Shape::Bold => BOLD_STYLE,
        _ if node.label == "•" => ACCENT_STYLE,
        _ => LABEL_STYLE,
    }
}

/// Rows a TD rank row needs: chips plus two columns between them.
fn rank_row_width(rank: &[usize], flow: &Flow, budget: usize) -> usize {
    rank.iter()
        .map(|&i| chip(&flow.nodes[i], budget).0.width())
        .sum::<usize>()
        .saturating_add(2usize.saturating_mul(rank.len().saturating_sub(1)))
}

/// Paint a turn, merging complementary corners that land on one cell
/// (a fan-out writes `└` and `┘` at the same column — that is a `┴`).
fn put_turn(grid: &mut Grid, x: usize, y: usize, ch: char) {
    let merge = |a: char, b: char| -> Option<char> {
        match (a, b) {
            ('└', '┘') | ('┘', '└') => Some('┴'),
            ('┌', '┐') | ('┐', '┌') => Some('┬'),
            _ => None,
        }
    };
    let existing = if x < grid.w && y < grid.h {
        Some(grid.cells[y * grid.w + x].0)
    } else {
        None
    };
    if let Some(e) = existing
        && let Some(m) = merge(e, ch)
    {
        grid.put(x, y, m, RULE_STYLE);
    } else {
        grid.put(x, y, ch, RULE_STYLE);
    }
}

/// Connector rows between TD ranks.
const GAP_ROWS: usize = 2;

fn draw_flow(flow: &Flow, width: usize) -> Option<Vec<TuiLine<'static>>> {
    let by_rank = ranks(flow);
    // shrink label budgets until the diagram fits: TD needs its
    // widest rank row to fit, LR its whole column strip. An LR strip
    // too long for the pane (a pipeline of many ranks) falls through
    let fits_lr = |budget: usize| lr_layout(flow, &by_rank, budget).2 <= width;
    let fits_td = |budget: usize| {
        by_rank
            .iter()
            .all(|r| rank_row_width(r, flow, budget) <= width)
    };
    let shrink = |fits: &dyn Fn(usize) -> bool| {
        let mut budget = width;
        while !fits(budget) && budget > 8 {
            budget -= 2;
        }
        fits(budget).then_some(budget)
    };
    // LR keeps its direction when it fits readably; a strip that only
    // fits with starved labels (or not at all) draws vertically —
    // whichever layout keeps the bigger label budget wins
    let b_lr = if flow.lr { shrink(&fits_lr) } else { None };
    let b_td = shrink(&fits_td);
    match (b_lr, b_td) {
        (Some(bl), Some(bt)) if bl >= bt => draw_flow_lr(flow, &by_rank, bl, width),
        (_, Some(bt)) => draw_flow_td(flow, &by_rank, bt, width),
        (Some(bl), None) => draw_flow_lr(flow, &by_rank, bl, width),
        (None, None) => None,
    }
}

/// Vertical layout: one rank per row, `GAP_ROWS` connector rows under
/// it carrying turns, labels, and arrowheads.
fn draw_flow_td(
    flow: &Flow,
    by_rank: &[Vec<usize>],
    budget: usize,
    width: usize,
) -> Option<Vec<TuiLine<'static>>> {
    let stride = 1 + GAP_ROWS;
    let height = by_rank.len() + GAP_ROWS * by_rank.len().saturating_sub(1);
    let mut grid = Grid::new(width, height.max(1));
    // centers[rank] = (center_x, node) for routing
    let mut centers: Vec<Vec<(usize, usize)>> = Vec::with_capacity(by_rank.len());
    for (r, rank) in by_rank.iter().enumerate() {
        let y = r * stride;
        let mut x = 0;
        let mut row = Vec::with_capacity(rank.len());
        for &i in rank {
            let (text, center) = chip(&flow.nodes[i], budget);
            grid.text(x, y, &text, chip_style(&flow.nodes[i]));
            row.push((x + center, i));
            x += text.width() + 2;
        }
        centers.push(row);
    }
    let find = |node: usize| -> Option<(usize, usize)> {
        centers
            .iter()
            .enumerate()
            .flat_map(|(r, cs)| cs.iter().map(move |c| (r, *c)))
            .find(|(_, c)| c.1 == node)
            .map(|(r, c)| (r, c.0))
    };
    for e in &flow.edges {
        let (sr, sx) = find(e.src)?;
        let (dr, dx) = find(e.dst)?;
        if dr <= sr {
            continue; // back edge: dropped rather than mislaid (v1)
        }
        let row_a = sr * stride + 1;
        let row_b = row_a + 1;
        let line = if e.dashed { '╌' } else { '─' };
        if sx == dx {
            grid.put(sx, row_a, '│', RULE_STYLE);
            if let Some(l) = &e.label {
                let label = format!(" {l} ");
                let x = (dx + 1).min(width.saturating_sub(label.width()));
                grid.text(x, row_b, &label, META_STYLE);
            }
        } else {
            put_turn(&mut grid, sx, row_a, if sx < dx { '└' } else { '┘' });
            let (x0, x1) = if sx < dx { (sx, dx) } else { (dx, sx) };
            for x in x0 + 1..x1 {
                grid.put(x, row_a, line, RULE_STYLE);
            }
            put_turn(&mut grid, dx, row_a, if sx < dx { '┐' } else { '┌' });
            if let Some(l) = &e.label {
                let label = format!(" {l} ");
                if x1 - x0 >= label.width() + 2 {
                    let mid = x0 + (x1 - x0 - label.width()) / 2;
                    grid.text(mid, row_a, &label, META_STYLE);
                } else {
                    // a short run keeps its line: the label rides right
                    // of the arrowhead on the row below
                    let x = (dx + 1).min(width.saturating_sub(label.width()));
                    grid.text(x, row_b, &label, META_STYLE);
                }
            }
        }
        if e.arrow {
            grid.put(dx, row_b, '▼', RULE_STYLE);
        } else {
            grid.put(dx, row_b, '│', RULE_STYLE);
        }
    }
    Some(grid.rows())
}

/// The LR column layout at a label budget: per-rank column widths and
/// per-gap connector widths (wide enough for the widest edge label
/// riding that gap), with the strip's total width.
fn lr_layout(
    flow: &Flow,
    by_rank: &[Vec<usize>],
    budget: usize,
) -> (Vec<usize>, Vec<usize>, usize) {
    let mut rank_of = vec![0usize; flow.nodes.len()];
    for (ri, rank) in by_rank.iter().enumerate() {
        for &i in rank {
            rank_of[i] = ri;
        }
    }
    // a gap fits its widest edge label, capped: long labels truncate
    // into the gap instead of starving the columns
    let gap_w: Vec<usize> = (0..by_rank.len().saturating_sub(1))
        .map(|ri| {
            flow.edges
                .iter()
                .filter(|e| rank_of[e.src] == ri && rank_of[e.dst] > ri && !e.invisible)
                .filter_map(|e| e.label.as_ref().map(|l| (l.width() + 2).min(14)))
                .max()
                .unwrap_or(3)
                .max(3)
        })
        .collect();
    let col_w: Vec<usize> = by_rank
        .iter()
        .map(|rank| {
            rank.iter()
                .map(|&i| chip(&flow.nodes[i], budget).0.width())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let total = col_w.iter().sum::<usize>() + gap_w.iter().sum::<usize>();
    (gap_w, col_w, total)
}

/// Horizontal layout: ranks as columns, nodes stacked inside a column,
/// edges routed through the gap between columns.
fn draw_flow_lr(
    flow: &Flow,
    by_rank: &[Vec<usize>],
    budget: usize,
    width: usize,
) -> Option<Vec<TuiLine<'static>>> {
    let (gap_w, col_w, total) = lr_layout(flow, by_rank, budget);
    if total > width {
        return None;
    }
    let mut rank_of = vec![0usize; flow.nodes.len()];
    for (ri, rank) in by_rank.iter().enumerate() {
        for &i in rank {
            rank_of[i] = ri;
        }
    }
    let mut col_x = Vec::with_capacity(by_rank.len());
    let mut x = 0;
    for (ri, w) in col_w.iter().enumerate() {
        col_x.push(x);
        x += w;
        if ri + 1 < col_w.len() {
            x += gap_w[ri];
        }
    }
    let height = by_rank.iter().map(|r| r.len() * 2 - 1).max().unwrap_or(1);
    let mut grid = Grid::new(width, height.max(1));
    // node_pos[i] = (left, y, right)
    let mut node_pos = vec![(0usize, 0usize, 0usize); flow.nodes.len()];
    for (ri, rank) in by_rank.iter().enumerate() {
        for (row, &i) in rank.iter().enumerate() {
            let (text, _) = chip(&flow.nodes[i], budget);
            let left = col_x[ri] + col_w[ri].saturating_sub(text.width()) / 2;
            let y = row * 2;
            grid.text(left, y, &text, chip_style(&flow.nodes[i]));
            node_pos[i] = (left, y, left + text.width());
        }
    }
    // a label refuses to land on another label or a chip: a cell is
    // free when blank or a plain rule line
    let put_label = |grid: &mut Grid, x: usize, y: usize, s: &str| {
        let w = s.width();
        let free = (x..x + w).all(|cx| {
            cx < grid.w
                && matches!(grid.cells[y * grid.w + cx].1, st if st == Style::new() || st == RULE_STYLE)
        });
        if free {
            grid.text(x, y, s, META_STYLE);
        }
    };
    for e in &flow.edges {
        if e.invisible {
            continue;
        }
        let (_sx, sy, sright) = node_pos[e.src];
        let (dx, dy, _) = node_pos[e.dst];
        let (rs, rd) = (rank_of[e.src], rank_of[e.dst]);
        if rd <= rs {
            continue;
        }
        let line = if e.dashed { '╌' } else { '─' };
        if sy == dy {
            // straight shot
            for x in sright..dx {
                grid.put(x, sy, line, RULE_STYLE);
            }
            if e.arrow {
                grid.put(dx.saturating_sub(1), sy, '▶', RULE_STYLE);
            }
            if let Some(l) = &e.label {
                let room = dx.saturating_sub(sright + 3);
                let l = trunc_chars(l, room);
                let lw = l.width() + 2;
                if dx - sright > lw {
                    let mid = sright + (dx - sright - lw) / 2;
                    put_label(&mut grid, mid, sy, &format!(" {l} "));
                }
            }
            continue;
        }
        // bent: out of the source, through the gap's center column,
        // into the target
        let gxc = col_x[rs] + col_w[rs] + gap_w[rs] / 2;
        for x in sright..gxc {
            grid.put(x, sy, line, RULE_STYLE);
        }
        grid.put(gxc, sy, if dy > sy { '┐' } else { '┘' }, RULE_STYLE);
        let (y0, y1) = if dy > sy { (sy + 1, dy) } else { (dy + 1, sy) };
        for y in y0..y1 {
            grid.put(gxc, y, '│', RULE_STYLE);
        }
        grid.put(gxc, dy, if dy > sy { '└' } else { '┌' }, RULE_STYLE);
        for x in gxc + 1..dx {
            grid.put(x, dy, line, RULE_STYLE);
        }
        if e.arrow {
            grid.put(dx.saturating_sub(1), dy, '▶', RULE_STYLE);
        }
        if let Some(l) = &e.label {
            let l = trunc_chars(l, dx.saturating_sub(gxc + 3));
            put_label(&mut grid, gxc + 1, dy, &format!(" {l} "));
        }
    }
    Some(grid.rows())
}

// ── sequence diagram ──────────────────────────────────────────────

#[derive(Debug, Clone)]
enum SeqEvent {
    Msg {
        from: usize,
        to: usize,
        text: String,
        dashed: bool,
    },
    Note {
        text: String,
    },
    BlockOpen(String),
    BlockElse(String),
    BlockClose,
}

#[derive(Debug, Default)]
struct Sequence {
    participants: Vec<(String, String)>,
    events: Vec<SeqEvent>,
}

impl Sequence {
    fn participant(&mut self, id: &str, name: Option<String>) -> usize {
        if let Some(i) = self.participants.iter().position(|(p, _)| p == id) {
            return i;
        }
        self.participants
            .push((id.to_string(), name.unwrap_or_else(|| id.to_string())));
        self.participants.len() - 1
    }
}

fn parse_sequence(source: &str) -> Option<Sequence> {
    let mut seq = Sequence::default();
    for raw in source.lines() {
        let line = strip_line_number(raw);
        let line = line.trim().trim_end_matches(';').trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        let word = line.split_whitespace().next().unwrap_or("");
        match word {
            "autonumber" | "activate" | "deactivate" => continue,
            "participant" | "actor" => {
                let rest = line
                    .strip_prefix("participant ")
                    .or_else(|| line.strip_prefix("actor "))?
                    .trim();
                let (id, name) = match rest.split_once(" as ") {
                    Some((id, name)) => (id.trim(), Some(name.trim().to_string())),
                    None => (rest, None),
                };
                if id.is_empty() {
                    return None;
                }
                seq.participant(id, name);
            }
            "Note" | "note" => {
                let (_, text) = line.split_once(':')?;
                seq.events.push(SeqEvent::Note {
                    text: text.trim().to_string(),
                });
            }
            "loop" | "opt" | "par" | "critical" | "alt" | "break" => {
                let text = line.strip_prefix(word).unwrap_or("").trim();
                let text = text.strip_prefix(':').unwrap_or(text).trim();
                seq.events.push(SeqEvent::BlockOpen(if text.is_empty() {
                    word.to_string()
                } else {
                    format!("{word}: {text}")
                }));
            }
            "else" | "and" | "option" => {
                let text = line.split_once(':').map_or("", |(_, t)| t.trim());
                seq.events.push(SeqEvent::BlockElse(text.to_string()));
            }
            "end" => seq.events.push(SeqEvent::BlockClose),
            _ => {
                let (left, text) = line.split_once(':')?;
                let left = left.trim();
                let (from, to, dashed) = if let Some((a, b)) = left.split_once("-->>") {
                    (a, b, true)
                } else if let Some((a, b)) = left.split_once("->>") {
                    (a, b, false)
                } else if let Some((a, b)) = left.split_once("-->") {
                    (a, b, true)
                } else if let Some((a, b)) = left.split_once("->") {
                    (a, b, false)
                } else {
                    return None;
                };
                let from = seq.participant(from.trim(), None);
                let to = seq.participant(to.trim(), None);
                seq.events.push(SeqEvent::Msg {
                    from,
                    to,
                    text: text.trim().to_string(),
                    dashed,
                });
            }
        }
    }
    (seq.participants.len() >= 2 && !seq.events.is_empty()).then_some(seq)
}

fn draw_sequence(source: &str, width: usize) -> Option<Vec<TuiLine<'static>>> {
    let seq = parse_sequence(source)?;
    // participant columns: truncate names until everyone fits with a
    // three-column gap between lifelines
    let mut name_budget = width;
    let mut centers = loop {
        let mut centers = Vec::with_capacity(seq.participants.len());
        let mut x = 0;
        for (_, name) in &seq.participants {
            let w = trunc_chars(name, name_budget).width();
            centers.push(x + w / 2);
            x += w + 3;
        }
        if x <= width || name_budget <= 2 {
            break centers;
        }
        name_budget = name_budget.saturating_sub(2).max(2);
    };
    let last = *centers.last()?;
    if last >= width {
        return None;
    }
    // spread the lifelines across the pane: tight gaps truncate
    // message labels, and the slack is otherwise dead air — but the
    // last chip must stay inside the pane
    if centers.len() > 1 {
        let slack = width.saturating_sub(last + 1);
        let half = seq
            .participants
            .iter()
            .map(|(_, n)| trunc_chars(n, name_budget).width() / 2)
            .max()
            .unwrap_or(0);
        let room = width.saturating_sub(last + 2 + half);
        let extra = slack.min(room) / (centers.len() - 1);
        let mut shift = 0;
        for (i, c) in centers.iter_mut().enumerate() {
            if i > 0 {
                shift += extra;
            }
            *c += shift;
        }
    }
    let mut height = 2;
    for ev in &seq.events {
        height += match ev {
            SeqEvent::Msg { from, to, .. } if from == to => 3,
            _ => 2,
        };
    }
    let mut grid = Grid::new(width, height);
    for (i, (_, name)) in seq.participants.iter().enumerate() {
        let chip = trunc_chars(name, name_budget);
        let start = centers[i].saturating_sub(chip.width() / 2);
        grid.text(start, 0, &chip, BOLD_STYLE);
    }
    let mut y = 2;
    for ev in &seq.events {
        match ev {
            SeqEvent::Msg {
                from,
                to,
                text,
                dashed,
            } => {
                let (a, b) = (centers[*from], centers[*to]);
                let line = if *dashed { '╌' } else { '─' };
                if from == to {
                    // self call: a two-row loop to the right of the
                    // lifeline, closing back into it
                    let span = (text.width() + 6).min(width.saturating_sub(a + 2)).max(4);
                    for x in a + 1..a + span {
                        grid.put(x, y, line, RULE_STYLE);
                        grid.put(x, y + 1, line, RULE_STYLE);
                    }
                    grid.put(a, y, '┌', RULE_STYLE);
                    grid.put(a + span, y, '┐', RULE_STYLE);
                    grid.put(a, y + 1, '◀', RULE_STYLE);
                    grid.put(a + span, y + 1, '┘', RULE_STYLE);
                    let label = format!(" {text} ");
                    let mid = a + 1 + span.saturating_sub(label.width() + 2) / 2;
                    grid.text(mid, y, &trunc_chars(&label, span - 1), LABEL_STYLE);
                    y += 3;
                } else {
                    let (x0, x1) = if a < b { (a, b) } else { (b, a) };
                    for x in x0..=x1 {
                        grid.put(x, y, line, RULE_STYLE);
                    }
                    let head_x = if a < b { x1 } else { x0 };
                    grid.put(head_x, y, if a < b { '▶' } else { '◀' }, RULE_STYLE);
                    // the label rides strictly inside the run so it can
                    // never cover the arrowhead; padding goes first when
                    // the run is tight
                    let interior = x1.saturating_sub(x0 + 1);
                    let label = if format!(" {text} ").width() <= interior {
                        format!(" {text} ")
                    } else {
                        text.clone()
                    };
                    let mid = x0 + 1 + interior.saturating_sub(label.width()) / 2;
                    grid.text(mid, y, &trunc_chars(&label, interior), LABEL_STYLE);
                    y += 2;
                }
            }
            SeqEvent::Note { text } => {
                let note = format!("┄ {text} ┄");
                // centered over the participant span, not the pane
                let span = centers.last().copied().unwrap_or(0);
                let start =
                    (span.saturating_sub(note.width()) / 2).min(width.saturating_sub(note.width()));
                grid.text(start, y, &trunc_chars(&note, width), META_STYLE);
                y += 2;
            }
            SeqEvent::BlockOpen(label) => {
                grid.text(0, y, &format!("╭─ {label}"), META_STYLE);
                y += 1;
            }
            SeqEvent::BlockElse(label) => {
                let text = if label.is_empty() {
                    "├─ else".to_string()
                } else {
                    format!("├─ else {label}")
                };
                grid.text(0, y, &text, META_STYLE);
                y += 1;
            }
            SeqEvent::BlockClose => {
                grid.text(0, y, "╰─", META_STYLE);
                y += 1;
            }
        }
    }
    // lifelines fill the air rows between events, never past the last
    // one; trailing air rows drop so the diagram ends on content
    for ry in 1..y.max(2) {
        if grid.row_is_blank(ry) {
            for c in &centers {
                grid.put(*c, ry, '│', RULE_STYLE);
            }
        }
    }
    let height = y.max(2).min(grid.h);
    grid.h = height;
    Some(grid.rows())
}

// ── quadrantChart ─────────────────────────────────────────────────

struct Quadrant {
    title: Option<String>,
    x: (String, String),
    y: (String, String),
    /// 1 top-right, 2 top-left, 3 bottom-left, 4 bottom-right
    q: [String; 4],
    points: Vec<(String, f64, f64)>,
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').to_string()
}

fn parse_quadrant(source: &str) -> Option<Quadrant> {
    let mut q = Quadrant {
        title: None,
        x: (String::new(), String::new()),
        y: (String::new(), String::new()),
        q: Default::default(),
        points: Vec::new(),
    };
    for raw in source.lines() {
        let line = strip_line_number(raw);
        let line = line.trim().trim_end_matches(';').trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if let Some(t) = line.strip_prefix("title ") {
            q.title = Some(unquote(t));
        } else if let Some(rest) = line.strip_prefix("x-axis") {
            let (a, b) = rest.split_once("-->")?;
            q.x = (unquote(a), unquote(b));
        } else if let Some(rest) = line.strip_prefix("y-axis") {
            let (a, b) = rest.split_once("-->")?;
            q.y = (unquote(a), unquote(b));
        } else if let Some(rest) = line.strip_prefix("quadrant-") {
            if let Some((n, label)) = rest.split_once([' ', ':']) {
                if let Ok(i) = n.trim().parse::<usize>() {
                    if (1..=4).contains(&i) {
                        q.q[i - 1] = unquote(label);
                    }
                }
            }
        } else if let Some((name, tail)) = line.split_once(':') {
            let inner = tail.trim().strip_prefix('[')?.strip_suffix(']')?;
            let (xs, ys) = inner.split_once(',')?;
            let x: f64 = xs.trim().parse().ok()?;
            let y: f64 = ys.trim().parse().ok()?;
            q.points
                .push((unquote(name), x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)));
        }
    }
    (!q.points.is_empty()).then_some(q)
}

fn draw_quadrant(q: &Quadrant, width: usize) -> Option<Vec<TuiLine<'static>>> {
    const MARGIN: usize = 20; // y-axis end labels sit left of the plane
    if width < MARGIN + 24 {
        return None;
    }
    let box_w = (width - MARGIN - 1).clamp(24, 48);
    let box_h = (box_w * 10 / 26).clamp(7, 15);
    let top = usize::from(q.title.is_some());
    let bot = top + box_h - 1;
    let right = MARGIN + box_w - 1;
    let mut grid = Grid::new(width, top + box_h + 1);

    if let Some(t) = &q.title {
        let t = trunc_chars(t, box_w.max(10));
        grid.text(MARGIN + (box_w - t.width()) / 2, 0, &t, BOLD_STYLE);
    }

    // the plane
    for x in MARGIN..=right {
        grid.put(x, top, '─', RULE_STYLE);
        grid.put(x, bot, '─', RULE_STYLE);
    }
    for y in top..=bot {
        grid.put(MARGIN, y, '│', RULE_STYLE);
        grid.put(right, y, '│', RULE_STYLE);
    }
    grid.put(MARGIN, top, '┌', RULE_STYLE);
    grid.put(right, top, '┐', RULE_STYLE);
    grid.put(MARGIN, bot, '└', RULE_STYLE);
    grid.put(right, bot, '┘', RULE_STYLE);

    // quadrant midlines (the dividers the name refers to)
    let mx = MARGIN + box_w / 2;
    let my = (top + bot) / 2;
    for y in top + 1..bot {
        if y % 2 == 0 {
            grid.put(mx, y, '╌', META_STYLE);
        }
    }
    for x in MARGIN + 1..right {
        if (x - MARGIN) % 2 == 0 {
            grid.put(x, my, '╌', META_STYLE);
        }
    }

    // quadrant labels in their corners, axis end labels around the box
    let half = (box_w / 2).saturating_sub(2).max(4);
    let place = |grid: &mut Grid, s: &str, x: usize, y: usize, right_align: bool| {
        let s = trunc_chars(s, half);
        let x = if right_align {
            x.saturating_sub(s.width())
        } else {
            x
        };
        grid.text(x, y, &s, META_STYLE);
    };
    place(&mut grid, &q.q[1], MARGIN + 2, top + 1, false);
    place(&mut grid, &q.q[0], right - 2, top + 1, true);
    place(&mut grid, &q.q[2], MARGIN + 2, bot - 1, false);
    place(&mut grid, &q.q[3], right - 2, bot - 1, true);

    let ylab = |grid: &mut Grid, s: &str, y: usize| {
        let s = trunc_chars(s, MARGIN - 1);
        grid.text(MARGIN - 1 - s.width(), y, &s, META_STYLE);
    };
    ylab(&mut grid, &q.y.1, top);
    ylab(&mut grid, &q.y.0, bot);
    let xlab = |grid: &mut Grid, s: &str, x: usize, right_align: bool| {
        let s = trunc_chars(s, (box_w / 2).max(6));
        let x = if right_align {
            x.saturating_sub(s.width())
        } else {
            x
        };
        grid.text(x, grid.h - 1, &s, META_STYLE);
    };
    xlab(&mut grid, &q.x.0, MARGIN, false);
    xlab(&mut grid, &q.x.1, right, true);

    // points: a `•` at the plotted cell with its name beside it —
    // right when there is room, left near the right edge — nudged a
    // row or two off collisions with other labels
    let inner_w = box_w.saturating_sub(3) as f64;
    let inner_h = (box_h - 3) as f64;
    for (name, px, py) in &q.points {
        let col = (MARGIN + 1 + (px * inner_w).round() as usize).min(right - 2);
        let row = (top + 1 + ((1.0 - py) * inner_h).round() as usize).clamp(top + 1, bot - 1);
        let room_right = right.saturating_sub(col + 2);
        let (name, nx) = if room_right >= 4 {
            (trunc_chars(name, room_right), col + 2)
        } else if col >= MARGIN + 6 {
            let n = trunc_chars(name, col - 3 - MARGIN);
            let nx = col - 2 - n.width();
            (n, nx)
        } else {
            (String::new(), col)
        };
        let x0 = nx.min(col);
        let x1 = if name.is_empty() {
            col
        } else {
            (nx + name.width()).max(col + 1) - 1
        };
        let fits = |g: &Grid, r: usize| (x0..=x1).all(|x| g.cells[r * g.w + x].1 == Style::new());
        let r = [0usize, 1, 2, 3, 4]
            .into_iter()
            .map(|d| if d % 2 == 1 { row + d / 2 + 1 } else { row })
            .filter(|r| (top + 1..bot).contains(r) && fits(&grid, *r))
            .min_by_key(|r| r.abs_diff(row))
            .unwrap_or(row);
        grid.put(col, r, '•', ACCENT_STYLE);
        if !name.is_empty() {
            grid.text(nx, r, &name, LABEL_STYLE);
        }
    }
    Some(grid.rows())
}

// ── entry point ───────────────────────────────────────────────────

/// Render a mermaid source at `width` columns; `None` when the diagram
/// kind (or its size at this width) is outside the subset — the caller
/// then keeps the plain source panel.
pub fn render(source: &str, width: u16) -> Option<Vec<TuiLine<'static>>> {
    let w = width.max(8) as usize;
    let mut lines = source.lines().filter(|l| !l.trim().is_empty());
    let head = strip_line_number(lines.next()?.trim()).trim();
    let body: Vec<&str> = lines.collect();
    if let Some(dir) = head
        .strip_prefix("flowchart ")
        .or_else(|| head.strip_prefix("graph "))
    {
        let lr = matches!(dir.trim().to_uppercase().as_str(), "LR" | "RL");
        let flow = parse_flow(&body.join("\n"), lr)?;
        draw_flow(&flow, w)
    } else if head.starts_with("sequenceDiagram") {
        draw_sequence(&body.join("\n"), w)
    } else if head.starts_with("stateDiagram") {
        let flow = parse_flow(&body.join("\n"), false)?;
        draw_flow(&flow, w)
    } else if head.starts_with("quadrantChart") {
        let q = parse_quadrant(&body.join("\n"))?;
        draw_quadrant(&q, w)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn text(rows: &[TuiLine<'static>]) -> Vec<String> {
        rows.iter()
            .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn trailing_trim(s: &str) -> String {
        s.trim_end().to_string()
    }

    #[test]
    fn flowchart_td_chain_with_labels_and_shapes() {
        let src = "flowchart TD\n\
                   A[Start] --> B{Ready?}\n\
                   B -->|yes| C[Run]\n\
                   B -->|no| D(Stop)\n";
        let rows = render(src, 60).map(|r| text(&r)).unwrap();
        let rows: Vec<String> = rows.iter().map(|r| trailing_trim(r)).collect();
        // A is rank 0; B (its target) is rank 1 — one chip per row
        assert_eq!(rows[0], "[ Start ]");
        assert!(rows.iter().any(|r| r.contains("< Ready? >")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains('▼')), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains(" yes ")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains(" no ")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("[ Run ]")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("( Stop )")), "{rows:?}");
    }

    #[test]
    fn flowchart_lr_lays_out_horizontally() {
        let src = "graph LR\nA[one] --> B[two] --> C[three]\n";
        let rows = render(src, 60).map(|r| text(&r)).unwrap();
        let row = trailing_trim(&rows[0]);
        assert!(row.contains("[ one ]"), "{row}");
        assert!(row.contains("─▶") || row.contains("──▶"), "{row}");
        assert!(row.contains("[ two ]"), "{row}");
        assert!(row.contains("[ three ]"), "{row}");
        assert_eq!(rows.len(), 1, "one rank per column, single row: {rows:?}");
    }

    #[test]
    fn state_diagram_marks_start_and_end() {
        let src = "stateDiagram-v2\n[*] --> s1\ns1 --> s2\ns2 --> [*]\n";
        let rows = render(src, 40).map(|r| text(&r)).unwrap();
        let all = rows.join("\n");
        assert!(all.contains("( • )"), "start/end chips: {all}");
        assert!(all.contains(" s1 "), "{all}");
        assert!(all.contains(" s2 "), "{all}");
        assert!(all.contains('▼'), "{all}");
    }

    #[test]
    fn sequence_messages_self_calls_notes_and_blocks() {
        let src = "sequenceDiagram\n\
                   participant A as Alice\n\
                   participant B as Bob\n\
                   A->>B: hello\n\
                   B-->>A: hi\n\
                   A->>A: think\n\
                   Note over A,B: greet first\n\
                   loop until done\n\
                   B->>A: ping\n\
                   end\n";
        let rows = render(src, 60).map(|r| text(&r)).unwrap();
        let rows: Vec<String> = rows.iter().map(|r| trailing_trim(r)).collect();
        assert!(
            rows[0].contains("Alice") && rows[0].contains("Bob"),
            "{:?}",
            rows[0]
        );
        assert!(
            rows.iter().any(|r| r.contains('▶') && r.contains("hello")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("╌") && r.contains(" hi ")),
            "{rows:?}"
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("┌") && r.contains(" think ")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("┄ greet first ┄")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("╭─ loop: until done")),
            "{rows:?}"
        );
        assert!(rows.iter().any(|r| r.contains("╰─")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains(" ping ")), "{rows:?}");
        // lifelines appear on the air rows
        assert!(
            rows.iter().filter(|r| r.contains('│')).count() >= 2,
            "{rows:?}"
        );
    }

    #[test]
    fn unsupported_kinds_fall_back_to_the_panel() {
        assert!(render("gantt\ntitle x\ndateformat YYYY-MM-DD\n", 60).is_none());
        assert!(render("pie\n\"a\": 1\n", 60).is_none());
        assert!(render("classDiagram\nA <|-- B\n", 60).is_none());
        assert!(render("erDiagram\nA ||--o{ B\n", 60).is_none());
        // subgraphs are outside the subset
        assert!(render("flowchart TD\nsubgraph one\nA --> B\nend\n", 60).is_none());
        // empty body: nothing to draw
        assert!(render("flowchart TD\n", 60).is_none());
    }

    #[test]
    fn every_row_fits_the_width_and_labels_truncate() {
        let src = "flowchart LR\n\
                   A[very long label that cannot possibly fit here] --> B[t]\n";
        for w in [12u16, 20, 34, 60] {
            // at 12 columns two chips cannot fit: the renderer declines
            // and the markdown caller keeps the source panel instead
            let Some(rows) = render(src, w).map(|r| text(&r)) else {
                assert!(w <= 14, "w{w} should have fit");
                continue;
            };
            for r in &rows {
                assert!(
                    unicode_width::UnicodeWidthStr::width(r.trim_end()) <= w as usize,
                    "w{w}: {r:?}"
                );
            }
        }
        // and a rank too dense to fit even with minimal labels
        // returns None (the markdown caller keeps the source panel)
        assert!(render("flowchart TD\nA --> B\nA --> C\nA --> D\n", 10).is_none());
    }

    #[test]
    fn cycles_and_back_edges_do_not_hang() {
        let src = "flowchart TD\nA --> B\nB --> A\nB --> C\n";
        let rows = render(src, 40).map(|r| text(&r)).unwrap();
        assert!(!rows.is_empty());
        assert!(rows.iter().any(|r| r.contains(" C ")), "{rows:?}");
    }

    /// A long LR pipeline (more ranks than the pane is wide),
    /// `~~~` invisible links and quoted `|"..."|` labels with
    /// `<br/>`: draws vertically instead of falling back, and the
    /// invisible link must not become a phantom node.
    #[test]
    fn long_lr_pipeline_with_invisible_links_draws() {
        let src = r##"flowchart LR
     Setext["Setext<br/>1992"]
     ST["StructuredText<br/>Zope, 1990s"]
     MD["Markdown<br/>2004 · Gruber"]
     Pandoc["Pandoc<br/>2006 · MacFarlane"]
     CM["CommonMark<br/>2014 · spec"]
     Djot["Djot<br/>2022 · MacFarlane"]

     Setext -->|"heading style"| MD
     Setext -->|"heading style"| RST
     MD -->|"standardized as"| CM
     CM -->|"same author,<br/>cleaner parse"| Djot
     MD -->|"dialect input"| Pandoc
     Pandoc -.->|"implements"| CM

     Textile ~~~ MD
     Org ~~~ MD
"##;
        let rows = render(src, 76).map(|r| text(&r)).unwrap();
        let all = rows.join("\n");
        assert!(!all.contains("~~~"), "{all}");
        assert!(all.contains("heading style"), "{all}");
        assert!(all.contains("same author, · cleaner parse"), "{all}");
        assert!(all.contains("Setext · 1992"), "{all}");
        // vertical fallback: ranks stack in rows, arrowheads point down
        assert!(all.contains('\u{25bc}'), "{all}");
        for r in &rows {
            assert!(
                unicode_width::UnicodeWidthStr::width(r.trim_end()) <= 76,
                "{r:?}"
            );
        }
    }

    #[test]
    fn quadrant_chart_draws_a_plane_with_points_and_labels() {
        let src = r##"quadrantChart
    title Capability vs adoption
    x-axis "Lean syntax" --> "Feature-rich syntax"
    y-axis "Small ecosystem" --> "Large ecosystem"
    quadrant-1 "Rich and widely used"
    quadrant-2 "Simple and everywhere"
    quadrant-3 "Niche and simple"
    quadrant-4 "Rich but niche"
    Markdown: [0.15, 0.95]
    CommonMark: [0.2, 0.9]
    AsciiDoc: [0.8, 0.6]
    Typst: [0.9, 0.4]
    Djot: [0.45, 0.2]
"##;
        let rows = render(src, 76).map(|r| text(&r)).unwrap();
        let all = rows.join("\n");
        assert!(all.contains("Capability vs adoption"), "{all}");
        assert!(
            all.contains('\u{250c}') && all.contains('\u{2518}'),
            "{all}"
        );
        assert!(
            all.contains("Lean syntax") && all.contains("Feature-rich"),
            "{all}"
        );
        assert!(all.contains("Large ecosystem"), "{all}");
        assert!(
            all.contains("Rich and widely used") && all.contains("Niche and simple"),
            "{all}"
        );
        assert!(all.matches('\u{2022}').count() == 5, "{all}");
        assert!(all.contains("Markdown") && all.contains("Typst"), "{all}");
        // Markdown sits high (large y), Typst low (small y)
        let md_row = rows.iter().position(|r| r.contains("Markdown")).unwrap();
        let tp_row = rows.iter().position(|r| r.contains("Typst")).unwrap();
        assert!(md_row < tp_row, "{all}");
        for r in &rows {
            assert!(
                unicode_width::UnicodeWidthStr::width(r.trim_end()) <= 76,
                "{r:?}"
            );
        }
        // no points to plot: keep the source panel
        assert!(render("quadrantChart\n    x-axis a --> b\n", 76).is_none());
        // too narrow: keep the source panel
        assert!(render(src, 30).is_none());
    }

    /// Real-world session shapes: models echo numbered files
    /// (`50\tsequenceDiagram`) and write multi-line labels with
    /// `<br/>`; both must draw, not fall back to the source panel.
    #[test]
    fn line_numbers_and_br_labels_still_draw() {
        // the fence is opened by the outer document; the renderer sees
        // the body from the head on
        let body = "50\tsequenceDiagram\n51\t    participant A as Alice\n52\t    participant B\n53\t    A->>B: hello\n";
        let rows = render(body, 60).map(|r| text(&r)).unwrap();
        let all = rows.join("\n");
        assert!(all.contains("Alice"), "{all}");
        assert!(all.contains("hello"), "{all}");
        assert!(all.contains('▶'), "{all}");

        let br = "flowchart TB\n  A[\"Markdown<br/>2004 · Gruber<br/>ubiquitous, minimal\"]\n\n  A --> B[\"Too minimal?<br/>tables, footnotes\"]\n";
        let rows = render(br, 60).map(|r| text(&r)).unwrap();
        let all = rows.join("\n");
        assert!(all.contains("Markdown · 2004"), "{all}");
        assert!(all.contains("Too minimal? · tables"), "{all}");
        assert!(all.contains('▼'), "{all}");
        // a leading digit run with a plain space is an id, not a number
        let rows = render("flowchart TD\n50 --> B[x]\n", 40)
            .map(|r| text(&r))
            .unwrap();
        assert!(rows.join("\n").contains("[ x ]"));
    }

    #[test]
    fn markdown_mermaid_fence_draws_instead_of_panel() {
        let src = "before\n\n```mermaid\nflowchart TD\nA[x] --> B[y]\n```\n\nafter\n";
        let rows = crate::markdown::render(src, 60);
        let all = text(&rows).join("\n");
        assert!(all.contains("[ x ]"), "{all}");
        assert!(all.contains("[ y ]"), "{all}");
        assert!(all.contains('▼'), "{all}");
        assert!(!all.contains("mermaid"), "no language tag row: {all}");
        assert!(all.contains("after"), "{all}");
        // an unsupported diagram keeps the source panel
        let src = "```mermaid\npie\n\"a\": 1\n```\n";
        let rows = crate::markdown::render(src, 60);
        let all = text(&rows).join("\n");
        assert!(all.contains("mermaid"), "{all}");
        assert!(all.contains("\"a\": 1"), "{all}");
    }
}
