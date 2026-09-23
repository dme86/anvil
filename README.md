# anvil

<p align="center">
  <img src="anvil.png" alt="anvil" width="420">
</p>

A minimal, dwm-inspired dynamic tiling Wayland compositor written in Rust with
[Smithay](https://github.com/Smithay/smithay).

Anvil currently provides a master/stack layout, nine tags, keyboard-driven window management and
a small [`config.toml`](config.toml). It runs nested through Smithay's Winit backend; direct
DRM/libinput support is the next milestone.

## Build

```sh
cargo build --release --features compositor
mkdir -p ~/.config/anvil
cp config.toml ~/.config/anvil/config.toml
./target/release/anvil
```

## Keys

- `Super+Return`: terminal
- `Super+j/k`: focus next/previous window
- `Super+h/l`: resize master area
- `Super+Shift+Return`: move window to master
- `Super+Shift+c`: close window
- `Super+1..9`: select tag
- `Super+Shift+1..9`: move window to tag
- `Super+Shift+q`: quit
