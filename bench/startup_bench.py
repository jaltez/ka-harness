#!/usr/bin/env python3
"""Startup bench: what a user waits for before any work happens.

Measures the paths a user waits on, in a hermetic environment, against
one or two binaries with the runs interleaved:

  version_us     `ka --version`, fresh process each run
  run_ms         one headless turn with the offline canned speaker
  run_cpu_ms     user+sys CPU of that turn (rusage of the child)
  tui_ready_ms   `ka` in a pty until the first frame carries the composer

Each side runs in its own HOME, config, state, data and tmp directories
under --work. The state directory holds --spills job spill files unless
--spills is 0, the realistic case: a machine with a week of history.

Every timed run is checked: a run whose output is missing what the
path must print counts as an error, never as a fast run. The JSON line
ends with the error count and the work count.

usage: startup_bench.py --side NAME=PATH [--side NAME=PATH ...] [--spills N]
                        [--runs N] [--repo [--repo-files N]] [--tui-discovery]
                        [--work DIR] [--syscalls] [--skip-tui]

The headless turn includes 60 ms of pacing by the canned speaker, a
constant across builds: compare builds with it, and never read it as
the cost of a real turn.
"""
import argparse
import fcntl
import json
import os
import pty
import resource
import select
import shutil
import struct
import subprocess
import sys
import termios
import time

KA_FLAGS = ["--offline", "--ignore-user-config", "--ephemeral", "--no-discovery"]
RUN_PROMPT = "say hi in three words"
RUN_MUST_PRINT = b"heard:"
TUI_MUST_PRINT = b"Ask anything"
NOW_MS = 1795521590416  # a fixed "now" so the spill names are reproducible


def make_spills(state_dir, count):
    """A realistic spill directory: `count` files spread over six days,
    two in seven carrying a `.done` marker, as on a real machine."""
    spills = os.path.join(state_dir, "ka", "spills")
    marker = os.path.join(state_dir, "ka", ".harness-count")
    have = -1
    try:
        with open(marker) as f:
            have = int(f.read().strip())
    except (OSError, ValueError):
        pass
    if have == count and os.path.isdir(spills):
        return
    shutil.rmtree(state_dir, ignore_errors=True)
    os.makedirs(spills, exist_ok=True)
    six_days_ms = 6 * 24 * 60 * 60 * 1000
    body = b"x" * 2048
    for i in range(count):
        stamp = NOW_MS - (i * six_days_ms // max(count, 1))
        name = f"job-{stamp}-{i}"
        with open(os.path.join(spills, name), "wb") as f:
            f.write(body)
        if i % 7 == 0:
            with open(os.path.join(spills, name + ".done"), "wb") as f:
                f.write(b"")
    with open(os.path.join(state_dir, "ka", "jobs.jsonl"), "wb"):
        pass
    with open(marker, "w") as f:
        f.write(str(count))

def side_env(work, name, spills, repo=False, files=40):
    root = os.path.join(work, name)
    shutil.rmtree(root, ignore_errors=True)
    home = os.path.join(root, "home")
    env = {
        "HOME": home,
        "XDG_CONFIG_HOME": os.path.join(root, "config"),
        "XDG_STATE_HOME": os.path.join(root, "state"),
        "XDG_DATA_HOME": os.path.join(root, "data"),
        "KA_DATA_DIR": os.path.join(root, "data", "ka"),
        "TMPDIR": os.path.join(root, "tmp"),
        "PATH": os.environ.get("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
    }
    for key in ("HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME", "KA_DATA_DIR", "TMPDIR"):
        os.makedirs(env[key], exist_ok=True)
    cwd = os.path.join(root, "cwd")
    os.makedirs(cwd, exist_ok=True)

    if repo:
        # a tracked tree of `files` files: ka's git awareness walks it every turn
        git = ["git", "-C", cwd]
        for step in (["init", "-q", "-b", "main"], ["config", "user.email", "t@t"],
                     ["config", "user.name", "t"]):
            subprocess.run(git + step, env=env, capture_output=True)
        for i in range(files):
            with open(os.path.join(cwd, f"file{i}.txt"), "w") as f:
                f.write("x" * 200)
        subprocess.run(git + ["add", "-A"], env=env, capture_output=True)
        subprocess.run(git + ["commit", "-q", "-m", "x"], env=env, capture_output=True)
    if spills:
        make_spills(env["XDG_STATE_HOME"], spills)
    return env, cwd


def child_cpu_ms():
    r = resource.getrusage(resource.RUSAGE_CHILDREN)
    return (r.ru_utime + r.ru_stime) * 1000.0


def time_version(path, env, cwd):
    t = time.perf_counter_ns()
    out = subprocess.run([path, "--version"], cwd=cwd, env=env, capture_output=True)
    us = (time.perf_counter_ns() - t) / 1000
    ok = out.returncode == 0 and out.stdout.startswith(b"ka ")
    return us, ok


def time_run(path, env, cwd):
    before = child_cpu_ms()
    t = time.perf_counter_ns()
    out = subprocess.run([path, "run", RUN_PROMPT, *KA_FLAGS], cwd=cwd, env=env, capture_output=True)
    ms = (time.perf_counter_ns() - t) / 1e6
    cpu = child_cpu_ms() - before
    ok = out.returncode == 0 and RUN_MUST_PRINT in out.stdout
    return ms, cpu, ok


def time_tui(path, env, cwd, flags=KA_FLAGS, cols=120, rows=40):
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execve(path, [path, *flags], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    t = time.perf_counter_ns()
    buf = b""
    ok = False
    deadline = time.monotonic() + 20
    try:
        while time.monotonic() < deadline:
            if not select.select([fd], [], [], 0.5)[0]:
                continue
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            buf += chunk
            if TUI_MUST_PRINT in buf:
                ok = True
                break
    finally:
        ms = (time.perf_counter_ns() - t) / 1e6
        try:
            os.kill(pid, 9)
        except OSError:
            pass
        os.waitpid(pid, 0)
        os.close(fd)
    return ms, ok


def stats(values):
    v = sorted(values)
    n = len(v)
    return {
        "p50": v[n // 2],
        "min": v[0],
        "max": v[-1],
        "runs": n,
    }


def syscall_counts(path, env, cwd):
    """One traced run of each path, as a diagnostic: the totals and the
    calls that explain them. Tracing slows the run, so these counts are
    reported apart from the timings."""
    out = {}
    for label, argv in (("version", [path, "--version"]), ("run", [path, "run", RUN_PROMPT, *KA_FLAGS])):
        tmp = os.path.join(cwd, f"strace-{label}.txt")
        subprocess.run(["strace", "-c", "-f", "-o", tmp, *argv], cwd=cwd, env=env, capture_output=True)
        counts = {}
        total = 0
        try:
            with open(tmp) as f:
                lines = f.read().splitlines()
        except OSError:
            lines = []
        # strace -c rows: %time seconds usecs/call calls [errors] syscall.
        # The summary row is named `total` and is skipped.
        for line in lines:
            parts = line.split()
            if len(parts) not in (5, 6) or parts[-1] == "total":
                continue
            calls = parts[-2] if len(parts) == 5 else parts[-3]
            if calls.isdigit():
                counts[parts[-1]] = counts.get(parts[-1], 0) + int(calls)
        total = sum(counts.values())
        out[label] = {
            "total": total,
            "lstat": counts.get("lstat", 0),
            "execve": counts.get("execve", 0),
            "getdents64": counts.get("getdents64", 0),
            "openat": counts.get("openat", 0) + counts.get("open", 0),
            "mmap": counts.get("mmap", 0),
        }
        try:
            os.remove(tmp)
        except OSError:
            pass
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--side", action="append", required=True, help="NAME=PATH, repeatable")
    ap.add_argument("--spills", type=int, default=9000)
    ap.add_argument("--runs", type=int, default=15, help="runs per side for run and tui")
    ap.add_argument("--version-runs", type=int, default=40)
    ap.add_argument("--work", default="/tmp/ka-startup-bench")
    ap.add_argument("--syscalls", action="store_true")
    ap.add_argument("--skip-tui", action="store_true")
    ap.add_argument("--repo", action="store_true",
                    help="give each side a small git repository as its working directory")
    ap.add_argument("--repo-files", type=int, default=40,
                    help="tracked files in that repository (40 is small; try 20000 for a large one)")
    ap.add_argument("--tui-discovery", action="store_true",
                    help="launch the TUI without --no-discovery, as a user passing only --offline gets")
    args = ap.parse_args()

    sides = []
    for spec in args.side:
        name, path = spec.split("=", 1)
        path = os.path.abspath(path)
        env, cwd = side_env(args.work, name, args.spills, args.repo, args.repo_files)
        sides.append({"name": name, "path": path, "env": env, "cwd": cwd})

    tui_flags = [f for f in KA_FLAGS if f != "--no-discovery"] if args.tui_discovery else KA_FLAGS
    result = {"spills": args.spills, "tui_discovery": args.tui_discovery, "repo": args.repo,
              "loadavg": os.getloadavg()[0], "errors": 0, "sides": {}}
    for s in sides:
        result["sides"][s["name"]] = {
            "bytes": os.path.getsize(s["path"]),
            "version_us": [],
            "run_ms": [],
            "run_cpu_ms": [],
            "tui_ready_ms": [],
        }

    # warm-up: page cache and the first-run layout of each side's dirs
    for s in sides:
        for _ in range(3):
            time_run(s["path"], s["env"], s["cwd"])
            time_version(s["path"], s["env"], s["cwd"])

    order = list(sides)
    for i in range(max(args.runs, args.version_runs)):
        for s in order if i % 2 == 0 else order[::-1]:
            r = result["sides"][s["name"]]
            if i < args.version_runs:
                us, ok = time_version(s["path"], s["env"], s["cwd"])
                r["version_us"].append(us)
                result["errors"] += not ok
            if i < args.runs:
                ms, cpu, ok = time_run(s["path"], s["env"], s["cwd"])
                r["run_ms"].append(ms)
                r["run_cpu_ms"].append(cpu)
                result["errors"] += not ok
                if not args.skip_tui:
                    tms, tok = time_tui(s["path"], s["env"], s["cwd"], flags=tui_flags)
                    r["tui_ready_ms"].append(tms)
                    result["errors"] += not tok

    runs_per_side = args.version_runs + args.runs * (1 if args.skip_tui else 2) + 3
    result["work"] = {"runs_per_side": runs_per_side, "sides": len(sides)}
    for s in sides:
        r = result["sides"][s["name"]]
        out = {"bytes": r["bytes"]}
        for key in ("version_us", "run_ms", "run_cpu_ms", "tui_ready_ms"):
            if r[key]:
                out[key] = stats(r[key])
        if args.syscalls:
            out["syscalls"] = syscall_counts(s["path"], s["env"], s["cwd"])
        result["sides"][s["name"]] = out
    print(json.dumps(result))


if __name__ == "__main__":
    sys.exit(main())
