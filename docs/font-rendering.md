# UI fonts

The bar, its click targets, launcher and window overview share font bytes and
compact character/line metadata. FreeType loads outlines only when a glyph
is actually rasterized; unused Nerd Font symbols are not expanded at startup.
A cache identifies a face by its canonical file path, collection index, file size
and modification time. Different Fontconfig patterns that resolve to the same
face reuse the font data. FreeType handles stay thread-local and short-lived;
only immutable font metadata and bounded glyph masks are shared. Weak references allow unused faces to be released.

Missing characters use the configured pattern's Fontconfig fallback order.
Fallback coverage is resolved once per shared UI font, and glyphs are loaded
only when a character needs them. Measurement and rasterization select the same
face. Rasterized glyph masks are also shared across UI consumers and reused at the
same character and pixel size. Their cache is limited to 1024 entries and 4 MiB
of bitmap data; oversized glyphs are rendered without being retained.
Character lookup caches are bounded, including failed lookups, so arbitrary
window titles cannot grow that cache indefinitely.

This works with regular monospace and proportional text fonts, patched Nerd
Fonts, and separate symbol fonts. Install fonts containing the characters you
need; Anvil does not download fonts. Font collections honor Fontconfig's face
index. Variable-font named instances use their base face; arbitrary variation
coordinates, complex shaping and color-emoji rendering are not implemented.
Presentation selectors and joiners have no standalone ink or advance rather
than appearing as missing-glyph boxes. This does not provide emoji shaping.

## Validation

```sh
cargo test --all-features --locked
cargo clippy --all-features --all-targets --locked -- -D warnings
```

Regression tests cover shared ownership, matching glyph measurement/rendering,
missing glyphs, invisible presentation characters, separate faces in a generated
TTC, and cache invalidation after replacing a font file.

Manual tests on the ARM64 Gentoo QEMU VM covered Hack, DejaVu Sans Mono, DejaVu
Sans, DejaVu Serif, Hack Nerd Font Mono and Symbols Nerd Font Mono. Latin,
accented Latin, Greek, Cyrillic, a sun symbol and a Nerd Font symbol were checked.

With Hack Nerd Font Mono, an isolated four-consumer test used approximately
53 MiB RSS after the change versus 209 MiB when parsing four independent fonts.
A separate before/after nested-compositor smoke test with the same empty desktop
configuration measured approximately 305 MiB versus 197 MiB RSS, with no render
failures. These are observations from this VM, not guaranteed hardware figures
or a measurement of the existing live desktop session. The change does not
replace the VM's llvmpipe software renderer or guarantee a higher frame rate.

A repeated-text microbenchmark on the VM measured approximately 7.4 ms without
bitmap reuse versus 0.32 ms with a warm glyph cache (1000 repeats of the same
short status text). This measures only glyph rasterization, not full-frame
rendering or input latency. The VM's llvmpipe workers remain the dominant CPU
cost during window movement.

## Lazy-outline memory measurements

The FreeType-backed implementation reduces the six-family font probe to about
3–6 MiB RSS, including regular Hack and DejaVu fonts, fully patched Nerd Fonts,
and a separate Symbols Nerd Font. The same four-consumer Hack Nerd Font Mono
probe previously required about 53 MiB after sharing Fontdue faces.

In a matching empty nested-compositor test on the ARM64 Gentoo VM, RSS fell from
approximately 198 MiB to 151 MiB. Setting the VM session's
`GLIBC_TUNABLES=glibc.malloc.hugetlb=0` reduced that run to approximately 103 MiB.
The kernel was configured for `madvise` transparent huge pages. The setting
avoids glibc's default AArch64 huge-page advice; it is VM configuration, not a
universal compositor setting. Limiting allocator arenas further did not improve
this sample, so it was not adopted. See the
[glibc allocator documentation](https://sourceware.org/glibc/manual/latest/html_node/Memory-Allocation-Tunables.html).

A direct-DRM test with one temporary terminal used approximately 101 MiB,
completed the synthetic motion test at 54–58 frames/s, and had no render errors.
Frame rates remain input/VM dependent; this is not evidence of a CPU or latency
improvement. The use of FreeType changes antialiasing slightly, not the selected
font or the supported text/icon fallback chain.
