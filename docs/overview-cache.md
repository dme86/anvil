# Cached overview layout and chrome

The overview separates three kinds of work:

- Window title/application metadata is retained on managed windows and updated
  by the existing native Wayland and X11 property callbacks. Native title/app-id
  updates now also request repaint, including changes without a buffer commit.
- A shared overview snapshot caches grouping, sorting, pagination and card
  geometry. Its layout inputs include dimensions, ordered monitor names and
  original candidate order/identity, owning monitor, application ID and complete
  tag mask. Sorting precomputes monitor rank and normalized application once per
  entry rather than allocating lowercase strings in every comparison.
- The renderer retains the label/chrome buffer while current-page visual inputs
  match. Its key includes dimensions, page/page count, empty state, headings,
  card/preview geometry, selection, titles, applications, outputs and tags. The
  key owns only visual data, never Window objects or client buffers.

Unchanged snapshots return the same Arc without rebuilding layout or cloning
all titles. Title and selection updates modify the snapshot without sorting or
layout calculation; existing Arc readers keep their immutable older view.
Selection fallback remains the first original candidate before grouping, and
selection still determines the visible page. This preserves multi-tag labels,
monitor/application sections, paging and keyboard/mouse activation.

Live preview elements are generated on every overview frame from each Window's
current buffer and geometry. Client damage/content is deliberately absent from
the layout and chrome keys. Animated/repainting clients and aspect-ratio changes
therefore continue to update while unchanged labels are reused.

Close, unmap/destruction, dead-handle pruning, final-output removal and lock
boundaries release the applicable state cache; XWayland disconnect also clears
it. This avoids retaining dead client Window handles while a VT is paused.
Metadata retained on live managed windows remains available for reopening.
Writing another scene into a shared renderer invalidates its chrome/bar keys.

## Regression checks

Run the normal Rust tests and all-feature Clippy, plus minimal and overview-only
feature checks. After building `tests/protocols/build.sh`, a writable
`XDG_RUNTIME_DIR` permits this explicit real-client test:

```sh
cargo test --all-features overview_reuses_layout_and_snapshot_without_freezing_window_state -- --ignored
```

A test-only layout counter establishes one build across 1,000 unchanged queries.
A buffer-only commit retains the snapshot; titles and selection update without
layout work. Tags, owner/output order and dimensions invalidate layout. Tests
also cover original-order fallback, immutable older Arc readers, unmap/remap,
selected-client death, immediate cache release and reopening. Chrome-key tests
cover visible text/selection/geometry/page changes, unchanged hidden-page labels
and invalidation when another scene overwrites its buffer.

Validated in the Gentoo ARM64 VM with 107 normal Rust tests, the explicit
real-client regression above, all-feature Clippy, minimal/overview-only feature
checks and an optimized release build. Native DRM and nested Winit overview
checks verified live red-to-green buffer updates, title/application changes,
new windows, keyboard activation, pagination, selected-window removal, the empty
state and repopulation. The native protocol suite and 100-pair window lifecycle
stress run also passed.

Source validation still scans the managed-window descriptors to detect changes;
this is not a constant-time overview claim or a promise of a particular FPS/CPU
improvement. It removes repeated metadata reads, sorting, layout, text measuring
and glyph-rectangle construction for unchanged overview frames.
