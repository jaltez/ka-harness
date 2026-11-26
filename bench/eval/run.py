#!/usr/bin/env python3
"""Curated real-world coding task eval runner for `ka` and `codex`.

Runs each `(task, trial, harness)` in a freshly initialized temporary git repo
with hermetic environment variables, randomized interleaved order, `/usr/bin/time -v`
resource accounting, and deterministic `verify_cmd` grading.

Usage:
  run.py --dry-run [--tasks all|<ids>]
  run.py --harnesses ka,codex --model openai/gpt-5.2@medium --codex-model gpt-5.2 \
         --trials 3 --tasks all --out /tmp/eval-out
"""
import argparse
import datetime
import json
import os
import platform
import random
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(os.path.dirname(HERE))
TASKS_PATH = os.path.join(HERE, "tasks.json")
FIXTURES_DIR = os.path.join(HERE, "fixtures")


def load_tasks(tasks_arg):
    with open(TASKS_PATH) as f:
        data = json.load(f)
    tasks = data["tasks"] if isinstance(data, dict) else data
    if tasks_arg == "all":
        return tasks
    wanted = [t.strip() for t in tasks_arg.split(",") if t.strip()]
    by_id = {t["id"]: t for t in tasks}
    missing = [w for w in wanted if w not in by_id]
    if missing:
        raise ValueError(f"unknown task id(s): {', '.join(missing)}")
    return [by_id[w] for w in wanted]


def materialize_repo(task, repo_dir):
    """Create a freshly initialized git repo populated with task baseline files."""
    os.makedirs(repo_dir, exist_ok=True)
    fixture_dir = task.get("fixture_dir")
    if fixture_dir:
        src = os.path.join(FIXTURES_DIR, fixture_dir)
        shutil.copytree(src, repo_dir, dirs_exist_ok=True)
    for rel_path, content in (task.get("setup_files") or {}).items():
        full = os.path.join(repo_dir, rel_path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w") as f:
            f.write(content)

    git_env = dict(os.environ)
    git_env.setdefault("GIT_AUTHOR_NAME", "bench")
    git_env.setdefault("GIT_AUTHOR_EMAIL", "bench@example.com")
    git_env.setdefault("GIT_COMMITTER_NAME", "bench")
    git_env.setdefault("GIT_COMMITTER_EMAIL", "bench@example.com")
    for cmd in (
        ["git", "init", "-q", "-b", "main"],
        ["git", "config", "user.name", "bench"],
        ["git", "config", "user.email", "bench@example.com"],
        ["git", "add", "-A"],
        ["git", "commit", "-q", "-m", "baseline"],
    ):
        subprocess.run(cmd, cwd=repo_dir, env=git_env, check=True, capture_output=True)


def run_verify(task, repo_dir):
    """Run deterministic task verifier; return (passed, exit_code, output, duration_s)."""
    t0 = time.monotonic()
    timeout_s = min(int(task.get("timeout_s", 120)), 180)
    try:
        proc = subprocess.run(
            ["bash", "-c", task["verify_cmd"]],
            cwd=repo_dir,
            capture_output=True,
            text=True,
            timeout=timeout_s,
        )
        dur = round(time.monotonic() - t0, 3)
        out = (proc.stdout or "") + ("\n--- STDERR ---\n" + proc.stderr if proc.stderr else "")
        return proc.returncode == 0, proc.returncode, out, dur
    except subprocess.TimeoutExpired as e:
        dur = round(time.monotonic() - t0, 3)
        out = f"VERIFY TIMEOUT after {timeout_s}s\n{(e.stdout or '')}\n{(e.stderr or '')}"
        return False, 124, out, dur


def parse_time_v(stderr_text):
    user_m = re.search(r"User time \(seconds\):\s*([0-9.]+)", stderr_text)
    sys_m = re.search(r"System time \(seconds\):\s*([0-9.]+)", stderr_text)
    rss_m = re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", stderr_text)
    return {
        "user_s": float(user_m.group(1)) if user_m else None,
        "sys_s": float(sys_m.group(1)) if sys_m else None,
        "peak_rss_kb": int(rss_m.group(1)) if rss_m else None,
    }


def make_hermetic_env(iso_dir):
    env = dict(os.environ)
    dirs = {
        "HOME": os.path.join(iso_dir, "home"),
        "XDG_CONFIG_HOME": os.path.join(iso_dir, "config"),
        "XDG_STATE_HOME": os.path.join(iso_dir, "state"),
        "XDG_DATA_HOME": os.path.join(iso_dir, "data"),
        "KA_DATA_DIR": os.path.join(iso_dir, "data", "ka"),
        "CODEX_HOME": os.path.join(iso_dir, "codex"),
        "TMPDIR": os.path.join(iso_dir, "tmp"),
    }
    for k, d in dirs.items():
        os.makedirs(d, exist_ok=True)
        env[k] = d
    # Preserve cargo/rustup homes so Rust tasks compile cleanly without re-downloading.
    if "CARGO_HOME" not in env and os.path.isdir(os.path.expanduser("~/.cargo")):
        env["CARGO_HOME"] = os.path.expanduser("~/.cargo")
    if "RUSTUP_HOME" not in env and os.path.isdir(os.path.expanduser("~/.rustup")):
        env["RUSTUP_HOME"] = os.path.expanduser("~/.rustup")
    return env


def run_dry(tasks):
    print(f"Validating {len(tasks)} eval task baselines (every task MUST fail on baseline)...")
    ok_all = True
    for task in tasks:
        tmp = tempfile.mkdtemp(prefix=f"eval-dry-{task['id']}-")
        try:
            repo_dir = os.path.join(tmp, "repo")
            materialize_repo(task, repo_dir)
            passed, rc, out, dur = run_verify(task, repo_dir)
            if passed:
                print(f"  [FAIL] {task['id']:<24} baseline PASSED unexpectedly (rc=0, {dur:.2f}s)")
                ok_all = False
            else:
                print(f"  [OK]   {task['id']:<24} baseline failed as expected (rc={rc}, {dur:.2f}s)")
        except Exception as e:
            print(f"  [ERR]  {task['id']:<24} setup/verify error: {e}")
            ok_all = False
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
    if ok_all:
        print(f"\nAll {len(tasks)} task baselines are valid (failing before agent edits).")
        return 0
    print("\nOne or more task baselines failed validation.", file=sys.stderr)
    return 1


def build_agent_cmd(harness, ka_bin, codex_bin, model, codex_model, prompt):
    if harness == "ka":
        cmd = [
            ka_bin,
            "run",
            "--print", "ndjson",
            "--ephemeral",
            "--ignore-user-config",
            "--ignore-rules",
            "--mode", "allow",
        ]
        if model:
            cmd.extend(["--model", model])
        cmd.append(prompt)
        return cmd
    if harness == "codex":
        cmd = [
            codex_bin,
            "exec",
            "--json",
            "--ephemeral",
            "--ignore-user-config",
            "--ignore-rules",
            "--skip-git-repo-check",
            "--dangerously-bypass-approvals-and-sandbox",
        ]
        if codex_model:
            cmd.extend(["-m", codex_model])
        cmd.append(prompt)
        return cmd
    raise ValueError(f"unsupported harness: {harness}")


def run_one(job, args, ka_bin, codex_bin, out_root):
    task = job["task"]
    trial = job["trial"]
    harness = job["harness"]
    run_id = f"{task['id']}__t{trial}__{harness}"
    run_dir = os.path.join(out_root, "runs", run_id)
    os.makedirs(run_dir, exist_ok=True)

    tmp = tempfile.mkdtemp(prefix=f"eval-{run_id}-")
    try:
        repo_dir = os.path.join(tmp, "repo")
        iso_dir = os.path.join(tmp, "iso")
        materialize_repo(task, repo_dir)
        env = make_hermetic_env(iso_dir)

        cmd = build_agent_cmd(harness, ka_bin, codex_bin, args.model, args.codex_model, task["prompt"])
        time_bin = "/usr/bin/time" if os.path.exists("/usr/bin/time") else shutil.which("time")
        exec_cmd = [time_bin, "-v", *cmd] if time_bin else cmd

        timeout_s = int(task.get("timeout_s", 180))
        t0 = time.monotonic()
        timed_out = False
        try:
            proc = subprocess.run(
                exec_cmd,
                cwd=repo_dir,
                env=env,
                capture_output=True,
                text=True,
                timeout=timeout_s,
            )
            agent_rc = proc.returncode
            stdout_s = proc.stdout or ""
            stderr_s = proc.stderr or ""
        except subprocess.TimeoutExpired as e:
            timed_out = True
            agent_rc = 124
            stdout_s = (e.stdout.decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or ""))
            stderr_s = (e.stderr.decode(errors="replace") if isinstance(e.stderr, bytes) else (e.stderr or ""))
        wall_s = round(time.monotonic() - t0, 3)

        with open(os.path.join(run_dir, "events.jsonl"), "w") as f:
            f.write(stdout_s)
        with open(os.path.join(run_dir, "stderr.log"), "w") as f:
            f.write(stderr_s)

        rusage = parse_time_v(stderr_s)
        diff_proc = subprocess.run(["git", "diff"], cwd=repo_dir, capture_output=True, text=True)
        status_proc = subprocess.run(["git", "status", "--short"], cwd=repo_dir, capture_output=True, text=True)
        with open(os.path.join(run_dir, "diff.patch"), "w") as f:
            f.write(diff_proc.stdout or "")

        passed, verify_rc, verify_out, verify_s = run_verify(task, repo_dir)
        with open(os.path.join(run_dir, "verify.log"), "w") as f:
            f.write(verify_out)

        meta = {
            "run_id": run_id,
            "task_id": task["id"],
            "title": task["title"],
            "category": task["category"],
            "trial": trial,
            "harness": harness,
            "model": args.model if harness == "ka" else args.codex_model,
            "cmd": cmd,
            "timeout_s": timeout_s,
            "timed_out": timed_out,
            "agent_rc": agent_rc,
            "wall_s": wall_s,
            "rusage": rusage,
            "has_edits": bool((status_proc.stdout or "").strip()),
            "git_status": (status_proc.stdout or "").strip().splitlines(),
            "passed": passed,
            "verify_rc": verify_rc,
            "verify_s": verify_s,
        }
        with open(os.path.join(run_dir, "meta.json"), "w") as f:
            json.dump(meta, f, indent=2)
        return meta
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser(description="Curated coding task eval runner (ka vs codex)")
    ap.add_argument("--harnesses", default="ka,codex", help="Comma-separated harnesses: ka,codex")
    ap.add_argument("--model", default="", help="Ka model selector (vendor/model[@effort])")
    ap.add_argument("--codex-model", default="", help="Codex model name (e.g. gpt-5.2)")
    ap.add_argument("--trials", type=int, default=3, help="Trials per (task, harness) (default: 3)")
    ap.add_argument("--tasks", default="all", help="'all' or comma-separated task IDs")
    ap.add_argument("--out", default=None, help="Output directory for run artifacts")
    ap.add_argument("--ka", default=None, help="Path to ka binary")
    ap.add_argument("--codex", default=None, help="Path to codex binary")
    ap.add_argument("--seed", type=int, default=42, help="PRNG seed for interleaved order")
    ap.add_argument("--dry-run", action="store_true", help="Validate that all task baselines fail verify_cmd")
    args = ap.parse_args()

    tasks = load_tasks(args.tasks)
    if args.dry_run:
        return run_dry(tasks)

    if not args.out:
        print("run.py: --out <dir> is required unless --dry-run is set", file=sys.stderr)
        return 2

    harnesses = [h.strip() for h in args.harnesses.split(",") if h.strip()]
    ka_bin = args.ka or (
        os.path.join(REPO_ROOT, "target", "release", "ka")
        if os.path.isfile(os.path.join(REPO_ROOT, "target", "release", "ka"))
        else shutil.which("ka")
    )
    codex_bin = args.codex or shutil.which("codex")
    if "ka" in harnesses:
        if not ka_bin or not os.path.exists(ka_bin):
            print("run.py: ka binary not found (pass --ka PATH)", file=sys.stderr)
            return 2
        ka_bin = os.path.abspath(ka_bin)
    if "codex" in harnesses:
        if not codex_bin or not os.path.exists(codex_bin):
            print("run.py: codex binary not found (pass --codex PATH)", file=sys.stderr)
            return 2
        codex_bin = os.path.abspath(codex_bin)

    os.makedirs(args.out, exist_ok=True)
    jobs = []
    for trial in range(1, args.trials + 1):
        for task in tasks:
            for harness in harnesses:
                jobs.append({"task": task, "trial": trial, "harness": harness})

    rng = random.Random(args.seed)
    rng.shuffle(jobs)

    manifest = {
        "schema_version": 1,
        "timestamp_utc": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "host": {
            "os": platform.system(),
            "kernel": platform.release(),
            "arch": platform.machine(),
            "cpu_count": os.cpu_count(),
        },
        "harnesses": harnesses,
        "model": args.model,
        "codex_model": args.codex_model,
        "trials": args.trials,
        "seed": args.seed,
        "task_ids": [t["id"] for t in tasks],
        "schedule": [{"task_id": j["task"]["id"], "trial": j["trial"], "harness": j["harness"]} for j in jobs],
    }
    with open(os.path.join(args.out, "manifest.json"), "w") as f:
        json.dump(manifest, f, indent=2)

    print(f"Running {len(jobs)} eval jobs ({len(tasks)} tasks x {args.trials} trials x {len(harnesses)} harnesses)...")
    for idx, job in enumerate(jobs, 1):
        meta = run_one(job, args, ka_bin, codex_bin, args.out)
        status = "PASS" if meta["passed"] else ("TIMEOUT" if meta["timed_out"] else "FAIL")
        print(
            f"[{idx:>3}/{len(jobs)}] {meta['harness']:<5} {meta['task_id']:<22} t{meta['trial']} "
            f"{status:<7} wall={meta['wall_s']:.1f}s"
        )
    print(f"\nDone. Generate report with:\n  python3 bench/eval/report.py --in {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
