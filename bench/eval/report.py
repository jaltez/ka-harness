#!/usr/bin/env python3
"""Normalized report generator for `bench/eval/run.py` artifacts.

Parses both Ka's NDJSON event stream (`turn_finished`, `call_started`,
`call_finished`) and Codex's `--json` stream (`turn.completed`,
`item.completed`) into one normalized JSON + Markdown report with a single
shared USD price table.

Usage:
  report.py --in <eval_dir> [--out-json report.json] [--out-md report.md] [--prices prices.json]
"""
import argparse
import collections
import glob
import json
import os
import statistics
import sys

# Shared USD price table per 1,000,000 tokens: (input, cached_input, output, reasoning).
# Both harnesses are priced through this exact table so cost comparisons reflect
# token efficiency rather than differing built-in cost estimators.
DEFAULT_PRICES_PER_M = {
    "gpt-5": {"input": 1.25, "cached_input": 0.125, "output": 10.0, "reasoning": 10.0},
    "gpt-5.2": {"input": 1.75, "cached_input": 0.175, "output": 14.0, "reasoning": 14.0},
    "gpt-5-codex": {"input": 1.25, "cached_input": 0.125, "output": 10.0, "reasoning": 10.0},
    "gpt-5-mini": {"input": 0.25, "cached_input": 0.025, "output": 2.0, "reasoning": 2.0},
    "gpt-4.1": {"input": 2.0, "cached_input": 0.5, "output": 8.0, "reasoning": 8.0},
    "o3": {"input": 2.0, "cached_input": 0.5, "output": 8.0, "reasoning": 8.0},
    "o4-mini": {"input": 1.1, "cached_input": 0.275, "output": 4.4, "reasoning": 4.4},
    "claude-sonnet-4": {"input": 3.0, "cached_input": 0.3, "output": 15.0, "reasoning": 15.0},
    "claude-opus-4": {"input": 15.0, "cached_input": 1.5, "output": 75.0, "reasoning": 75.0},
    "_default": {"input": 2.0, "cached_input": 0.25, "output": 10.0, "reasoning": 10.0},
}


def lookup_price(model_name, table):
    if not model_name:
        return table["_default"]
    norm = model_name.lower().split("@", 1)[0]
    if "/" in norm:
        norm = norm.split("/", 1)[1]
    if norm in table:
        return table[norm]
    for key in sorted(table.keys(), key=len, reverse=True):
        if key != "_default" and norm.startswith(key):
            return table[key]
    return table["_default"]


def compute_cost_usd(tokens, price):
    return (
        tokens["input"] * price["input"]
        + tokens["cached_input"] * price["cached_input"]
        + tokens["output"] * price["output"]
        + tokens["reasoning"] * price["reasoning"]
    ) / 1_000_000.0


def parse_ka_events(path):
    """Parse Ka's `--print ndjson` stream."""
    tokens = {"input": 0, "cached_input": 0, "output": 0, "reasoning": 0}
    round_trips = 0
    tool_calls = 0
    tool_errors = 0
    agent_errors = []
    stop_reason = None

    if not os.path.exists(path):
        return tokens, round_trips, tool_calls, tool_errors, agent_errors, stop_reason

    with open(path, errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line or not line.startswith("{"):
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            etype = ev.get("type", "")
            if etype == "turn_started":
                round_trips += 1
            elif etype == "call_started":
                tool_calls += 1
            elif etype == "call_finished":
                if ev.get("ok") is False:
                    tool_errors += 1
            elif etype == "turn_finished":
                stop_reason = ev.get("stop")
                u = ev.get("usage") or {}
                tokens["input"] += int(u.get("input") or 0)
                tokens["cached_input"] += int(u.get("cache_read") or 0)
                tokens["output"] += int(u.get("output") or 0)
                tokens["reasoning"] += int(u.get("reasoning") or 0)
            elif etype == "error":
                agent_errors.append(ev.get("message") or str(ev))

    return tokens, round_trips, tool_calls, tool_errors, agent_errors, stop_reason


def parse_codex_events(path):
    """Parse Codex CLI's `codex exec --json` stream."""
    tokens = {"input": 0, "cached_input": 0, "output": 0, "reasoning": 0}
    round_trips = 0
    tool_calls = 0
    tool_errors = 0
    agent_errors = []
    stop_reason = None

    tool_item_types = {"command_execution", "file_change", "mcp_tool_call", "web_search", "function_call"}

    if not os.path.exists(path):
        return tokens, round_trips, tool_calls, tool_errors, agent_errors, stop_reason

    with open(path, errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line or not line.startswith("{"):
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            etype = ev.get("type", "")
            if etype == "turn.started":
                round_trips += 1
            elif etype == "item.completed":
                item = ev.get("item") or {}
                itype = item.get("type", "")
                if itype in tool_item_types:
                    tool_calls += 1
                    status = item.get("status", "")
                    exit_code = item.get("exit_code")
                    if status == "failed" or (isinstance(exit_code, int) and exit_code != 0):
                        tool_errors += 1
            elif etype == "turn.completed":
                stop_reason = "done"
                u = ev.get("usage") or {}
                raw_in = int(u.get("input_tokens") or 0)
                cached = int(u.get("cached_input_tokens") or (u.get("input_tokens_details") or {}).get("cached_tokens") or 0)
                out_tok = int(u.get("output_tokens") or 0)
                reas = int(
                    u.get("reasoning_output_tokens")
                    or (u.get("output_tokens_details") or {}).get("reasoning_tokens")
                    or 0
                )
                # Normalize: input = uncached input, output = non-reasoning output (or total if reasoning not split)
                tokens["input"] += max(0, raw_in - cached)
                tokens["cached_input"] += cached
                tokens["output"] += max(0, out_tok - reas) if reas <= out_tok else out_tok
                tokens["reasoning"] += reas
            elif etype in ("turn.failed", "error"):
                stop_reason = "error"
                err = ev.get("error") or ev.get("message") or str(ev)
                agent_errors.append(err if isinstance(err, str) else json.dumps(err))

    return tokens, round_trips, tool_calls, tool_errors, agent_errors, stop_reason


def classify_failure(meta, agent_errors):
    if meta.get("passed"):
        return None
    if meta.get("timed_out"):
        return "timeout"
    if agent_errors or (meta.get("agent_rc", 0) not in (0, 124)):
        return "agent_error"
    if not meta.get("has_edits"):
        return "no_edits"
    return "test_failure"


def aggregate_harness(runs):
    if not runs:
        return {}
    by_task = collections.defaultdict(list)
    for r in runs:
        by_task[r["task_id"]].append(r)

    total_runs = len(runs)
    passed_runs = sum(1 for r in runs if r["passed"])
    tasks_all_pass = sum(1 for rs in by_task.values() if all(x["passed"] for x in rs))
    tasks_any_pass = sum(1 for rs in by_task.values() if any(x["passed"] for x in rs))
    n_tasks = len(by_task)

    tok_keys = ("input", "cached_input", "output", "reasoning")
    tok_totals = {k: sum(r["tokens"][k] for r in runs) for k in tok_keys}
    tok_means = {k: round(tok_totals[k] / total_runs, 1) for k in tok_keys}

    total_cost = sum(r["cost_usd"] for r in runs)
    walls = [r["wall_s"] for r in runs]
    cpus = [
        (r["rusage"]["user_s"] + r["rusage"]["sys_s"])
        for r in runs
        if r.get("rusage") and r["rusage"].get("user_s") is not None and r["rusage"].get("sys_s") is not None
    ]
    rss_mibs = [
        r["rusage"]["peak_rss_kb"] / 1024.0
        for r in runs
        if r.get("rusage") and r["rusage"].get("peak_rss_kb") is not None
    ]
    total_calls = sum(r["tool_calls"] for r in runs)
    total_call_errs = sum(r["tool_errors"] for r in runs)
    total_rts = sum(r["round_trips"] for r in runs)

    failures = collections.Counter(r["failure_class"] for r in runs if r["failure_class"])

    return {
        "runs": total_runs,
        "tasks": n_tasks,
        "passed_runs": passed_runs,
        "pass_at_1": round(passed_runs / total_runs, 4),
        "consistency_all_pass": round(tasks_all_pass / n_tasks, 4) if n_tasks else 0.0,
        "consistency_any_pass": round(tasks_any_pass / n_tasks, 4) if n_tasks else 0.0,
        "tokens_total": tok_totals,
        "tokens_mean": tok_means,
        "cost_usd_total": round(total_cost, 4),
        "cost_usd_mean": round(total_cost / total_runs, 4),
        "wall_s_p50": round(statistics.median(walls), 2) if walls else None,
        "wall_s_mean": round(statistics.mean(walls), 2) if walls else None,
        "wall_s_total": round(sum(walls), 2),
        "round_trips_mean": round(total_rts / total_runs, 2),
        "tool_calls_total": total_calls,
        "tool_calls_mean": round(total_calls / total_runs, 2),
        "tool_error_rate": round(total_call_errs / total_calls, 4) if total_calls else 0.0,
        "cpu_s_mean": round(statistics.mean(cpus), 3) if cpus else None,
        "peak_rss_mib_p50": round(statistics.median(rss_mibs), 2) if rss_mibs else None,
        "failure_taxonomy": dict(failures),
        "per_task": {
            tid: {
                "passed": sum(1 for x in rs if x["passed"]),
                "trials": len(rs),
                "pass_rate": round(sum(1 for x in rs if x["passed"]) / len(rs), 3),
                "wall_s_p50": round(statistics.median(x["wall_s"] for x in rs), 2),
                "cost_usd_mean": round(statistics.mean(x["cost_usd"] for x in rs), 4),
            }
            for tid, rs in sorted(by_task.items())
        },
    }


def render_markdown(report):
    harnesses = sorted(report["summary"].keys())
    lines = ["# Coding Task Eval Report", ""]
    man = report.get("manifest") or {}
    if man:
        lines.append(
            f"- **Date (UTC):** {man.get('timestamp_utc', 'n/a')}  \n"
            f"- **Models:** ka=`{man.get('model', 'n/a')}` · codex=`{man.get('codex_model', 'n/a')}`  \n"
            f"- **Trials:** {man.get('trials', 'n/a')} · **Seed:** {man.get('seed', 'n/a')}"
        )
        lines.append("")

    header = "| Metric | " + " | ".join(f"`{h}`" for h in harnesses) + " |"
    sep = "|---|" + "|".join("---:" for _ in harnesses) + "|"
    lines.extend([header, sep])

    def row(label, fn):
        vals = [fn(report["summary"][h]) for h in harnesses]
        lines.append("| " + label + " | " + " | ".join(vals) + " |")

    row("`pass@1`", lambda s: f"{s['pass_at_1']*100:.1f}% ({s['passed_runs']}/{s['runs']})")
    row("Pass consistency (all trials)", lambda s: f"{s['consistency_all_pass']*100:.1f}%")
    row("Pass consistency (any trial)", lambda s: f"{s['consistency_any_pass']*100:.1f}%")
    row("Mean uncached input tokens", lambda s: f"{s['tokens_mean']['input']:,.0f}")
    row("Mean cached input tokens", lambda s: f"{s['tokens_mean']['cached_input']:,.0f}")
    row("Mean output tokens", lambda s: f"{s['tokens_mean']['output']:,.0f}")
    row("Mean reasoning tokens", lambda s: f"{s['tokens_mean']['reasoning']:,.0f}")
    row("Mean USD cost (shared table)", lambda s: f"${s['cost_usd_mean']:.4f}")
    row("Total USD cost", lambda s: f"${s['cost_usd_total']:.4f}")
    row("Wall time (p50 / mean)", lambda s: f"{s['wall_s_p50']}s / {s['wall_s_mean']}s")
    row("Mean model round-trips", lambda s: f"{s['round_trips_mean']:.2f}")
    row("Mean tool calls (error rate)", lambda s: f"{s['tool_calls_mean']:.1f} ({s['tool_error_rate']*100:.1f}%)")
    row("Harness CPU (mean)", lambda s: f"{s['cpu_s_mean']:.2f}s" if s["cpu_s_mean"] is not None else "n/a")
    row("Harness peak RSS (p50)", lambda s: f"{s['peak_rss_mib_p50']:.1f} MiB" if s["peak_rss_mib_p50"] is not None else "n/a")
    row("Failures", lambda s: ", ".join(f"{k}:{v}" for k, v in sorted(s["failure_taxonomy"].items())) or "none")

    lines.extend(["", "## Per-Task Breakdown", ""])
    all_tasks = sorted({tid for h in harnesses for tid in report["summary"][h]["per_task"].keys()})
    lines.append("| Task | " + " | ".join(f"`{h}` pass (wall / cost)" for h in harnesses) + " |")
    lines.append("|---|" + "|".join("---:" for _ in harnesses) + "|")
    for tid in all_tasks:
        cells = []
        for h in harnesses:
            t = report["summary"][h]["per_task"].get(tid)
            if not t:
                cells.append("n/a")
            else:
                cells.append(f"{t['passed']}/{t['trials']} ({t['wall_s_p50']}s / ${t['cost_usd_mean']:.4f})")
        lines.append(f"| `{tid}` | " + " | ".join(cells) + " |")
    lines.append("")
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser(description="Parse ka & codex eval runs into normalized JSON + Markdown")
    ap.add_argument("--in", dest="in_dir", required=True, help="Eval output directory from run.py")
    ap.add_argument("--out-json", default=None, help="Write normalized JSON report here")
    ap.add_argument("--out-md", default=None, help="Write Markdown report here")
    ap.add_argument("--prices", default=None, help="Optional JSON price table override")
    args = ap.parse_args()

    prices = dict(DEFAULT_PRICES_PER_M)
    if args.prices:
        with open(args.prices) as f:
            prices.update(json.load(f))

    manifest = {}
    man_path = os.path.join(args.in_dir, "manifest.json")
    if os.path.exists(man_path):
        with open(man_path) as f:
            manifest = json.load(f)

    meta_files = sorted(glob.glob(os.path.join(args.in_dir, "runs", "*", "meta.json")))
    if not meta_files:
        print(f"report.py: no run meta.json files found under {args.in_dir}/runs/", file=sys.stderr)
        return 2

    normalized_runs = []
    by_harness = collections.defaultdict(list)
    for mp in meta_files:
        with open(mp) as f:
            meta = json.load(f)
        ev_path = os.path.join(os.path.dirname(mp), "events.jsonl")
        harness = meta["harness"]
        if harness == "ka":
            tokens, rts, calls, call_errs, agent_errs, stop = parse_ka_events(ev_path)
        else:
            tokens, rts, calls, call_errs, agent_errs, stop = parse_codex_events(ev_path)

        price = lookup_price(meta.get("model"), prices)
        cost = compute_cost_usd(tokens, price)
        rec = {
            **meta,
            "tokens": tokens,
            "cost_usd": round(cost, 6),
            "round_trips": rts,
            "tool_calls": calls,
            "tool_errors": call_errs,
            "agent_errors": agent_errs,
            "stop_reason": stop,
            "failure_class": classify_failure(meta, agent_errs),
        }
        normalized_runs.append(rec)
        by_harness[harness].append(rec)

    report = {
        "schema_version": 1,
        "manifest": manifest,
        "price_table_per_1m": prices,
        "summary": {h: aggregate_harness(rs) for h, rs in sorted(by_harness.items())},
        "runs": normalized_runs,
    }

    md = render_markdown(report)
    print(md)
    if args.out_md:
        os.makedirs(os.path.dirname(os.path.abspath(args.out_md)), exist_ok=True)
        with open(args.out_md, "w") as f:
            f.write(md)
    if args.out_json:
        os.makedirs(os.path.dirname(os.path.abspath(args.out_json)), exist_ok=True)
        with open(args.out_json, "w") as f:
            json.dump(report, f, indent=2)
    return 0


if __name__ == "__main__":
    sys.exit(main())
