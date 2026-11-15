# Multiple outputs and concurrent captures

Capture work is scoped to an output and a single compositor render pass. Modern
image-copy sessions and legacy screencopy requests with the same cursor variant
share one offscreen render and GPU readback. Full-output and cropped requests
copy their own rows into their own SHM buffers. When the scene contains no cursor,
both cursor options share the same readback; otherwise there are at most two.
The variants are processed sequentially, without retaining an extra full-output
CPU image. Cropping and vertical normalization read directly from mapped pixels.

No pixels are cached across outputs or later frames. Client-buffer validation,
individual completion/failure events, cancellation, resize constraints and
session-lock isolation still apply to every request. Dead frame resources are
discarded before rendering.

A capture request wakes the renderer without incrementing the desktop scene
generation. Native DRM only visits outputs with scene changes or pending capture;
unrelated GPU renderers are not acquired. A capture-only pass does not submit an
unchanged scanout or send another client frame callback. Existing per-output
refresh pacing and KMS completion wakeups also apply to captures. Desktop changes
continue to use normal repaint invalidation. The nested backend now builds its
desktop scene once for both capture and display, temporarily adding its synthetic
capture cursor only for readback.

`anvilctl debug stats` includes cumulative `capture_readbacks` and `capture_frames`
counters. The first counts successful GPU mappings; the second counts completed
client images. These additive fields default to zero when a new CLI reads an
older server's response.

## Verification

`tests/protocols/build.sh` builds `capture-batch`, and the normal protocol CI runs
it on layer-shell, default and all-feature builds. The default/all-feature jobs
also check that two rounds of eight requests per output require at most four
readbacks per output. The fixture checks independent modern sessions, modern and
legacy cursor variants, full images, exact crops, padded SHM stride/offset guards,
a canceled request and fresh pixels after a content change.

Native VM validation used three separate virtio GPUs at 1280x800/60 Hz,
1024x768/75 Hz and 800x600/75 Hz. The two batches completed 48 images using eight
readbacks, with no render failures. A separate screenshot completed with one
readback and zero additional scanout render attempts/submissions. The full native
protocol suite, 100 window pairs, and nested capture/protocol checks also passed.
These are shared-work counts, not a claim of a particular CPU or FPS improvement.
