# Incremental window placement

An arrange pass still computes the desired visible layout. Tiling changes can
legitimately resize every tile, so this is not a claim of constant-time layout.
Applying the result no longer tears down and rebuilds the whole Smithay Space:

- Unmap only elements absent from the desired visible set (including dead or
  unbuffered windows). A tag change still removes the windows it actually hides.
- Map newly visible windows and move existing elements only when their position
  changes. Smithay retains output membership during a move; its normal Space
  refresh handles real output enter/leave transitions.
- Keep each window's last requested geometry separately from its current client
  buffer. Only changed sizes enter the Wayland size negotiation path; moving a
  window does not ask it or its neighbours to redraw. X11 configure requests are
  similarly limited to changed geometry, with failed requests eligible for retry.
- Preserve the surviving stacking order and keep the focused visible window on
  top. Moving an element in Space can raise it implicitly; a linear subsequence
  scan identifies the suffix that needs explicit raising. An unchanged stack
  needs no raises, and reordering does not change activation of other clients.
- Compute tiling rectangles only in tiling mode, not in floating/monocle mode.

Client null-buffer unmapping invalidates the cached layout geometry. Remapping,
output moves, scale changes, delayed buffers and close-resize covers retain their
existing lifecycle handling. Fractional-scale and idle-inhibitor updates remain
in place; this change does not cache those policies.

## Regression checks

On Linux, build the protocol client with `tests/protocols/build.sh`. In a dedicated,
quiet test session with the normal IPC-enabled Anvil build:

```sh
ANVILCTL=/usr/local/bin/anvilctl target/protocol-tests/client layer-shell layout-noop
```

This creates twelve real Wayland windows and reloads the unchanged configuration
twenty times. It fails if any window receives another configure or output
leave/enter event after initial setup. `layout-watch` instead leaves twelve
windows running and prints `CONFIG`, `ENTER` and `LEAVE` events for external
input tests. Use the matching `default` argument for a build without layer-shell.

Also run the full protocol suite and `layer-shell stress 100` to exercise delayed
first images, close/unmap coverage, focus, popups and actual capture pixels.
The explicit `logical_output_recovery` Rust test covers output migration and
zero-output recovery. Unit tests exercise all 576 combinations of current and
desired stacking order for four overlapping windows.

## VM comparison (2026-11-15)

Compared with commit `45e4396` using the same ARM64 VM and twelve test windows:

| Operation | Previous output leave/enter events | Incremental placement |
| --- | ---: | ---: |
| 20 unchanged config reloads | 240 leaves + 240 enters | 0 |
| ~9 seconds dragging one floating window | 1,212 leaves + 1,212 enters | 0 |
| Hide and restore the tag | 12 leaves + 12 enters | 12 leaves + 12 enters |

No unnecessary configure events occurred in the unchanged-layout or pure-move
checks. These counts demonstrate removed protocol churn, not a guaranteed FPS
or CPU improvement on every renderer. XWayland is compiled and linted with all
features; this VM comparison uses native Wayland clients.
