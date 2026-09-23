# anvil

<p align="center">
  <img src="anvil.png" alt="anvil" width="420">
</p>

A minimal, dwm-inspired dynamic tiling Wayland compositor written in Rust with
[Smithay](https://github.com/Smithay/smithay).

Anvil provides a master/stack layout, nine tags, keyboard-driven window management, an optional
dwm-style bar and a small [`config.toml`](config.toml). It runs directly on DRM/KMS and libinput;
Winit remains available for nested development.

## Build

```sh
cargo build --release
mkdir -p ~/.config/anvil
cp config.toml ~/.config/anvil/config.toml
sudo install -Dm755 target/release/anvil /usr/local/bin/anvil
sudo install -Dm644 anvil.desktop /usr/share/wayland-sessions/anvil.desktop
```

Use `cargo build --release --no-default-features` to build Anvil without the bar.

Select **Anvil** in a display manager, or launch `anvil` from a free TTY. Use `anvil --nested` to
run it in a window inside an existing graphical session.

## Keys

- `Super+Return`: terminal
- `Super+j/k`: focus next/previous window
- `Super+h/l`: resize master area
- `Super+Shift+Return`: move window to master
- `Super+Shift+c`: close window
- `Super+1..9`: select tag
- `Super+Shift+1..9`: move window to tag
- `Super+Shift+q`: quit
