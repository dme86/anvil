# Asynchronous shell blocks for the built-in bar

Anvil's default bar supports independent shell blocks, modeled after
[dwmblocks-async](https://github.com/dme86/dwm/tree/main/dwmblocks-async/scripts).
The compositor never waits for a script: a bounded worker captures one-line stdout,
and a Calloop notification updates only the changed bar text. No repaint is requested
when values are identical.

## Setup

Copy selected examples from `scripts/bar/` to `~/.config/anvil/scripts/`.
Install Bash and GNU coreutils (`timeout`), plus any tools needed by a script.
Scripts can be invoked as `bash ~/.config/anvil/scripts/weather.sh` without
an executable bit; or run `chmod +x ~/.config/anvil/scripts/*.sh`.

```toml
[bar]
font = "Symbols Nerd Font Mono"
# Optional legacy setting: ignored when [[bar.blocks]] are defined.
# status_commands = ["date '+%H:%M'"]
refresh_interval_ms = 1000

[[bar.blocks]]
name = "internet"
command = "bash ~/.config/anvil/scripts/internet.sh"
interval = 15

[[bar.blocks]]
name = "battery"
command = "bash ~/.config/anvil/scripts/battery.sh"
interval = 10

[[bar.blocks]]
name = "weather"
command = "bash ~/.config/anvil/scripts/weather.sh"
interval = 1800
timeout = 12

[[bar.blocks]]
name = "updates"
command = "bash ~/.config/anvil/scripts/gentoo_updates.sh"
interval = 3600
timeout = 60

[[bar.blocks]]
name = "kernel"
command = "bash ~/.config/anvil/scripts/kernel.sh"
interval = 86400

[[bar.blocks]]
name = "clock"
command = "date '+%a, %d %b %H:%M'"
interval = 60
```

Blocks display in configuration order, separated by ` | `. Empty output hides a block.
All scripts run as the current desktop user and must print only their display text to stdout.
Command strings use `/bin/sh -c`, so `~`, pipes and shell syntax work.
Intervals and timeouts are seconds; interval ranges from 1 to 86400, timeout from
1 to 120 (default 10). Maximum 16 blocks, each with a unique alphanumeric,
underscore or hyphen name. Output is limited to 4096 bytes and 256 visible characters.
`timeout` is required from GNU coreutils. A hanging/failed command retains its last
successful output. A command cannot overlap itself; other blocks keep updating.
Avoid destructive scripts or untrusted text in commands.

Legacy `status_commands` still work but now run on workers with
`refresh_interval_ms` rounded up to whole seconds. Adding `[[bar.blocks]]`
supersedes the legacy list (including the default clock command). Anvil does not
run scripts when compiled without the `bar` feature.

`anvilctl reload` replaces script definitions atomically after validation,
invalidates older results, and starts newly configured jobs. The status bar shows
new values as each block completes; no compositor restart is needed.

## Gentoo updates

The Portage example reads **already synchronized** repositories using
`emerge --pretend --update --deep --newuse @world` and counts planned
`[ebuild ...]` operations. It never runs `emerge --sync` or `sudo` from the bar.
Run `emaint sync -a` separately during regular maintenance. The count includes
upgrades, new dependencies and rebuilds, not only direct package version bumps.
If the user lacks permissions or Portage encounters a conflict, the script exits
nonzero and Anvil keeps the last successful value. It is not a live online
update feed.

## Operational safeguards

Worker commands have a timeout, capped captured output, and do not block input,
rendering, or DRM. Results arrive through Calloop pings. Any job from before a
configuration reload is ignored. The 1s (configurable) bar check only launches
jobs which are due. Same-value output does not request a new KMS frame.

The optional older `[bar.network]` NetworkManager widget uses its original
command path; it is separate from script blocks.
