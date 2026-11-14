# Configuration guide

All the usual choices live in **`~/.config/anvil/config.toml`**: your terminal, keybinds, gaps, layouts, colors, displays, floating rules and bar. Layout sizes stay at least one logical pixel; extreme gap values saturate coordinates at the supported integer bounds.

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

The `icon_size` option enlarges Nerd Font private-use symbols while leaving normal text at `font_size`. It expects a font containing those icons (a fully patched Nerd Font works best); missing glyphs use installed Fontconfig fallbacks, without color-emoji rendering or complex shaping. `icon_size` and most bar appearance changes work on reload. Changing the font family still needs a restart.

Use `anvilctl bar refresh updates` to rerun a named status block immediately (e.g. from an Arch pacman hook), or `anvilctl bar refresh --all` to refresh every block. Names come from the active `[[bar.blocks]]` and must be unique. One async execution updates the status on every monitor, without additional polling. See [Async bar blocks](async-bar-blocks.md) for a safe pacman-hook wrapper.

The repository includes [ready-to-adapt sample scripts](../scripts/bar) for Gentoo updates, network, weather, kernel and battery. The Gentoo checker reads the local Portage metadata; it **never** syncs the package tree or asks for sudo from the bar. For details, see [Async bar blocks](async-bar-blocks.md).

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

