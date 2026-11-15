# Event-local pointer hit reuse

Native relative and nested/absolute motion share one dispatcher. For client-directed
motion, raw relative events are delivered before applying cursor confinement or
lock policy, without a new rate limit. Overview continues to own its pointer input.

A two-entry lazy cache records the hit (including no surface) at the current
and proposed coordinates for the duration of that event only. Equal coordinates
share an entry. Relative-pointer origin validation, active-constraint validation
and final pointer focus can reuse those hits. Without an active constraint,
constraint handling does not request an origin hit at all.

All surface-tree traversal runs outside Smithay's constraint callback. That
callback holds the surface private-data mutex as well as constraint data;
re-entering the surface tree there can deadlock. First inspect only active
constraint kind, perform the required hits, then borrow the confinement region
in a short callback that only evaluates coordinates. This avoids copying client
regions. A lock still validates the matching current hit: stale focus on a hidden
surface must not freeze the cursor before it can leave that surface.

A move/resize gesture (including its full-layout fallback) invalidates both
entries before final focus lookup, because placement, stacking and surface-local
origins may have changed. Nothing is cached across client commits or later input
events. Popup/subsurface origins therefore remain governed by the same Smithay
surface traversal as before. Explicit confinement regions retain their existing
replace-surface-bounds semantics, including region holes.

## Checks

`tests/protocols/build.sh` builds quiet, locked and confined persistent client
fixtures. With a writable `XDG_RUNTIME_DIR`, run the real-client counter test:

```sh
cargo test --all-features motion_hits_are_shared_only_within_an_unchanged_scene -- --ignored
```

The test-only counter measures actual `surface_under` traversals. Normal relative
motion over a focused surface uses two traversals (previously three); ordinary
absolute motion uses one (previously two). Equal positions and cached misses
reuse one result, locked motion uses one, and confinement uses at most current
and proposed hits. A confined floating drag deliberately requires a fresh third
hit after geometry mutation. Tests also check hidden stale locks, absolute lock
motion, rejected confinement and unchanged explicit-region behavior.

Run normal Rust tests/Clippy and the native/nested protocol suites too. The
counter reduction establishes removed work; it does not quantify a general
CPU, FPS or input-to-display latency improvement.

Native ARM64 VM checks also passed for continuing raw motion under a lock,
accepted/rejected confinement, popup-local origins, confined floating drag and
hidden locked surfaces. All move/resize variants were exercised with twelve
windows. Native/nested capture, lifecycle and lock suites and 100 window pairs
passed without render errors; the native session returned to zero idle render
attempts. These runtime checks use native Wayland clients; the optional XWayland
build is covered by all-features compilation and linting.
