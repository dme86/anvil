# Measuring idle CPU usage (issue #63)

This is a **measurement recipe**, not evidence that Anvil has reached a particular
CPU target. In a QEMU TCG x86_64 guest on Apple Silicon, the old direct DRM
implementation used about **6.05% of one emulated vCPU** over a 20-second
sample with approximately **47.9 context switches/s**. That is a *pre-fix*
sample, not a guaranteed post-fix baseline.

Anvil v0.6.2 removed two sources of redundant idle work: per-dispatch
`wait4(WNOHANG)` polling of long-running children (now Linux pidfds with a
bounded fallback), and a periodic 16-ms direct DRM repaint scheduler
(now event-driven with a separate bar timer and delayed error retry).
Neither change establishes a measured CPU improvement until the same workload
is sampled with comparable settings.

## Reproducible guest sampling

From an idle Anvil session on Linux, ideally via SSH to avoid interacting with
the desktop during the measurement:

```sh
# Record which Anvil binary and VM are being tested.
anvil --version 2>/dev/null || true   # version flag may not be supported
sha256sum /usr/local/bin/anvil
uname -a
lscpu | grep -E 'Architecture|Model name|CPU\\(s\\)' || true

# Run with the same startup children, windows, bar settings, VM and display mode.
# If running over SSH without XDG_RUNTIME_DIR, pass --no-ipc; use anvilctl inside
# the logged-in desktop session to collect optional render counters instead.
python3 scripts/idle_cpu_profile.py --duration 60 --output /tmp/anvil-063-idle.json
```

The script records Linux process CPU ticks, context switches on TIDs present
at both endpoints, thread churn, and, if `anvilctl debug stats` is reachable,
deltas of render attempts/submissions/failures and requested repaints. It also
lists candidate Mesa/EGL/DRI mapped library **basenames**. These cannot prove
whether the actual renderer is llvmpipe, softpipe, virgl or GPU-accelerated;
capture EGL/GL implementation information separately where possible.

Run the same command on the **previous build**, with an identical workload,
for a before/after comparison. A 60-second sample on v0.6.3 alone **cannot**
recover a historical v0.6.1 baseline. Collect at least three runs per version
and retain all JSON files to show variability. Report utilization as a
percentage of **one** (emulated) vCPU, not as a share of the entire VM.

For independent confirmation of idle `wait4` calls, attach briefly with:

```sh
pid=$(pgrep -xo anvil)
sudo timeout -s INT 6s strace -tt -e trace=wait4 -p "$pid" -o /tmp/anvil-wait4.log
grep -c 'wait4(' /tmp/anvil-wait4.log || true
```

A modern kernel with working pidfds should not poll every long-running child
per event-loop iteration. The fallback reaper on older kernels may still
check children at a bounded interval. Tracing significantly perturbs timing,
especially under QEMU TCG. Context switches are not identical to compositor
timer wakeups, and syscall wall times cannot be treated as CPU percentages.

## What still needs evidence

- Exact before/after versions, hashes, VM/GPU acceleration status, bar and
  child-process configuration, and CPU/context-switch measurements.
- Responsiveness under real work (input, redraw, KMS VBlank, page-flip retry,
  lock/unlock, hotplug, XWayland when enabled).
- A native GPU comparison if possible, to separate VM emulation from
  compositor overhead. See [direct hardware validation](hardware-validation.md).

Nested CI measures idle CPU and checks for excessive idle *frames* and leaks
(`tests/protocols/validation.py`), but cannot establish direct DRM CPU
performance inside the actual QEMU TCG guest. Keep #63 open until evidence
satisfies its acceptance criteria.
