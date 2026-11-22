# Changelog

## Unreleased — feature lots (2026-11-22)

### Batch 5 — customizable TUI
- Themes as data: `[tui] theme` + `/theme` picker (`.ka/themes/*.toml`, user dir; Nocturne fallback).
- Remappable keybindings: `[keybindings]` action-id → chord table (hard error on unknown action or doubled key); `/help` shows effective bindings.
- Segmented status line: `[tui] status_line = [...]` (model/mode/session/effort/context/cost/tokens/cache/toggles/update).
- Notification tuning: `[tui] notify_on = [...]`, `[tui] notify_unfocused` (focus reporting where the terminal supports it).
- Dynamic terminal title: `[tui] title` (default on).
- `/copy` block picker (copy or write to file) · `/search <text>` transcript search · `/changelog` · `/queue` follow-up queue view · `:emoji:` shortcodes on completion.

### Batch 4 — context surgery & automation
- `/diff` session cumulative diff (Myers, spill-backed) · `/compactfrom [N] [focus]` rewind+summary (also in the Rewind modal) · `ContextEdit` append-only message edits (protocol) · `/btw` side questions · `/goal` with budget and ◎ badge · `/loop [--every Ns]` with ↻ badge · `[context] prewalk` repo brief · `MemoryConsolidate` (MEMORY.md rewrite from inbox, undo-able).

### Batch 3 — sessions & context (cheap)
- Session pins (★) · branch summaries on rewind · `ka gc` · idle recaps · fuzzy session picker · `/shake [thinking|images]` · Ctrl+S draft stash · `/focus` · `/context` hints · `/autocompact N|off`.

### Batch 2 — structural CLI/config
- `--profile` · `--add-dir` · `ka queue` · `ka sandbox` runner · `ka import codex|gemini` (+ honest cursor guidance) · `ka import-session claude|codex` · `ka run --rpc` · `ka session rename|delete|archive|unarchive` · `KA_<KEY>` env layer (`__` = dot) · arrays merge across layers (specific-first).

### Batch 1 — CLI/config quick wins
- `--effort` · `--image` · `--offline` · `--system-prompt` · `--tools`/`--exclude-tools` · `--ignore-user-config` · `--ignore-rules` · `--ephemeral` · `ka run -o/--max-turns/--max-budget-usd/--max-cost/--warn-cost/--max-time` (exit 3 caps) · `ka config set/get/path` · `ka models <query>` · `ka export --jsonl` · `[context] prewalk` · `[prompt] identity_inline` · `[features] allow` · `[history]` · `[[model_pins]]` · `project_doc_fallback_filenames` + `AGENTS.override.md` · `#:schema` sidecar on save.

### Batch 0 — hygiene
- Overlay fix ([lsp]/[debug]) · strict `[tui] mouse`/`[tools.mcp] discovery` enums · doc↔code drift fixes · hermetic serve tests.

### Earlier batches (v0.7.2)
- See `research/ka-harness/` and the GitHub release notes.
