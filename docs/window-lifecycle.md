# Window image readiness

Wayland toplevel objects are tracked immediately but do not occupy a tile or
receive focus until their first buffer commits. Initial configure sizes are
planned with the same layout code as mapped windows, without resizing existing
clients while a new application is still starting.

When a window closes or unmaps, the surviving clients receive their new tile
sizes immediately. Until a survivor commits a new buffer acknowledging that
configure (or a later one), its existing textures are scaled into the assigned
tile. Text can briefly stretch while the client redraws. This avoids exposing
the wallpaper without copying a screenshot, retaining the closed client's
content, freezing unrelated windows, or adding a timeout/polling loop.

The cover uses Smithay render-element transforms and applies consistently to
DRM, nested rendering and output capture. Layer-shell backgrounds/panels are
not scaled. Focus borders follow the target tile. A later layout change that already fits the existing buffer also clears the cover.
Otherwise the preview ends on a fresh buffer commit, not merely on an acknowledgement or empty commit. Session-lock
rendering continues to exclude desktop surfaces.

`tests/protocols/client.c` tests delayed first buffers, destruction before first
map, and a survivor which deliberately delays its replacement image on close.
The test samples actual capture pixels inside the formerly occupied tile,
including after an acknowledgement followed by an empty commit. The unchanged
image must remain visible until the new buffer is attached.

Window placement is applied incrementally; see [incremental layout](incremental-layout.md)
for visibility, geometry and stacking rules and their regression checks.
