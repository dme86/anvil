# Keeping transport and helper I/O off the compositor loop

## Control socket

Accepted IPC streams are nonblocking Calloop sources. Reading preserves partial
JSON fragments until newline or EOF, with the existing 64 KiB request limit.
A dispatch handles at most 64 KiB of socket input/output; the listener accepts at
most sixteen peers per dispatch. Up to sixty-four connections can remain live.
Incomplete readers expire after 100 ms, and a response has a two-second write
deadline. Expired/excess peers are closed instead of waiting on them.

Response writes resume at their previous offset when the peer has space.
Writable interest is enabled only after a response exists, preventing writable
sockets from spinning while waiting for requests. One deadline source follows
the connection's current phase; completion/error/timeout removes both resources.
Window lists remain complete rather than being truncated to an arbitrary size.

Command dispatch still executes on the compositor thread because it mutates
compositor state. This change removes transport waits; explicit configuration
reload/backend application and process-launch operations are not moved into
threads.

## Network icon

The optional NetworkManager icon uses one outstanding worker, shared across
outputs and config generations. Results return through a channel and Calloop
ping. The last icon remains visible while a query is pending; failures retain
the existing offline fallback. Disabled widgets start no query. Obsolete results
are rejected, and repeated reloads cannot overlap workers.

A query uses the existing GNU coreutils `timeout` dependency: two seconds plus
one second kill grace. Output is capped at 64 KiB; overflow/read errors kill the
owned process group and reap it in the worker. No compositor callback reads
child stdout, waits on the child, or joins a worker.

## Launcher index

Startup and opening the launcher no longer scan application files synchronously.
The launcher opens from its cached index, while one worker reads/parses/sorts
regular desktop files (including symlinks to regular files). Opening requests a
refresh, so new installations and removals remain discoverable without restart.
Repeated openings coalesce into one pending follow-up scan.

Completions update the current query's matches without discarding typed input.
Selection follows the application's desktop-file identity where it remains in
the result list. Closed launchers receive a refreshed cache without a repaint.
Workers own only scan data and report through a channel/ping; there are no drop
joins. Directory/file I/O can still wait on a slow filesystem inside that one
worker. A stalled scan does not stall input or start an unbounded set of threads.

Background workers are installed after the backend establishes the environment
used for child applications, for both native and nested sessions.

## Validation

Rust tests exercise nonblocking fragments/EOF/oversized input, partial response
writes with a stalled reader, network timeout/output limits/single-flight stale
results, and delayed launcher scans preserving cached results/query/selection.
All-feature Clippy and minimal, launcher-only and bar-only checks pass.

In a native ARM64 QEMU VM, eight incomplete peers were held while healthy IPC
queries completed in at most 0.14 ms in one sample. During an intentionally
1.2-second fake NetworkManager query the maximum sampled healthy query took
0.18 ms. These are observed local VM samples, not a general latency guarantee.
A repeat while compiling concurrently reached 33.03 ms maximum (0.22 ms median)
with stalled peers, illustrating host-load variability. The launcher was tested
with 2,000 desktop files and a newly installed marker
application; typed query/argv launch worked and deleting the application removed
it after refresh. Three batches of twenty expired peers returned to the same
open-FD count (44). Socket deadlines, not render polling, clean these peers up.

The actual Calloop source is tested through READ-to-WRITE reregistration with a
2 MB response and stalled peer: an independent heartbeat still fires, writable
callbacks stop when the socket fills, and resuming the reader receives the full
ordered payload. All 104 normal Rust tests and all-feature Clippy passed. Native
and nested lifecycle/capture/lock suites, idle-notification timing and 100 window
pairs passed without render failures; the native output returned to idle.
