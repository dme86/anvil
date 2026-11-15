# Build and install

### Grab a release

Find prebuilt Linux packages on the [Releases page](https://github.com/dme86/anvil/releases). **Choose a build that matches your CPU architecture.** If your platform does not have a prebuilt archive (including aarch64, depending on the release), build it on the target machine.

You'll need a Linux system with working DRM/KMS, libinput, a seat manager (logind or seatd), an installed cursor theme, and your preferred terminal. [`rst`](https://github.com/dme86/rst) is the default terminal (Super+Enter). Install it separately (`make install` in the rst repository, with `~/.local/bin` on your `PATH`), or change `[general].terminal` in your Anvil configuration to use another emulator. [The example configuration](../config.toml) is a good place to start.

### Build it yourself

Install a current Rust toolchain (Rust **1.87+**) and the development libraries for FreeType, EGL, GBM, libinput, libseat, libudev, Wayland and xkbcommon, then:

```sh
git clone https://github.com/dme86/anvil.git
cd anvil
make                    # Show all documented targets
make build              # Build the default release as your normal user
sudo make install       # Install binaries + Wayland session, without recompiling as root

mkdir -p ~/.config/anvil
cp -n config.toml ~/.config/anvil/config.toml  # Optional starting config; keeps existing files
```

Select **Anvil** in your display manager or run `anvil` from an available Linux TTY. For development inside another graphical session, use `anvil --nested`.

**Already using Gentoo?** Building natively on aarch64 works too, as long as the corresponding Smithay system dependencies are installed. Avoid installing the x86_64 release binary on an ARM guest.

### Pick the build you actually want

The Makefile wraps Cargo and remembers **which release variant was last built**, so `sudo make install` installs the matching binaries rather than compiling as root or accidentally copying a stale artifact. It also installs `anvil.desktop` to the usual display-manager directory. The installed config in `~/.config/anvil/` is never overwritten. Run `make` or `make help` for the full automatically documented target list.

```sh
make build              # Full desktop
make build-no-bar       # Launcher, overview, IPC and layer-shell; no built-in bar
make build-minimal      # Small core; no optional UI or anvilctl
make build-xwayland     # Default desktop + XWayland
make build-all          # All features
sudo make install       # Installs whichever variant you just built
make check              # Formatting, Rust tests, Clippy, Makefile tests
```

You can change installation paths without editing files, for example: `make install PREFIX=$HOME/.local SESSION_DIR=$HOME/.local/share/wayland-sessions` (user session entries might not be discovered by display managers). For distribution packaging, `make install DESTDIR=/tmp/anvil-package PREFIX=/usr` stages files under that directory while keeping the correct `Exec=/usr/bin/anvil` in the desktop entry. By default, binaries go into `/usr/local/bin` and the session file into `/usr/share/wayland-sessions`. To remove those installed files, use `sudo make uninstall`. The minimal variant deliberately skips installing `anvilctl`; it does not delete a previously installed CLI.

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

