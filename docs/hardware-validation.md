# Direct DRM/KMS validation

This checklist is the reproducible manual part of issue [#19](https://github.com/dme86/anvil/issues/19).
Nested CI does not establish physical GPU, display, seat, VT or suspend behavior. No physical
system has been validated by adding this document: the issue remains open until a completed
report includes at least one physical DRM/KMS system and the required scenarios below.

## Prepare and record the environment

Use a normal logged-in user's local TTY, outside another compositor. Build the exact commit
being tested and retain its configuration. For the compatibility pass, build with all features
and install the external applications below; also repeat basic startup with the minimal build.

```sh
git rev-parse HEAD
cargo build --release --all-features --locked
uname -a
cat /etc/os-release
lspci -nnk | sed -n '/VGA\|3D\|Display/,+3p'
loginctl session-status
ls -l /dev/dri
```

Record the Rust toolchain, Mesa/proprietary driver version (from the distro package manager),
libseat provider (logind or seatd), display models/connectors, resolutions/refresh rates,
scale/transform settings, input devices, XWayland version and desktop application versions.
If available, save `eglinfo` and `drm_info` output. A virtual GPU is a separate useful test
environment, but does not count as the required physical system.

Use `RUST_LOG=anvil=debug target/release/anvil --config /absolute/path/config.toml
>anvil-hardware.log 2>&1` directly from the TTY. For the X11 pass set `[compat] xwayland = true`.
Run diagnostics in a terminal inside Anvil with `target/release/anvilctl debug stats`; collect
snapshots before and after each scenario. Keep logs from external clients as well.

## Validation pass

Run each applicable row, recording PASS/FAIL/NOT TESTED/NOT AVAILABLE, actual results and
evidence filenames. A missing device or application is not a passing test. Exercise both Intel
and AMD where available; record NVIDIA separately, including proprietary versus Mesa drivers.

| Scenario | Procedure and expected behavior |
| --- | --- |
| Direct startup (required) | Start from a local TTY without a host compositor. A native Wayland terminal appears; keyboard and mouse work; no seat or KMS initialization error. Repeat with `--no-default-features`. |
| Seat ownership | Record the libseat provider. Confirm the user session owns input/display access and another inactive VT cannot receive Anvil's input. |
| Native applications (required) | Open a terminal and browser, type, scroll, resize/retile, change focus/tags and close them. Content updates and focus remain correct. |
| Single monitor | Start with one display; verify tiling, floating, fullscreen, pointer edges and refresh. |
| Multiple monitors | Start with two displays; move/focus windows across outputs with configured positions. Each output's tags/layout remain independent and windows stay reachable. |
| Display hotplug (required) | Add, remove and reconnect a display with tiled, floating and fullscreen windows present. Removed-output windows remain accessible; reconnect does not crash or duplicate outputs. |
| Modes and scaling | Try supported resolutions/refresh rates, 1.0/1.25/1.5/1.75/2.0 scales and a rotated output. Compare advertised mode, client sizes, pointer hit testing and capture dimensions. |
| Input and hotplug | Exercise keyboard layout/repeat, mouse buttons/wheel and touchpad scrolling/tapping. Unplug/reconnect each available device while applications remain open. |
| VT switching (required) | Switch away with Ctrl+Alt+Fn, then back repeatedly. The inactive session releases input/display ownership; returning restores the desktop and focus. |
| Lock/unlock (required) | Run `swaylock` or another ext-session-lock client. Verify every display hides the desktop and accepts only locker input. Unlock restores existing windows/focus. |
| Locked hotplug | Add/remove a display while locked. New displays must never show desktop contents; lock confirmation and subsequent unlock remain correct. |
| Locker crash | Terminate the locker from another TTY. The desktop must remain inaccessible, with no input leak; record the recovery procedure and logs. A crashed locker must never silently unlock the session. |
| Suspend/resume (required) | Suspend and resume from the user session, first unlocked, then locked. Repeat with an external monitor. Restore displays/input without revealing the locked desktop. |
| XWayland (required when enabled) | Open `xmessage` and a representative X11 application with `compat.xwayland = true`. Verify window list, focus, tags, resize and shutdown alongside native clients. Record server-failure behavior separately. |
| External layer-shell bar (required when enabled) | Start Waybar with a basic clock module. Verify placement, exclusive space, output removal, bar exit and lock isolation. Run a layer-shell-only build as well. |
| Workspace integration | Test the external workspace tool and protocol, if implemented/installed. Record unavailable protocols explicitly rather than counting a clock-only Waybar as workspace validation. |
| Clipboard/primary selection | Use `wl-copy`/`wl-paste` and their `--primary` variants; verify independent contents, ownership replacement and owner exit. |
| Screen capture | Use `grim` for every output and a crop; compare actual content, scale and dimensions. While locked, capture must be rejected. Portal screen sharing is a separate capability, not established by a screenshot. |
| GPU-backed clients | Open a known accelerated Wayland application. Record renderer/driver information and DMA-BUF diagnostics; verify redraw, resize and exit without import errors where supported. SHM-only clients do not prove DMA-BUF operation. |
| Fullscreen/games | Enter/leave fullscreen repeatedly, alt-tab/focus, switch tags and remove the fullscreen output. For a representative game test relative mouse input and pointer capture/release. |
| Recovery/config reload | Keep clients open, replace the config with invalid TOML and run `anvilctl reload`. It must report failure while retaining the working configuration. Restore the file, reload and verify normal use. |
| Resource cleanup | Save stats with clients running and after exit; compare client/window/fd counts. Record workload duration and idle CPU observations; retained driver caches are not automatically a compositor leak. |

## Report template

Copy this section into a new dated report or issue comment. Keep separate reports for different
GPU/driver combinations. Attach logs/configuration with private application contents removed.

```text
Date / tester:
Commit / Rust toolchain / build features:
Physical machine / GPU / kernel driver:
Distro / kernel / Mesa or proprietary driver version:
Seat provider / launch method:
Displays / connectors / modes / refresh / positions / scales / transforms:
Input devices:
External client versions:
Configuration and log filenames:

Scenario | result | observed behavior | evidence
(one row per checklist scenario; required rows must have actual results)

Duration / repetitions:
Known limitations and reproduction steps:
Follow-up issue links:
Baseline and final diagnostics:
```

Before closing #19, verify direct physical startup, display hotplug, VT switching,
suspend/resume, locking and native applications have passed, plus XWayland and layer-shell
passes when enabled. Record GPU/driver details and unresolved hardware limitations. Repeat
this pass for a stable 1.0 candidate; automated nested results alone cannot satisfy it.
