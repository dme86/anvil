# anvil

<p align="center">
  <img src="anvil.png" alt="anvil" width="420">
</p>

A minimal, dwm-inspired dynamic tiling Wayland compositor written in Rust with
[Smithay](https://github.com/Smithay/smithay).

Anvil provides a master/stack layout, configurable tags, keyboard-driven window management, an
optional dwm-style bar using the system's Fontconfig fonts and a small
[`config.toml`](config.toml). It runs directly on DRM/KMS and libinput; Winit remains available for
nested development.

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
- `Super+p`: application launcher
- `Super+h/l`: resize master area
- `Super+Shift+Return`: move window to master
- `Super+Shift+c`: close window
- `Super+1..9`: select tag
- `Super+Shift+1..9`: move window to tag
- `Super+Shift+q`: quit

## Control

The default build exposes a user-only Unix socket at `$XDG_RUNTIME_DIR/anvil.sock`:

```sh
anvilctl window list
anvilctl spawn firefox
anvilctl reload
```

Commands and responses use a versioned JSON protocol so bars and scripts can use the same API.
