# Neofetch identifies Anvil as Sway

Older Neofetch versions detect Wayland window managers by looking for a fixed
list of process-name substrings. Anvil is absent from that list. When a wallpaper
client named `swaybg` is running, its name matches `sway`, producing the incorrect
`WM: sway` line. This does not mean the desktop is running Sway.

Anvil supplies its own Wayland session identity to applications it launches.
That helps tools which understand the standard desktop environment variables.
The older Neofetch heuristic still needs a small configuration hook; a compositor
cannot add itself to another program's hardcoded detection list.

## Configure Neofetch once

After installing the current source with `sudo make install`, run this
**as your normal user**, without `sudo`:

```sh
anvil-neofetch-configure
neofetch
```

From a source checkout, the equivalent command is:

```sh
make configure-neofetch
```

Release archives created before this integration (including the original
v0.7.0 archive) do not contain the helper; use the current source checkout for
that command. Future release archives include the standalone helper.

The helper updates `$XDG_CONFIG_HOME/neofetch/config.conf`, or
`~/.config/neofetch/config.conf` when `XDG_CONFIG_HOME` is unset. It keeps your
existing settings and their file permissions, saves the original configuration
once as `config.conf.before-anvil-wm`, and replaces only its own marked hook when
run again. Dotfile symlinks remain symlinks; their target is updated and the
backup is written beside that target. A missing configuration is created without
replacing Neofetch's built-in defaults.

An explicit file is also supported:

```sh
anvil-neofetch-configure --config /path/to/neofetch/config.conf
```

The hook sets `wm=Anvil` only for an Anvil Wayland session, using a
case-insensitive component match on `XDG_CURRENT_DESKTOP`. Other desktop sessions
keep Neofetch's normal detection. It changes neither the wallpaper process nor
installed Neofetch package files. Existing terminals can run `neofetch` again;
newly installed session-identity changes apply to applications launched by the
new Anvil version.

The standalone hook is available in
[`contrib/neofetch/anvil.conf`](../contrib/neofetch/anvil.conf) for distributions
which manage user configuration themselves. Remove the marked
`# BEGIN Anvil session identity` through `# END Anvil session identity` block to
return to unmodified Neofetch detection.
