#!/usr/bin/env python3
"""Measure Anvil idle CPU and wakeup proxies using Linux /proc (issue #63).

Run twice under the same VM, clients and configuration, once per binary version.
This is observational: context switches do not identify timer wakeups, and
mapped Mesa libraries alone do not identify the active GPU renderer.
"""
import argparse
import json
import os
import pathlib
import subprocess
import sys
import time


def parse_stat(raw):
    # /proc/PID/stat field 2 is parenthesized and can contain spaces or ')'.
    end = raw.rfind(")")
    if end < 0:
        raise ValueError("malformed /proc/PID/stat: missing command terminator")
    fields = raw[end + 1:].split()
    if len(fields) < 20:
        raise ValueError("malformed /proc/PID/stat: missing CPU/starttime fields")
    return {
        "cpu_ticks": int(fields[11]) + int(fields[12]),  # fields 14 and 15
        "starttime_ticks": int(fields[19]),              # field 22
    }


def parse_context_switches(raw):
    values = {}
    for line in raw.splitlines():
        name, separator, value = line.partition(":")
        if separator and name in ("voluntary_ctxt_switches", "nonvoluntary_ctxt_switches"):
            values[name] = int(value.strip())
    if len(values) != 2:
        raise ValueError("missing context-switch counters in task status")
    return sum(values.values())


def sample(pid, proc_root=pathlib.Path("/proc")):
    directory = proc_root / str(pid)
    counters = parse_stat((directory / "stat").read_text())
    # Count per-thread switches using stable TIDs, not the single process status.
    threads = {}
    for task in (directory / "task").iterdir():
        if not task.name.isdigit():
            continue
        try:
            threads[task.name] = parse_context_switches((task / "status").read_text())
        except FileNotFoundError:
            # A thread can exit between the task listing and status read.
            continue
    if not threads:
        raise RuntimeError("no live threads available for measurement")
    counters["threads"] = threads
    return counters


def render_libraries(pid):
    try:
        mapped = pathlib.Path(f"/proc/{pid}/maps").read_text()
    except (OSError, UnicodeError):
        return []
    markers = ("mesa", "gallium", "egl", "gbm", "dri", "virgl", "swrast", "llvmpipe")
    # Basenames avoid including personal home-directory paths in report artifacts.
    return sorted({
        pathlib.PurePath(line.split()[-1]).name for line in mapped.splitlines()
        if line.split() and "/" in line.split()[-1]
        and ".so" in line.split()[-1]
        and any(marker in line.split()[-1].lower() for marker in markers)
    })


def ipc_stats(command):
    try:
        result = subprocess.run([command, "debug", "stats"], capture_output=True,
                                text=True, timeout=5, check=False)
        if result.returncode:
            return None
        parsed = json.loads(result.stdout)
        stats = parsed.get("stats")
        return stats if isinstance(stats, dict) else None
    except (OSError, subprocess.TimeoutExpired, ValueError):
        return None


def summarize(pid, before, after, elapsed, ticks_per_second, stats_before=None, stats_after=None):
    if before["starttime_ticks"] != after["starttime_ticks"]:
        raise RuntimeError("Anvil exited or PID was reused during the sample")
    shared = before["threads"].keys() & after["threads"].keys()
    switches = sum(max(0, after["threads"][tid] - before["threads"][tid]) for tid in shared)
    cpu_ticks = after["cpu_ticks"] - before["cpu_ticks"]
    if cpu_ticks < 0:
        raise RuntimeError("process CPU ticks decreased during the sample")
    delta = {}
    if stats_before is not None and stats_after is not None:
        for key in ("requested_repaints", "render_attempts", "rendered_frames", "render_failures"):
            a, b = stats_before.get(key), stats_after.get(key)
            if isinstance(a, int) and isinstance(b, int):
                delta[key] = b - a
    return {
        "pid": pid,
        "elapsed_seconds": round(elapsed, 3),
        "cpu_time_seconds": round(cpu_ticks / ticks_per_second, 6),
        "cpu_percent_of_one_vcpu": round(100 * cpu_ticks / ticks_per_second / elapsed, 3),
        "context_switches_per_second": round(switches / elapsed, 3),
        "context_switches_common_threads": switches,
        "thread_count_start": len(before["threads"]),
        "thread_count_end": len(after["threads"]),
        "thread_churn_detected": before["threads"].keys() != after["threads"].keys(),
        "anvilctl_stats_available": stats_before is not None and stats_after is not None,
        "render_counter_deltas": delta,
    }


def find_anvil():
    result = subprocess.run(["pgrep", "-xo", "anvil"], capture_output=True,
                            text=True, check=False)
    if result.returncode or not result.stdout.strip():
        raise RuntimeError("Anvil PID not found; use --pid PID")
    return int(result.stdout.strip())


def measure(pid, duration, cli):
    before = sample(pid)
    stats_before = ipc_stats(cli) if cli else None
    start = time.monotonic()
    time.sleep(duration)
    elapsed = time.monotonic() - start
    after = sample(pid)
    stats_after = ipc_stats(cli) if cli else None
    result = summarize(pid, before, after, elapsed, os.sysconf("SC_CLK_TCK"),
                       stats_before, stats_after)
    result["mapped_renderer_library_candidates"] = render_libraries(pid)
    result["warning"] = ("Context switches are not timer firings; library names do not prove "
                         "GPU acceleration. Compare identical idle workloads and VM settings.")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=int, help="compositor PID; default: pgrep -xo anvil")
    parser.add_argument("--duration", type=float, default=60, help="idle sample seconds (default: 60)")
    parser.add_argument("--output", type=pathlib.Path, help="write full JSON result here")
    parser.add_argument("--no-ipc", action="store_true", help="skip anvilctl debug stats")
    parser.add_argument("--anvilctl", default="anvilctl", help="anvilctl executable path")
    args = parser.parse_args()
    if not 0 < args.duration <= 3600:
        parser.error("--duration must be between 0 and 3600 seconds")
    try:
        data = measure(args.pid or find_anvil(), args.duration,
                       None if args.no_ipc else args.anvilctl)
    except (OSError, RuntimeError, ValueError) as error:
        parser.exit(1, f"idle profiling failed: {error}\n")
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(data, indent=2) + "\n")
    print(json.dumps(data, indent=2))


if __name__ == "__main__":
    main()
