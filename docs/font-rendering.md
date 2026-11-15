# UI fonts

The bar, its click targets, launcher and window overview share parsed font faces.
A cache identifies a face by its canonical file path, collection index, file size
and modification time. Different Fontconfig patterns that resolve to the same
face reuse the outlines. Weak references allow unused faces to be released.

Missing characters use the configured pattern's Fontconfig fallback order.
Fallback coverage is resolved once per shared UI font, and outlines are loaded
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
