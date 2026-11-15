# Memory use and v0.7.0 validation

Approximately 100 MiB RSS is the observed one-window baseline of this Gentoo ARM64
QEMU setup, including Anvil's UI fonts, mapped surfaces and llvmpipe userspace
driver resources. It is not a fixed budget: output resolution/count, window
buffers, glyph working set, driver and allocator behavior affect memory use.
RSS counts resident shared mappings as well; PSS apportions shared pages.

## Changes retained for v0.7.0

- Font-file bytes are weakly cached independently of the collection face index.
  Multiple faces of one TTC/OTC share one immutable file allocation, while keeping
  independent metrics/character coverage and normal file-change invalidation.
- Per-face Unicode indices use exact-size sorted arrays. Existing glyph masks
  remain shared and bounded, preserving repeated-text rendering behavior.
- Capture-only passes explicitly drain renderer resource cleanup after their
  temporary framebuffer, renderbuffer and readback mapping have been dropped.
  This frees transient GPU resources even when no later scanout is scheduled.

No font, layout, overview, launcher, bar, output or capture feature was removed.

## Matching before/after samples

Both binaries used `--release --all-features`, one 1280x800 native DRM output,
Hack Nerd Font Mono, one quiet test window, no status scripts, and
`GLIBC_TUNABLES=glibc.malloc.hugetlb=0`. Values below are process RSS from
`/proc/PID/smaps_rollup`, read after each phase settled.

| Phase | Before (KiB) | v0.7.0 (KiB) |
| --- | ---: | ---: |
| Quiet window | 104580 | 104368 |
| After one desktop screenshot, idle | 112652 | 108564 |
| Overview visible after capture | 120756 | 120660 |
| Overview closed | 112880 | 112788 |
| After three reopen/capture/close cycles | 116884 | 112776–120788 |

The lower first-capture sample does not imply a uniformly lower RSS after every
workload. The last row includes the range from two new-binary runs and illustrates
allocator/driver retention and variation;
freeing owned resources does not guarantee immediately returning all heap pages
to the OS. No global allocator tuning or forced heap trimming was introduced.
Collection-byte sharing has its largest benefit when multiple faces of a large
collection are actually used; this single-font desktop shows only a modest idle
difference.

A separate matching 20-cycle run, starting directly with the overview/capture
workload, settled at 116832 KiB before versus 112788 KiB with v0.7.0. The new
process stayed at that value at cycles 5, 10, 15 and 20, with no render failures.
This supports the targeted transient-resource saving without claiming that every
sequence produces exactly the same resident heap size.

An audit also tested immediately destroying the hidden overview's CPU/GPU scene.
The first close used less memory, but repeated reopening increased allocation
churn and did not produce a reliable steady RSS win. That change was omitted;
the warm overview cache is retained. Twenty reopen/capture/close cycles showed
bounded rather than monotonic growth in the comparison. Pixels were identical
after reopening and between the old and new font implementations.

Rust regressions compare every compact character mapping with FreeType, check
shared collection-file ownership and replacement invalidation, and exercise
fallback/measurement/rasterization behavior. The release also runs real native
and nested capture/protocol checks and the full GitHub CI/footprint matrix.
