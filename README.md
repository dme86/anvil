# anvil

<p align="center">
  <img src="anvil.png" alt="anvil" width="420">
</p>

A minimal, dwm-inspired dynamic tiling Wayland compositor written in Rust with
[Smithay](https://github.com/Smithay/smithay).

Anvil provides a master/stack layout, configurable tags, keyboard-driven window management, an
optional dwm-style bar using the system's Fontconfig fonts and a small
[`config.toml`](config.toml). It runs directly on DRM/KMS and libinput; Winit remains available for
nested development. Both shared-memory and `linux-dmabuf` client buffers are supported, allowing
native Wayland applications to use GPU-backed rendering when available. External lock-screen
clients can secure the session through `ext-session-lock-v1`. The standard clipboard and the
select-to-copy, middle-click primary selection are both supported.

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

`bar`, `anvilctl` and `launcher` are independent features. The ordinary build enables all three:

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
```

Always install `target/release/anvil`. Install `target/release/anvilctl` as well only when the
`anvilctl` feature was enabled for that build.

Select **Anvil** in a display manager, or launch `anvil` from a free TTY. Use `anvil --nested` to
run it in a window inside an existing graphical session.

## Keys

- `Super+Return`: terminal
- `Super+j/k`: focus next/previous window
- `Super+,/.`: focus previous/next output
- `Super+Shift+,/.`: move the focused window to the previous/next output
- `Super+p`: application launcher
- `Super+h/l`: resize master area
- `Super+Shift+Return`: move window to master
- `Super+Shift+c`: close window
- `Super+1..9`: select tag
- `Super+Shift+1..9`: move window to tag
- `Super+Shift+q`: quit

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
scale = 1.0
transform = "normal"
```

Transforms are `normal`, `90`, `180`, `270`, `flipped`, `flipped-90`, `flipped-180` and
`flipped-270`. Hotplugging remains active with static display settings.

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
```

Commands and responses use a versioned JSON protocol so bars and scripts can use the same API.
