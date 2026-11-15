# Reusing unchanged rendering

Each output's bar renderer retains the input description of its last completed
bar buffer: width, bar configuration and the full visual snapshot. If these
match, it returns another render element for the same buffer without rebuilding
bar layout, measuring text, constructing glyph rectangles or painting pixels.
The existing rectangle comparison remains useful when changed inputs happen to
produce the same pixels.

The snapshot covers output focus, monitor count, tags, membership counts, layout
symbol, ordered window titles and focus, status, network state and launcher
query/results/selection. Appearance and width changes invalidate the cached
result. Snapshot collection and equality checks still read current state; this
is intentionally not a separate global dirty-bit model. A renderer reused for
an overview/launcher scene invalidates its bar key when writing that scene.
A failed GPU import can retry the same completed buffer.

Window resize previews now produce the backend's final render-element iterator
directly. Ordinary windows retain their original elements; only surfaces with
an actual active preview receive a scaling wrapper. This removes the temporary
transformed-element vector and identity scaling wrappers. Preview discovery
still inspects managed windows. Output capture and nested rendering use the
same path, including subsurfaces/popups and session-lock isolation.

## Validation

Run `cargo test --all-features`, Clippy with all targets/features and the minimal
feature check on Linux. Cache tests cover changes in visual content/appearance
and invalidation when another scene overwrites the buffer. GPU integration is
checked with the native and nested protocol suites, including actual capture
pixels for delayed images and close-resize covers.

In the ARM64 QEMU VM (1280x800@59.995, llvmpipe), 4,000 nominal 1-ms mouse events
submitted 267 frames over approximately 4.6 seconds, without render errors.
Captured bar pixels remained identical throughout pointer movement. Subsequent
status, background-color and window-title changes changed the captured bar
pixels as expected. Native and nested lifecycle/capture/lock tests and 100
window-pair stress iterations passed; the native output returned to idle with
no additional render attempts. These checks establish correct reuse and
invalidation, not a measured general CPU or GPU speedup.
