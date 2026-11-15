# Direct-backend frame pacing

The DRM backend limits render starts to each output's current advertised refresh
rate (including fractional rates such as 59.995 Hz). KMS page-flip completion
still prevents reuse of an in-flight buffer. A separate monotonic deadline is
needed because virtual DRM drivers can acknowledge flips earlier than that rate.
There is no user-configured global 60-FPS cap; 144-Hz outputs use their own period.
An unavailable or invalid refresh value falls back to 60 Hz.

Input dispatch, client pointer events, hit testing and window movement still run
for every input event. Only scene rendering is deferred. Further input replaces
the pending scene with its latest state. One shared, cancellable, one-shot timer
wakes the compositor at the earliest eligible output deadline. The final pending
frame is therefore rendered even after input stops. No periodic repaint timer
runs at idle. After a quiet interval the first damaged frame can render immediately.

A damage generation distinguishes actual scene changes from scheduler wakeups.
Each output consumes a generation only after successful rendering/submission
(including an unchanged scene with no submission). A fast output which has
already consumed it does not render again just because a slower output is still
waiting. Resume and mode changes reset pacing; hotplugged outputs start ready.
GPU errors retain pending damage and keep the bounded delayed retry.

## Validation

Unit tests cover per-output generation consumption, pending newer damage,
60/144-Hz deadlines and invalid refresh fallback. Run `cargo test --all-features`
and `cargo clippy --all-features --all-targets -- -D warnings` on Linux.

For a runtime regression check, use a quiet native DRM session and inject a
relative pointer stream through Linux uinput substantially faster than the
output refresh (for example 4,000 events with a nominal 1-ms delay). Record
`anvilctl debug stats` immediately before and after the stream and after a short
settling interval. Compare counter deltas rather than lifetime averages:

- All input may request repaint, but submitted frames should stay near the output
  refresh times elapsed time, with a single boundary frame of tolerance.
- The final pending frame must complete after the stream stops; subsequent quiet
  samples must show no further render attempts or submissions.
- Repeat over a client surface, while dragging, after VT pause/resume, and with
  window open/close, resize, popup, capture and session-lock protocol tests.
- On mixed-refresh hardware, verify that each output progresses independently and
  that stopping input returns all outputs to idle. Synthetic unit tests do not
  replace this hardware check.

CPU percentages refer to one core. In a VM using llvmpipe, renderer worker CPU
must be distinguished from the compositor main thread. Submitted frames are not
proof of physical presentation rate or measured input-to-display latency.

## VM check (2026-11-15)

ARM64 QEMU, one 1280x800@59.995 output, llvmpipe, static bar, isolated VT:

| Workload | Submitted frames/s | Process CPU, one-core units |
| --- | ---: | ---: |
| 4,000 pointer events, nominal 1-ms spacing, empty desktop | 59.87 | 2.46% |
| Same stream over a floating rst window | 59.55 | 6.36% |
| Dragging, nominal 60 input events/s (actual ~54/s) | 53.87 | 68.64% |

The previous build submitted 385 and 269 frames/s for the first two workloads.
Its injected streams took longer under load; these are observed VM samples,
not a controlled CPU speedup claim or a hardware latency benchmark. The key
regression criterion is that submission stays within the advertised refresh.
Two batches of 100 window pairs (400 toplevels total), with resize/fullscreen/
popup activity, completed without render errors and returned to zero idle
render attempts. Full protocol tests also passed, including actual capture
pixels, delayed buffers and lock isolation. Mixed-refresh physical hardware
remains untested.
