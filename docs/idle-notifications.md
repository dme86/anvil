# Idle notification timer reuse

Each idle notification records its own latest activity time and retains at most
one armed Calloop timer. Matching seat input updates that timestamp and resumes
an idle listener immediately. An already armed source is kept; the input path
does not remove/reinsert it or clone the listener vector.

When a timer fires it checks the current activity deadline. If input has moved
that deadline into the future, `TimeoutAction::ToInstant` reschedules the same
source/token using Calloop's reusable timer-wheel entry. If the deadline has
arrived, it emits one `idled` event and drops the timer. New input emits `resumed`
and arms the next timeout. Zero timeout follows the same one-shot state machine
instead of spinning without input. A failed registration can be retried on later
activity.

Notification creation starts its own timeout; it does not inherit older seat
activity. Activity affects only listeners for the matching seat. Ordinary idle
notifications stop during visible application inhibition and restart with a
full timeout after inhibition ends. An inhibitor becoming active resumes an
already-idle ordinary listener. Input-idle notifications ignore those transitions
and keep their independent timers/activity times. Listener destruction cancels
its outstanding source; idle/dead/inhibited callbacks clear the stored token
when dropping rather than leaving a stale registration handle.

## Regression checks

Build the protocol client with `tests/protocols/build.sh`, then use a writable
`XDG_RUNTIME_DIR` for the explicit server-side test:

```sh
cargo test --all-features input_activity_keeps_idle_timer_registrations -- --ignored
```

The test uses two real protocol listeners and test-only registration/cancellation
counters. After their initial two registrations, 10,000 activity notifications
create zero additional sources and cancel zero sources. It also checks that an
early callback retains its token and follows the latest exact deadline, a second
seat cannot reset these listeners, inhibitors leave input-idle timestamps and
sources untouched, and explicit destruction/disconnect cancels pending sources.
Counters are absent from production builds.

For external timing/ordering tests, run:

```sh
target/protocol-tests/client layer-shell idle-watch
```

The fixture accepts commands `add ID normal|input TIMEOUT_MS`, `del ID`, `inhibit`,
`uninhibit`, `destroy-window`, `new-window`, `sync` and `quit`. It prints monotonic
`IDLED`/`RESUMED` events and `ACK` timestamps and rejects duplicate or out-of-order
notification transitions. Use `default` instead of `layer-shell` for a minimal
build. Run it in a dedicated quiet session, inject real seat activity and verify
that idling follows the latest input, including input after the original timer
deadline. Check multiple timeouts, zero timeout, inhibitors, tag hiding/showing
and destruction while a source is pending.

This removes per-input source churn. Notification/seat checks and timestamp
updates still scale with listener count; no general CPU percentage improvement
is claimed by the registration counter test.

The native ARM64 VM timing checks passed for latest-input reset, immediate
resume, independent normal/input listeners, a continuous input stream, zero
timeout, inhibitor toggle/hide/destruction and listener destruction/disconnect.
Native and nested lifecycle/capture/lock suites plus 100 window pairs passed
without render failures; the native output returned to zero idle render attempts.
