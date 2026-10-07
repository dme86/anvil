<p align="center">
  <img src="anvil.png" alt="anvil" width="420">
</p>

---

<p align="center">
  <a href="https://github.com/dme86/anvil/actions/workflows/ci.yml"><img src="https://github.com/dme86/anvil/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/dme86/anvil/releases/latest"><img src="https://img.shields.io/github/v/release/dme86/anvil?label=release" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/dme86/anvil" alt="License"></a>
  <img src="https://img.shields.io/badge/Rust-stable-orange?logo=rust" alt="Rust">
  <img src="https://img.shields.io/badge/Wayland-native-blue" alt="Wayland">
  <img src="https://img.shields.io/badge/XWayland-optional-lightgrey" alt="XWayland">
</p>

Anvil is a fast, minimal Wayland compositor for people who want a focused, keyboard-driven desktop without the weight of a full desktop environment. Inspired by dwm, it combines a simple master/stack workflow, tags, floating and monocle layouts, multi-monitor support, an optional bar and launcher, and native Wayland rendering in a small, predictable package.

It is designed to work as a practical daily driver rather than a toy compositor: Anvil supports DRM/KMS and libinput directly, GPU-backed `linux-dmabuf` clients, secure session locking through `ext-session-lock-v1`, clipboard and primary selection, configurable input and output handling, window rules, IPC through `anvilctl`, and optional XWayland compatibility for legacy applications. `xdg-activation-v1` provides guarded focus requests. External session tools can monitor activity through `ext-idle-notify-v1`, while visible applications can inhibit idle handling through `zwp_idle_inhibit_manager_v1`; blanking and automatic locking remain outside Anvil. Relative pointer input plus pointer locking and confinement are available to games, remote desktops and other clients that request them.

If you like the philosophy of dwm but want it on a modern Wayland stack, Anvil is built for exactly that.

## Build

```sh
cargo build --release
mkdir -p ~/.config/anvil
cp config.toml ~/.config/anvil/config.toml
sudo install -Dm755 target/release/anvil /usr/local/bin/anvil
sudo install -Dm755 target/release/anvilctl /usr/local/bin/anvilctl
sudo install -Dm644 anvil.desktop /usr/share/wayland-sessions/anvil.desktop
```

### Feature combinations

`bar`, `anvilctl`, `launcher`, `xwayland` and `layer-shell` are independent features. The ordinary build enables
the first three; legacy X11 support is opt-in:

```sh
cargo build --release
```

Choose an exact combination by disabling the defaults first:

```sh
# With bar, without anvilctl
cargo build --release --no-default-features --features bar

# Without bar, with anvilctl
cargo build --release --no-default-features --features anvilctl

# With bar and launcher, without anvilctl
cargo build --release --no-default-features --features bar,launcher

# Without bar, with centered launcher and anvilctl
cargo build --release --no-default-features --features launcher,anvilctl

# Minimal compositor without optional components
cargo build --release --no-default-features

# Default components plus optional XWayland support
cargo build --release --features xwayland

# External bars and panels, without the built-in bar or launcher
cargo build --release --no-default-features --features layer-shell,anvilctl
```

Always install `target/release/anvil`. Install `target/release/anvilctl` as well only when the
`anvilctl` feature was enabled for that build.

Select **Anvil** in a display manager, or launch `anvil` from a free TTY. Use `anvil --nested` to
run it in a window inside an existing graphical session.

### Optional XWayland compatibility

Build with `--features xwayland`, install the `Xwayland` executable, then set
`compat.xwayland = true` in `config.toml`. X11 windows use the same tiling, floating, focus, tag,
monitor and window-rule behavior as native Wayland windows. If XWayland is absent or fails to
start, Anvil logs the failure and keeps the native Wayland session running.

### External bars, panels and backgrounds

Build with `--features layer-shell` to advertise `zwlr_layer_shell_v1`. Without that feature,
the protocol is absent. External clients such as Waybar remain separate programs:

```sh
cargo build --release --no-default-features --features layer-shell,anvilctl
waybar
```

Background, bottom, top and overlay surfaces use their requested output, anchors, margins and
sizes. Exclusive zones reserve space for panels and immediately update tiling when changed or
removed. Anvil's built-in bar reserves its own strip independently; disable the `bar` feature
when replacing it with an external bar. Exclusive keyboard focus is supported on top/overlay
layers; on-demand surfaces receive keyboard focus when clicked, while non-interactive surfaces
receive pointer events without stealing keyboard focus. Disconnected outputs close their layer
surfaces and migrate ordinary windows through the existing output fallback policy. External
surfaces are hidden and cannot receive input while the session is locked.

### Screenshots and output capture

Anvil exposes `ext_output_image_capture_source_manager_v1` and `ext_image_copy_capture_manager_v1`
for modern output capture, plus `zwlr_screencopy_manager_v1` for existing tools such as `grim`:

```sh
grim screenshot.png
grim -o DP-1 monitor.png
grim -g '0,0 800x600' region.png
```

Frames contain the composed desktop, including client surfaces, the built-in bar and enabled
layer-shell panels. Each connected output can be captured individually. Capture uses the active
renderer, so imported DMA-BUF client content is included; the destination buffer currently uses
ARGB8888 shared memory rather than DMA-BUF. No GPU readback occurs without a capture request.
Modern sessions refresh their constraints on output changes and stop on output removal or
session locking. Pending captures are rejected while locked. The standalone cursor-stream and
foreign-toplevel capture extensions are not implemented; output capture provides the protocol
foundation for a future portal backend, but does not itself install a screen-sharing portal.

Capture globals are available to clients in the user's Wayland session, as with other compositors
that support `grim`; there is no in-compositor permission dialog. Remote or untrusted clients
should not be granted access to that socket.

### Reproduce the protocol smoke tests

On Ubuntu install the ordinary build dependencies plus `libwayland-dev`, `libxkbcommon-x11-dev`, `xvfb`, `grim`, `waybar`,
`fonts-dejavu-core`, `ripgrep` and `dbus-daemon`, then run:

```sh
cargo build --all-features --locked
tests/protocols/build.sh
tests/protocols/run.sh all-features
```

The suite starts a nested compositor under Xvfb with Mesa software rendering, creates a real
Wayland window, checks captured pixel colors, rejects an incorrectly sized buffer, reuses a
capture session, checks session-lock isolation and recovery, and exercises layer-shell exclusive
zones and keyboard focus. It also runs `grim` for full/cropped captures and starts a real Waybar.
CI runs minimal, default, layer-shell-only and all-feature builds; disabled layer-shell globals
are checked explicitly. Logs and screenshots are retained as CI artifacts. Physical DRM/KMS
hotplug and hardware-specific DMA-BUF paths still require a physical system; these tests do not
claim that hardware validation.

## Keys

- `Super+Return`: terminal
- `Super+j/k`: focus next/previous window
- `Super+Space`: cycle tiling, monocle and floating modes
- `Super+,/.`: focus previous/next monitor
- `Super+Shift+,/.`: move the focused window to the previous/next monitor
- `Super+p`: application launcher
- `Super+h/l`: resize master area
- `Super+Shift+Return`: move window to master
- `Super+Shift+c`: close window
- `Super+1..9`: select tag
- `Super+Shift+1..9`: move window to tag
- `Super+Shift+q`: quit

## Layouts and floating windows

- **Tiling** uses a dwm-style master/stack layout. `Super+h/l` changes the master width and
  `Super+Shift+Return` promotes the focused window to master.
- **Monocle** gives every visible window the complete usable display area. Window focus can still
  be cycled normally.
- **Floating** keeps independent window positions and sizes.

In floating mode, or for a window selected by a floating rule, hold `Super` and drag with the left
mouse button to move it. Drag with the right button to resize both axes, the middle button to resize
vertically, or `Super+Shift` plus the right button to resize horizontally.

Parented dialogs float automatically by default. `[[window_rules]]` entries in `config.toml` can
match `app_id`, `title`, or both and set `floating = true` or `false`; later matching rules win.

## Multiple monitors

The direct DRM backend discovers connected displays, uses each display's preferred resolution and
refresh rate, and places monitors from left to right. Connecting, disconnecting or changing a
monitor is handled while Anvil is running. Each monitor keeps its own selected tag and layout mode,
renders its own bar, and shows the launcher only on the focused monitor. Windows from a disconnected
monitor move to a remaining display automatically. When several monitors are connected, the active
bar is marked with a line configured through `bar.output_focus_color`.

Add a `[[outputs]]` block to `config.toml` for each display that needs fixed settings. `name` is the
DRM connector name; mode uses `WIDTHxHEIGHT@HZ`. `mode`, `position`, `scale` and `transform` are all
optional, so displays without matching entries keep the automatic behavior.

```toml
[[outputs]]
name = "DP-1"
mode = "2560x1440@144"
position = [0, 0]
scale = 1.25
transform = "normal"
```

Transforms are `normal`, `90`, `180`, `270`, `flipped`, `flipped-90`, `flipped-180` and
`flipped-270`. Fractional values such as `1.25`, `1.5` and `1.75` are supported; clients receive
the appropriate preferred scale when moving between monitors. Hotplugging remains active with
static display settings.

## Input configuration

`[input.keyboard]` configures the XKB `layout` and `variant` plus `repeat_rate` and
`repeat_delay`. Optional `[input.mouse]` values select the `flat` or `adaptive` acceleration
profile and a `sensitivity` from `-1.0` to `1.0`. Optional `[input.touchpad]` values control
`tap` and `natural_scroll`. Omitted mouse and touchpad values keep libinput's device defaults; see
[`config.toml`](config.toml) for a complete example.

## Control

The default build exposes a user-only Unix socket at `$XDG_RUNTIME_DIR/anvil.sock`:

```sh
anvilctl window list
anvilctl spawn firefox
anvilctl reload
anvilctl debug stats
```

Commands and responses use a versioned JSON protocol so bars and scripts can use the same API.

`debug stats` returns a JSON snapshot with uptime, connected Wayland clients (including XWayland),
managed windows, outputs, lock state, repaint requests, render attempts/submissions/failures,
average CPU-side render duration, DMA-BUF import successes/failures, RSS bytes and open file
descriptors. Rendering counters are cumulative since startup and count each output separately.
Repaint requests count calls before coalescing; submitted frames count successful backend
submissions, not display presentation or missed-vblank timing. Render duration excludes GPU
completion and compositor scheduling. Both backends coalesce scene/input/status changes;
nested mode also honors host exposure redraws without continuously scheduling idle frames. Linux process data is read
only when requested and is `null` when unavailable. Counters need no polling thread, external
service or additional dependency. The command is omitted with the existing `anvilctl` feature.

## Integration testing

The CI suite runs real Wayland clients against nested Anvil on Xvfb with Mesa software rendering
for minimal, default, layer-shell-only and all-feature builds. It checks startup, connection,
toplevel configure/resize/destroy, keyboard focus restoration, actual capture pixels, invalid
capture buffers, session reuse, lock isolation, layer exclusive zones and real grim/Waybar clients.
Default and all-feature runs also check IPC statistics, rapid keyboard focus/tag/layout changes,
window/popup churn, fullscreen requests, relative resource cleanup and failure recovery.
Protocol errors, crashed children and timeouts fail the suite; the runner
terminates its compositor/display/panel children and removes its temporary runtime directory.

To reproduce on Linux, install the development libraries listed above plus `xvfb`, `grim`,
`waybar`, `libxkbcommon-x11`, `dbus-daemon`, `ripgrep`, `xdotool`, `wl-clipboard`, `xwayland`,
`x11-utils` (including `xmessage`), Python 3 and a C compiler, then run:

```sh
cargo build --all-features --locked
tests/protocols/build.sh
tests/protocols/run.sh all-features
```

This short suite runs on ordinary pull requests. DMA-BUF driver behavior, physical display
hotplug, VT switching and suspend/resume still need direct hardware validation; nested tests
do not claim those results. Activation and driver-backed DMA-BUF crash scenarios remain
additional coverage for suitable environments.

### Stress, resources and recovery

The normal PR run uses 30 iterations per batch and four measured batches after an identical
warm-up. Each iteration creates/destroys two windows and a popup, checks configure/focus and
exercises fullscreen requests. The runner separately drives real focus/tag/move-to-tag and
tiling/monocle/floating shortcuts, repeated independent client connections, selection owner
death, an active pointer-lock owner crash, simultaneous client exits and invalid config/IPC.
The C fixture releases replacement SHM allocations so the harness itself can run long soaks.

```sh
# Thousands of windows/popups, with more time allowed for a local soak.
ANVIL_SOAK_ITERATIONS=1000 ANVIL_SOAK_BATCHES=8 ANVIL_SOAK_TIMEOUT=18000 \
  tests/protocols/run.sh all-features
```

`target/protocol-tests/resources.json` records baseline and per-batch RSS, FD counts, idle CPU
percentage and render counters. RSS is compared with the warmed baseline (15% or 4 MiB slack,
whichever is larger), and late growth is checked again across batches. FD growth over two
descriptors fails. An unchanged desktop may submit at most two host-exposure frames in the
one-second idle sample; idle CPU is measured rather than subjected to a machine-wide absolute
limit. Tune workload length, not the baseline, when investigating growth. These finite tests
detect regressions within the measured workload; they cannot prove indefinite leak freedom.

All-feature CI also invokes `logical_output_recovery` explicitly: a live Wayland window is
migrated off a removed logical fullscreen output, survives zero outputs, and becomes reachable
when an output returns. Clients created while no output exists are retained and mapped on return, while locked-output bookkeeping stays isolated. This state-level test
does not emulate KMS hotplug. A fresh nested instance runs a real X11 window alongside a native
client, kills XWayland and checks native focus, object cleanup and subsequent new windows.
A crashed session locker must remain locked and reject screenshots; this intentionally ends
that test session and requires a session restart rather than silently unlocking it.

### Binary and dependency footprint

CI builds and measures `minimal`, `default`, `all-features` and `default-xwayland` with Rust
1.87.0 and the normal release LTO/strip settings. The checked-in
[`tests/footprint/baseline.json`](tests/footprint/baseline.json) records exact binary bytes and
the target's normal/build dependency package versions for every profile. The job summary and
`binary-footprint-<profile>` artifacts contain current measurements. The initial x86_64 Linux baseline is:

| Profile | Binary bytes | Dependencies |
| --- | ---: | ---: |
| minimal | 9,823,312 | 139 |
| default | 10,413,344 | 152 |
| all-features | 11,209,336 | 154 |
| default + XWayland | 11,086,312 | 154 |

Any dependency addition/removal or
binary increase exceeding both 12% and 256 KiB produces a visible CI warning. Small size
variation and shrinkage do not fail builds; different compiler/target baselines are marked
informational. A measurement/build failure still fails the job.

```sh
python3 tests/footprint/test_check.py
python3 tests/footprint/check.py
# Update only after reviewing the feature/dependency cost, and commit the resulting baseline.
python3 tests/footprint/check.py --record
```

The [direct-backend hardware checklist](docs/hardware-validation.md) records physical GPU,
display hotplug, VT, lock and suspend/resume validation separately from automated nested CI.
