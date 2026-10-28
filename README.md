<p align="center">
  <img src="anvil.png" alt="Anvil — a keyboard-first Wayland window manager" width="420">
</p>

<p align="center">
  <strong>Your desktop. Your shortcuts. Nothing in the way.</strong>
</p>

<p align="center">
  <a href="https://github.com/dme86/anvil/actions/workflows/ci.yml"><img src="https://github.com/dme86/anvil/actions/workflows/ci.yml/badge.svg" alt="Build status"></a>
  <a href="https://github.com/dme86/anvil/releases/latest"><img src="https://img.shields.io/github/v/release/dme86/anvil?label=latest" alt="Latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/dme86/anvil" alt="MIT license"></a>
  <img src="https://img.shields.io/badge/built_with-Rust-orange?logo=rust" alt="Built with Rust">
</p>

**Anvil is a lightweight, keyboard-driven Wayland window manager inspired by dwm.** It gets you from an empty screen to the terminal, browser or app you actually wanted — fast, with sensible defaults and without a full desktop environment deciding how you should work.

If you love tiling, minimal interfaces and changing things to suit *your* workflow, you might feel right at home.

## Why Anvil?

- **Stay on the keyboard.** Open apps, switch windows, jump between tags and arrange your desktop without hunting for a mouse.
- **Three layouts, your tags.** Each tag remembers its own master/stack tiling, monocle or floating mode independently, even across monitors. Super+Space only changes the active tag.
- **Your workspace, your monitors.** Move windows between displays, navigate in the direction the monitors actually sit, and keep working when one is unplugged.
- **A bar that stays out of your way.** Configurable tags, window titles and asynchronous shell-powered status blocks. Press **Super+B** to hide it and give that space back to your windows.
- **Your favorite symbols, at the right size.** Use a Nerd Font and adjust `icon_size` without making the rest of the text huge.
- **Find windows instantly.** **Super+P** opens the app launcher; **Super+Tab** brings up an overview of windows across tags and monitors.
- **A desktop that plays nicely with your tools.** Wallpapers with `swaybg` and external panels such as Waybar work out of the box. Leave out the bar, launcher, overview or IPC for an extra-lean build; add XWayland for older apps.
- **Built for actual Linux desktops.** Native Wayland, DRM/KMS, libinput, clipboard, multi-output, window rules, session locking and modern rendering protocols.

Anvil is still evolving. The project runs automated Wayland client and resource tests, while full native-hardware verification is tracked [here](docs/hardware-validation.md). It is a small project with big daily-driver ambitions — not a full desktop distribution.

## See the workflow

| You want to… | Do this |
| --- | --- |
| Open your terminal | **Super+Enter** |
| Open the app launcher | **Super+P** |
| Find a window on any tag or monitor | **Super+Tab** |
| Switch layout | **Super+Space** |
| Hide / show the built-in bar | **Super+B** |
| Focus next / previous window | **Super+J / K** |
| Resize the master area | **Super+H / L** |
| Move a window to master | **Super+Shift+Enter** |
| Jump to a tag | **Super+1–9** |
| Send a window to a tag | **Super+Shift+1–9** |
| Focus the monitor to the left/right/up/down | **Super+Arrow keys** |
| Move a window to the monitor in that direction | **Super+Shift+Arrow keys** |
| Cycle through monitors | **Super+, / .** |
| Move a window to previous / next monitor | **Super+Shift+, / .** |
| Close the focused window | **Super+Shift+C** |
| Quit Anvil | **Super+Shift+Q** |

These are the defaults; change the keys under `[keys]` in [config.toml](config.toml). **Super** is configurable as `modifier`. The launcher, overview and bar shortcuts only exist when those features are compiled in.

Want to move or resize a floating window? Hold **Super** while dragging with the left mouse button to move, right button to resize, or middle button for vertical resizing. **Super+Shift+right drag** resizes horizontally.

## Get started

### Grab a release

Find prebuilt Linux packages on the [Releases page](https://github.com/dme86/anvil/releases). **Choose a build that matches your CPU architecture.** If your platform does not have a prebuilt archive (including aarch64, depending on the release), build it on the target machine.

You'll need a Linux system with working DRM/KMS, libinput, a seat manager (logind or seatd), an installed cursor theme, and your preferred terminal. Alacritty is the default. [The example configuration](config.toml) is a good place to start.

### Build it yourself

Install a current Rust toolchain (Rust **1.87+**) and the development libraries for EGL, GBM, libinput, libseat, libudev, Wayland and xkbcommon, then:

```sh
git clone https://github.com/dme86/anvil.git
cd anvil
cargo build --release

mkdir -p ~/.config/anvil
cp config.toml ~/.config/anvil/config.toml

sudo install -Dm755 target/release/anvil /usr/local/bin/anvil
sudo install -Dm755 target/release/anvilctl /usr/local/bin/anvilctl
sudo install -Dm644 anvil.desktop /usr/share/wayland-sessions/anvil.desktop
```

Select **Anvil** in your display manager or run `anvil` from an available Linux TTY. For development inside another graphical session, use `anvil --nested`.

**Already using Gentoo?** Building natively on aarch64 works too, as long as the corresponding Smithay system dependencies are installed. Avoid installing the x86_64 release binary on an ARM guest.

### Pick the build you actually want

The default build enables the built-in bar, launcher, overview, `anvilctl` **and layer-shell**. That means Wayland wallpaper tools (such as `swaybg`) and external panels (such as Waybar) work without extra Cargo flags. The `bar`, `launcher`, `overview`, `anvilctl`, `xwayland` and `layer-shell` features remain individually selectable when you disable defaults.

```sh
# The Anvil experience: bar, launcher, overview, CLI and layer-shell
cargo build --release

# Just the essentials: no optional UI or IPC
cargo build --release --no-default-features

# Keep the bar and launcher, drop the other optional integrations
cargo build --release --no-default-features --features bar,launcher

# Regular desktop plus legacy X11 applications
cargo build --release --features xwayland

# External Waybar-style panels instead of the built-in bar
cargo build --release --no-default-features --features layer-shell,anvilctl

# Everything available in one build
cargo build --release --all-features
```

The companion `anvilctl` binary exists only when the `anvilctl` feature is enabled. For X11 support, also install the `Xwayland` executable and set `[compat].xwayland = true`. To use an external bar **instead** of the built-in bar, build with `--no-default-features --features layer-shell,anvilctl`, then run that bar separately.

### Put a wallpaper behind your windows

With the **default build**, install `swaybg` and choose a wallpaper:

```sh
swaybg -i ~/.local/share/wallpapers/fuji.png -m fill &
```

`swaybg` runs as a separate process and keeps the background visible without adding a compositor timer. For JPG/JPEG images, make sure your `swaybg` package supports those formats (Gentoo's `gdk-pixbuf` USE flag enables additional image loaders). The minimal `--no-default-features` build intentionally does **not** advertise layer-shell. **Super+B** still returns the built-in bar's reserved space to windows, even when layer-shell is enabled.

## Make it yours

All the usual choices live in **`~/.config/anvil/config.toml`**: your terminal, keybinds, gaps, layouts, colors, displays, floating rules and bar.

### A bar powered by your scripts

Already have Bash scripts for battery, weather or package updates? Put them to work. Each status block runs independently in the background; a slow network request doesn't pause your compositor.

```toml
[bar]
height = 26
font = "JetBrainsMono Nerd Font Mono"
font_size = 14.0
icon_size = 19.0
# Existing bar colors and other options still apply.

[[bar.blocks]]
name = "weather"
command = "bash ~/.config/anvil/scripts/weather.sh"
interval = 1800

[[bar.blocks]]
name = "updates"
command = "bash ~/.config/anvil/scripts/gentoo_updates.sh"
interval = 3600
timeout = 60

[[bar.blocks]]
name = "clock"
command = "date '+%H:%M'"
interval = 60

[keys]
modifier = "Super"
toggle_bar = "b"
```

The `icon_size` option enlarges Nerd Font private-use symbols while leaving normal text at `font_size`. It expects a font containing those icons (a fully patched Nerd Font works best); it does **not** add emoji fallback. `icon_size` and most bar appearance changes work on reload. Changing the font family still needs a restart.

Use `anvilctl bar refresh updates` to rerun a named status block immediately (e.g. from an Arch pacman hook), or `anvilctl bar refresh --all` to refresh every block. Names come from the active `[[bar.blocks]]` and must be unique. One async execution updates the status on every monitor, without additional polling. See [Async bar blocks](docs/async-bar-blocks.md) for a safe pacman-hook wrapper.

The repository includes [ready-to-adapt sample scripts](scripts/bar) for Gentoo updates, network, weather, kernel and battery. The Gentoo checker reads the local Portage metadata; it **never** syncs the package tree or asks for sudo from the bar. For details, see [Async bar blocks](docs/async-bar-blocks.md).

**Super+B** hides the bar on all monitors and instantly expands the usable window area. The setting lives in memory; it stays hidden until you toggle it back, even if you reload your configuration.

### Tiling, tags, floating and window rules

Tiling puts your main app in a master area and the rest in a stack. Monocle gives one window the display. Floating lets you arrange windows freely. **Each tag remembers its own layout on each monitor.** For example, you can keep Tag 1 tiled, Tag 2 in monocle and Tag 3 floating. Press **Super+Space** (or click the layout symbol) to change only the current tag, then **Super+1–9** to switch back without losing that choice.

For windows visible on multiple tags, Anvil uses the layout of the **lowest numbered selected tag**. The other tags keep their own layout settings. Switching monitors, reloading configuration, hiding the bar or changing the display resolution will not reset layouts for existing tags. Freshly created outputs start with Tiling; tag modes are runtime state and reset when Anvil exits.

Keep a dialog floating, or place a favorite app on a specific monitor and tag:

```toml
[[window_rules]]
app_id = "firefox"
output = "DP-1"
tag = 1

[[window_rules]]
app_id = "org.gnome.Calculator"
floating = true
```

Anvil handles monitor disconnects without stranding windows. Use `anvilctl output list` to discover connector names. The example config documents optional resolution, scale, orientation and input settings.

### Tweak things while Anvil is running

```sh
anvilctl config validate
anvilctl config get layout.gap
anvilctl config set layout.gap 12 --reload
anvilctl reload
anvilctl output list
anvilctl window list
anvilctl debug stats
```

Live reload covers gaps, layouts, most bar settings (including icon size), the script blocks, many window rules, and supported input/display changes. Changing the **bar font family**, keyboard layout, startup applications, XWayland setting or built-in network widget initialization still needs a restart. Invalid settings are rejected without replacing the active configuration.

## A few more things you'll appreciate

Anvil supports native Wayland apps, clipboard and primary selection, screenshots/output capture, session locking, touchpads and mouse configuration, XDG activation, per-output fractional scaling and optional legacy X11 clients. For games and specialized apps, relative pointer and pointer confinement are available. On a quiet desktop, the direct DRM backend schedules redraws when something actually changes instead of painting an empty screen endlessly.

You'll find more background in the [hardware validation checklist](docs/hardware-validation.md), [idle CPU profiling guide](docs/idle-cpu-profiling.md) and [async-bar documentation](docs/async-bar-blocks.md). CI runs real nested Wayland protocol tests, stress checks and binary-footprint measurements; no particular graphics card or seat setup is promised compatible until physically tested.

## Contribute

Bug reports, feature requests, ideas for the bar or launcher, and reports from **real hardware** are welcome. Check the [issues](https://github.com/dme86/anvil/issues) or open a pull request.

Anvil is open source under the [MIT License](LICENSE). Built in Rust, inspired by dwm, and made for people who enjoy having their desktop exactly the way they like it.
