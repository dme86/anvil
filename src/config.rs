//! Loading and validation of Anvil's single `config.toml` interface.
//!
//! The structs in this module mirror the TOML sections. Every section has defaults, which means a
//! user may override one value without copying the complete example configuration. Unknown keys
//! are rejected on purpose: a typo in a window manager config should fail loudly instead of being
//! silently ignored and leaving the user with surprising behavior.

use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Complete runtime configuration assembled from defaults and an optional TOML file.
pub struct Config {
    pub general: General,
    pub compat: Compat,
    pub input: Input,
    pub layout: Layout,
    pub appearance: Appearance,
    pub bar: Bar,
    pub floating: Floating,
    pub window_rules: Vec<WindowRule>,
    pub keys: Keys,
    /// Optional, unmodified XF86 multimedia keys. Empty commands pass through to clients.
    pub media: Media,
    /// Optional static settings matched against DRM connector names such as `DP-1`.
    pub outputs: Vec<OutputConfig>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Optional compatibility services which are not required by native Wayland clients.
pub struct Compat {
    /// Starts XWayland when the binary was compiled with the `xwayland` Cargo feature.
    pub xwayland: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Keyboard and libinput device settings. Optional device values preserve libinput defaults.
pub struct Input {
    pub keyboard: Keyboard,
    pub mouse: Mouse,
    pub touchpad: Touchpad,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// XKB keymap and repeat settings used when Anvil creates its single logical seat.
pub struct Keyboard {
    pub layout: String,
    pub variant: String,
    pub repeat_rate: i32,
    pub repeat_delay: i32,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Overrides applied only to libinput devices that expose pointer acceleration.
pub struct Mouse {
    pub accel_profile: Option<AccelProfile>,
    pub sensitivity: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Overrides applied to tap-capable libinput devices, which identifies touchpads reliably.
pub struct Touchpad {
    pub tap: Option<bool>,
    pub natural_scroll: Option<bool>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
/// Acceleration algorithms exposed by libinput.
pub enum AccelProfile {
    Flat,
    Adaptive,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Static overrides for one DRM output. Missing fields retain the automatic backend defaults.
pub struct OutputConfig {
    pub name: String,
    pub mode: Option<OutputMode>,
    pub position: Option<[i32; 2]>,
    pub scale: Option<f64>,
    pub transform: Option<OutputTransform>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
/// A requested pixel size and refresh rate; refresh is stored in millihertz like Smithay's mode.
pub struct OutputMode {
    pub width: i32,
    pub height: i32,
    pub refresh_millihz: i32,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
/// Rotation and reflection names accepted by `[[outputs]]` entries.
pub enum OutputTransform {
    Normal,
    #[serde(rename = "90")]
    Rotate90,
    #[serde(rename = "180")]
    Rotate180,
    #[serde(rename = "270")]
    Rotate270,
    Flipped,
    #[serde(rename = "flipped-90")]
    Flipped90,
    #[serde(rename = "flipped-180")]
    Flipped180,
    #[serde(rename = "flipped-270")]
    Flipped270,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Programs started by the compositor itself.
pub struct General {
    pub terminal: String,
    pub startup: Vec<String>,
    /// Number of dwm-style tags exposed by the bar and numeric key bindings.
    pub tags: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Values controlling the dwm-style master/stack geometry.
pub struct Layout {
    pub gap: i32,
    pub outer_gap: i32,
    pub master_factor: f64,
    pub master_count: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Minimal visual settings kept separate from layout policy.
pub struct Appearance {
    pub background: [f32; 4],
    /// Focus-ring color in CSS-style `#RRGGBB` or `#RRGGBBAA` notation.
    pub focus_border_color: String,
    /// Focus-ring thickness in logical pixels.
    pub focus_border_width: i32,
    /// Whether clients may draw their own title bar and window-control buttons.
    ///
    /// When false, Anvil advertises server-side decoration mode. Anvil intentionally draws no
    /// server-side frame, producing the borderless windows expected from a minimal tiling WM.
    pub client_side_decorations: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Appearance and shell-fed status content for the optional dwm-style bar.
pub struct Bar {
    /// Logical height reserved at the top of the output.
    pub height: i32,
    /// Fontconfig family or pattern resolved against fonts installed on the system.
    pub font: String,
    /// Font size in logical pixels.
    pub font_size: f32,
    /// Maximum number of miniature window markers drawn next to each tag number.
    ///
    /// The cap keeps a busy tag from consuming an unbounded part of the bar while still making
    /// the common case (a handful of windows) visible at a glance.
    pub max_window_indicators: usize,
    pub background: String,
    pub foreground: String,
    pub selected_background: String,
    pub selected_foreground: String,
    /// Color of the thin active-output line shown only while multiple monitors are connected.
    pub output_focus_color: String,
    pub occupied: String,
    /// Fast shell commands whose trimmed stdout is joined from left to right in the status area.
    pub status_commands: Vec<String>,
    /// Optional right-hand NetworkManager icon with its own click target.
    pub network: BarNetwork,
    /// Minimum time between status command executions.
    pub refresh_interval_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Native NetworkManager status icon, disabled unless explicitly configured.
pub struct BarNetwork {
    /// Enable a clickable Wi-Fi/Ethernet/offline glyph in the right-hand bar.
    pub enabled: bool,
    /// Fontconfig font family supplying the three Nerd Font symbols.
    pub icon_font: String,
    /// Shell command launched by clicking the glyph (typically "alacritty -e nmtui-connect").
    pub click_command: String,
    /// Minimum interval between NetworkManager queries, in milliseconds.
    pub refresh_interval_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Default policy and initial geometry for windows that are not part of the tile tree.
pub struct Floating {
    /// Whether an xdg-toplevel with a parent is treated as a dialog automatically.
    pub dialogs: bool,
    /// Initial logical width used when a rule first turns a window into a floating window.
    pub default_width: i32,
    /// Initial logical height used when a rule first turns a window into a floating window.
    pub default_height: i32,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
/// One ordered window rule. Omitted match fields act as wildcards over that property.
pub struct WindowRule {
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub floating: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Key names are stored as strings so changing bindings never requires recompilation.
pub struct Keys {
    pub modifier: String,
    pub terminal: String,
    pub quit: String,
    pub focus_next: String,
    pub focus_previous: String,
    /// Focuses the output to the left; Shift with either output key moves the focused window.
    pub output_previous: String,
    /// Focuses the output to the right; Shift moves the focused window there.
    pub output_next: String,
    /// Opens the application launcher.
    pub launcher: String,
    /// Cycles Tiling -> Fullscreen/Monocle -> Floating.
    pub layout_mode: String,
    pub swap_master: String,
    pub master_grow: String,
    pub master_shrink: String,
    pub close: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Commands executed on unmodified XF86 media-key presses. All are opt-in.
pub struct Media {
    pub volume_up: String,
    pub volume_down: String,
    pub volume_mute: String,
    pub brightness_up: String,
    pub brightness_down: String,
}

impl Default for General {
    fn default() -> Self {
        Self {
            // Alacritty is the default terminal opened by Super+Return.
            // Users can override this with any terminal command in config.toml.
            terminal: "alacritty".into(),
            startup: Vec::new(),
            tags: 4,
        }
    }
}

impl Default for Keyboard {
    fn default() -> Self {
        Self {
            // Empty XKB names defer to the environment and ultimately xkeyboard-config's default.
            layout: String::new(),
            variant: String::new(),
            repeat_rate: 25,
            repeat_delay: 200,
        }
    }
}

impl<'de> Deserialize<'de> for OutputMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl FromStr for OutputMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (size, refresh) = value
            .split_once('@')
            .ok_or_else(|| "output mode must use WIDTHxHEIGHT@HZ notation".to_owned())?;
        let (width, height) = size
            .split_once('x')
            .ok_or_else(|| "output mode must use WIDTHxHEIGHT@HZ notation".to_owned())?;
        let width = width
            .parse::<i32>()
            .map_err(|_| "output mode width must be a positive integer".to_owned())?;
        let height = height
            .parse::<i32>()
            .map_err(|_| "output mode height must be a positive integer".to_owned())?;
        let refresh = refresh
            .parse::<f64>()
            .map_err(|_| "output mode refresh must be a positive number".to_owned())?;
        if width <= 0 || height <= 0 || !refresh.is_finite() || refresh <= 0.0 {
            return Err("output mode dimensions and refresh must be positive".to_owned());
        }
        let refresh_millihz = (refresh * 1_000.0).round();
        if refresh_millihz > f64::from(i32::MAX) {
            return Err("output mode refresh is too large".to_owned());
        }
        Ok(Self {
            width,
            height,
            refresh_millihz: refresh_millihz as i32,
        })
    }
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            gap: 8,
            outer_gap: 8,
            master_factor: 0.6,
            master_count: 1,
        }
    }
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            background: [0.06, 0.06, 0.07, 1.0],
            focus_border_color: "#707070".into(),
            focus_border_width: 2,
            client_side_decorations: false,
        }
    }
}

impl Default for Bar {
    fn default() -> Self {
        Self {
            height: 22,
            font: "monospace".into(),
            font_size: 16.0,
            max_window_indicators: 5,
            background: "#181818".into(),
            foreground: "#b8b8b8".into(),
            selected_background: "#707070".into(),
            selected_foreground: "#ffffff".into(),
            output_focus_color: "#707070".into(),
            occupied: "#d0d0d0".into(),
            status_commands: vec!["date '+%Y-%m-%d %H:%M'".into()],
            network: BarNetwork::default(),
            refresh_interval_ms: 1_000,
        }
    }
}

impl Default for BarNetwork {
    fn default() -> Self {
        Self {
            enabled: false,
            icon_font: "Symbols Nerd Font Mono".into(),
            click_command: "alacritty -e nmtui-connect".into(),
            refresh_interval_ms: 5_000,
        }
    }
}

impl Default for Floating {
    fn default() -> Self {
        Self {
            dialogs: true,
            default_width: 800,
            default_height: 600,
        }
    }
}

impl Default for Keys {
    fn default() -> Self {
        Self {
            modifier: "Super".into(),
            terminal: "Return".into(),
            quit: "q".into(),
            focus_next: "j".into(),
            focus_previous: "k".into(),
            output_previous: "comma".into(),
            output_next: "period".into(),
            launcher: "p".into(),
            layout_mode: "space".into(),
            swap_master: "Return".into(),
            master_grow: "l".into(),
            master_shrink: "h".into(),
            close: "c".into(),
        }
    }
}

impl Config {
    /// Loads an explicitly selected file or the XDG default location.
    ///
    /// A missing implicit file is not an error because Anvil is usable with built-in defaults.
    /// In contrast, a missing file passed with `--config` is an error: the caller explicitly
    /// requested that exact file and most likely made a path mistake.
    pub fn load(explicit: Option<&Path>) -> Result<(Self, Option<PathBuf>)> {
        let path = explicit.map(PathBuf::from).or_else(default_path);
        let Some(path) = path else {
            return Ok((Self::default(), None));
        };
        if !path.exists() && explicit.is_none() {
            return Ok((Self::default(), None));
        }

        let text = fs::read_to_string(&path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let config: Self =
            toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))?;
        config.validate()?;
        Ok((config, Some(path)))
    }

    /// Rejects values that would create unusable or invalid compositor state.
    ///
    /// Validation is kept after deserialization rather than hidden in the layout code. That gives
    /// users one clear startup error and lets the layout engine assume sane input thereafter.
    pub fn validate(&self) -> Result<()> {
        if self.input.keyboard.repeat_rate <= 0 {
            bail!("input keyboard repeat_rate must be positive");
        }
        if self.input.keyboard.repeat_delay < 0 {
            bail!("input keyboard repeat_delay must not be negative");
        }
        if self
            .input
            .mouse
            .sensitivity
            .is_some_and(|value| !value.is_finite() || !(-1.0..=1.0).contains(&value))
        {
            bail!("input mouse sensitivity must be between -1.0 and 1.0");
        }
        if self.layout.gap < 0 || self.layout.outer_gap < 0 {
            bail!("gaps must not be negative");
        }
        if !(0.1..=0.9).contains(&self.layout.master_factor) {
            bail!("master_factor must be between 0.1 and 0.9");
        }
        if self.layout.master_count == 0 {
            bail!("master_count must be at least 1");
        }
        if self.general.terminal.trim().is_empty() {
            bail!("terminal must not be empty");
        }
        if !(1..=9).contains(&self.general.tags) {
            bail!("general tags must be between 1 and 9");
        }
        if self
            .appearance
            .background
            .iter()
            .any(|v| !(0.0..=1.0).contains(v))
        {
            bail!("background components must be between 0.0 and 1.0");
        }
        if !(1..=32).contains(&self.appearance.focus_border_width) {
            bail!("focus_border_width must be between 1 and 32 logical pixels");
        }
        if !(14..=128).contains(&self.bar.height) {
            bail!("bar height must be between 14 and 128 logical pixels");
        }
        if self.bar.font.trim().is_empty() {
            bail!("bar font must not be empty");
        }
        if !(8.0..=64.0).contains(&self.bar.font_size) {
            bail!("bar font_size must be between 8 and 64 logical pixels");
        }
        if !(1..=10).contains(&self.bar.max_window_indicators) {
            bail!("bar max_window_indicators must be between 1 and 10");
        }
        if self.bar.refresh_interval_ms < 100 {
            bail!("bar refresh_interval_ms must be at least 100");
        }
        if self.bar.network.enabled {
            if self.bar.network.icon_font.trim().is_empty() {
                bail!("bar.network.icon_font must not be empty when network icon is enabled");
            }
            if self.bar.network.click_command.trim().is_empty() {
                bail!("bar.network.click_command must not be empty when network icon is enabled");
            }
            if self.bar.network.refresh_interval_ms < 1_000 {
                bail!("bar.network.refresh_interval_ms must be at least 1000");
            }
        }
        for color in [
            &self.bar.background,
            &self.bar.foreground,
            &self.bar.selected_background,
            &self.bar.selected_foreground,
            &self.bar.output_focus_color,
            &self.bar.occupied,
        ] {
            parse_named_hex_color("bar color", color)?;
        }
        if self.floating.default_width <= 0 || self.floating.default_height <= 0 {
            bail!("floating default_width and default_height must be positive");
        }
        for (index, rule) in self.window_rules.iter().enumerate() {
            if rule.app_id.is_none() && rule.title.is_none() {
                bail!(
                    "window_rules entry {} needs at least app_id or title",
                    index + 1
                );
            }
        }
        let mut output_names = HashSet::new();
        for output in &self.outputs {
            let name = output.name.trim();
            if name.is_empty() {
                bail!("outputs entries need a non-empty name");
            }
            if !output_names.insert(name) {
                bail!("duplicate outputs entry for {name}");
            }
            if let Some(scale) = output.scale {
                if !scale.is_finite() || !(0.25..=8.0).contains(&scale) {
                    bail!("output {name} scale must be between 0.25 and 8.0");
                }
            }
        }
        parse_hex_color(&self.appearance.focus_border_color)?;
        Ok(())
    }

    /// Returns the static overrides for a connector, if the user named it in the configuration.
    pub fn output(&self, name: &str) -> Option<&OutputConfig> {
        self.outputs.iter().find(|output| output.name == name)
    }

    /// Resolves the final floating state from protocol metadata and ordered user rules.
    ///
    /// Parented xdg-toplevels start as dialogs when that automatic policy is enabled. Rules are
    /// then evaluated from top to bottom and every match replaces the current result. “Last match
    /// wins” makes broad application rules easy to refine with a later, title-specific exception.
    pub fn window_should_float(
        &self,
        app_id: Option<&str>,
        title: Option<&str>,
        is_dialog: bool,
    ) -> bool {
        let mut floating = self.floating.dialogs && is_dialog;
        for rule in &self.window_rules {
            if rule.matches(app_id, title) {
                floating = rule.floating;
            }
        }
        floating
    }
}

impl WindowRule {
    fn matches(&self, app_id: Option<&str>, title: Option<&str>) -> bool {
        self.app_id
            .as_deref()
            .is_none_or(|pattern| app_id.is_some_and(|value| wildcard_match(pattern, value)))
            && self
                .title
                .as_deref()
                .is_none_or(|pattern| title.is_some_and(|value| wildcard_match(pattern, value)))
    }
}

/// Matches a case-insensitive glob containing `*` wildcards.
///
/// Window rules deliberately support only one metacharacter. It covers practical app-id and title
/// rules while avoiding a regex language (and another dependency) in a startup-critical config.
fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let value: Vec<char> = value.to_lowercase().chars().collect();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;

    for token in pattern {
        let mut current = vec![false; value.len() + 1];
        if token == '*' {
            current[0] = previous[0];
            for index in 1..=value.len() {
                current[index] = previous[index] || current[index - 1];
            }
        } else {
            for index in 1..=value.len() {
                current[index] = previous[index - 1] && token == value[index - 1];
            }
        }
        previous = current;
    }
    previous[value.len()]
}

/// Converts a configuration color into the normalized RGBA format expected by Smithay.
///
/// Six-digit colors are opaque. Eight-digit colors accept an explicit alpha component, which is
/// useful for a subtler border without coupling this platform-independent module to renderer types.
pub fn parse_hex_color(value: &str) -> Result<[f32; 4]> {
    parse_named_hex_color("focus_border_color", value)
}

/// Shared color parser used by focus borders and the optional bar.
pub fn parse_named_hex_color(name: &str, value: &str) -> Result<[f32; 4]> {
    let hex = value
        .strip_prefix('#')
        .ok_or_else(|| anyhow::anyhow!("{name} must start with '#': {value}"))?;
    if hex.len() != 6 && hex.len() != 8 {
        bail!("{name} must use #RRGGBB or #RRGGBBAA notation");
    }

    let component = |offset: usize| -> Result<f32> {
        let byte = u8::from_str_radix(&hex[offset..offset + 2], 16)
            .with_context(|| format!("invalid hex color component in {value}"))?;
        Ok(f32::from(byte) / 255.0)
    };

    Ok([
        component(0)?,
        component(2)?,
        component(4)?,
        if hex.len() == 8 { component(6)? } else { 1.0 },
    ])
}

fn default_path() -> Option<PathBuf> {
    // Follow the XDG base-directory specification first. HOME is only the conventional fallback
    // for environments that do not export XDG_CONFIG_HOME (for example a small TTY login).
    if let Some(base) = env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(base).join("anvil/config.toml"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/anvil/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_uses_defaults() {
        let config: Config = toml::from_str("[layout]\ngap = 3\n").unwrap();
        assert_eq!(config.layout.gap, 3);
        assert_eq!(config.layout.master_count, 1);
        assert_eq!(config.general.terminal, "alacritty");
        assert_eq!(config.general.tags, 4);
        assert!(!config.compat.xwayland);
        assert_eq!(config.input.keyboard.repeat_rate, 25);
        assert_eq!(config.input.keyboard.repeat_delay, 200);
        assert_eq!(config.input.mouse.sensitivity, None);
        assert_eq!(config.input.touchpad.tap, None);
        assert!(!config.appearance.client_side_decorations);
        assert_eq!(config.appearance.focus_border_color, "#707070");
        assert_eq!(config.bar.height, 22);
        assert_eq!(config.bar.max_window_indicators, 5);
        assert!(config.floating.dialogs);
        assert_eq!(config.floating.default_width, 800);
        assert!(config.outputs.is_empty());
    }

    #[test]
    fn network_widget_is_opt_in_and_has_checked_configuration() {
        let defaults = Config::default();
        assert!(!defaults.bar.network.enabled);
        assert_eq!(defaults.bar.network.click_command, "alacritty -e nmtui-connect");
        let example: Config = toml::from_str(
            "[bar.network]\nenabled = true\nicon_font = 'Symbols Nerd Font Mono'\nclick_command = 'alacritty -e nmtui-connect'\n",
        )
        .unwrap();
        assert!(example.validate().is_ok());
        let mut broken = example.clone();
        broken.bar.network.click_command.clear();
        assert!(broken.validate().is_err());
        assert!(toml::from_str::<Config>("[bar.network]\nunknwon = true").is_err());
    }

    #[test]
    fn media_commands_default_disabled_and_parse_explicitly() {
        let defaults = Config::default();
        assert!(defaults.media.volume_up.is_empty());
        assert!(defaults.media.brightness_down.is_empty());

        let configured: Config = toml::from_str(
            "[media]\nvolume_up = 'kova-osd volume up'\nbrightness_down = 'kova-osd brightness down'\n",
        )
        .unwrap();
        assert_eq!(configured.media.volume_up, "kova-osd volume up");
        assert_eq!(configured.media.brightness_down, "kova-osd brightness down");
        assert!(configured.validate().is_ok());
        assert!(toml::from_str::<Config>("[media]\nvolme_up='oops'").is_err());
    }

    #[test]
    fn parses_input_configuration() {
        let config: Config = toml::from_str(
            r#"
                [input.keyboard]
                layout = "de"
                variant = "nodeadkeys"
                repeat_rate = 35
                repeat_delay = 180

                [input.mouse]
                accel_profile = "flat"
                sensitivity = -0.25

                [input.touchpad]
                tap = true
                natural_scroll = true
            "#,
        )
        .unwrap();
        assert_eq!(config.input.keyboard.layout, "de");
        assert_eq!(config.input.keyboard.variant, "nodeadkeys");
        assert_eq!(config.input.keyboard.repeat_rate, 35);
        assert_eq!(config.input.keyboard.repeat_delay, 180);
        assert_eq!(config.input.mouse.accel_profile, Some(AccelProfile::Flat));
        assert_eq!(config.input.mouse.sensitivity, Some(-0.25));
        assert_eq!(config.input.touchpad.tap, Some(true));
        assert_eq!(config.input.touchpad.natural_scroll, Some(true));
    }

    #[test]
    fn parses_optional_xwayland_configuration() {
        let config: Config = toml::from_str("[compat]\nxwayland = true\n").unwrap();
        assert!(config.compat.xwayland);
    }

    #[test]
    fn rejects_invalid_input_values() {
        let mut config = Config::default();
        config.input.keyboard.repeat_rate = 0;
        assert!(config.validate().is_err());
        config.input.keyboard.repeat_rate = 25;
        config.input.mouse.sensitivity = Some(1.1);
        assert!(config.validate().is_err());
    }

    #[test]
    fn parses_output_configuration() {
        let config: Config = toml::from_str(
            r#"
                [[outputs]]
                name = "DP-1"
                mode = "2560x1440@143.97"
                position = [-2560, 120]
                scale = 1.25
                transform = "90"
            "#,
        )
        .unwrap();
        let output = config.output("DP-1").unwrap();
        assert_eq!(
            output.mode,
            Some(OutputMode {
                width: 2560,
                height: 1440,
                refresh_millihz: 143_970,
            })
        );
        assert_eq!(output.position, Some([-2560, 120]));
        assert_eq!(output.scale, Some(1.25));
        assert_eq!(output.transform, Some(OutputTransform::Rotate90));

        let flipped: Config =
            toml::from_str("[[outputs]]\nname='HDMI-A-1'\ntransform='flipped-270'").unwrap();
        assert_eq!(
            flipped.outputs[0].transform,
            Some(OutputTransform::Flipped270)
        );
    }

    #[test]
    fn rejects_duplicate_outputs_and_invalid_scale() {
        let mut config: Config = toml::from_str(
            r#"
                [[outputs]]
                name = "DP-1"
                [[outputs]]
                name = "DP-1"
            "#,
        )
        .unwrap();
        assert!(config.validate().is_err());
        config.outputs.pop();
        config.outputs[0].scale = Some(0.0);
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_malformed_output_mode() {
        assert!(toml::from_str::<Config>("[[outputs]]\nname='DP-1'\nmode='1920x1080'").is_err());
        assert!(toml::from_str::<Config>("[[outputs]]\nname='DP-1'\nmode='0x1080@60'").is_err());
    }

    #[test]
    fn rejects_invalid_factor() {
        let mut config = Config::default();
        config.layout.master_factor = 1.0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn parses_rgb_and_rgba_hex_colors() {
        assert_eq!(
            parse_hex_color("#ff8000").unwrap(),
            [1.0, 128.0 / 255.0, 0.0, 1.0]
        );
        assert_eq!(
            parse_hex_color("#00000080").unwrap(),
            [0.0, 0.0, 0.0, 128.0 / 255.0]
        );
        assert!(parse_hex_color("707070").is_err());
        assert!(parse_hex_color("#xyzxyz").is_err());
    }

    #[test]
    fn window_rules_match_case_insensitive_wildcards() {
        let config: Config = toml::from_str(
            r#"
                [[window_rules]]
                app_id = "steam"
                title = "*settings*"
                floating = true
            "#,
        )
        .unwrap();
        assert!(config.window_should_float(Some("Steam"), Some("Controller Settings"), false));
        assert!(!config.window_should_float(Some("foot"), Some("Settings"), false));
    }

    #[test]
    fn later_rule_can_tile_an_automatic_dialog() {
        let config: Config = toml::from_str(
            r#"
                [[window_rules]]
                app_id = "editor"
                floating = false
            "#,
        )
        .unwrap();
        assert!(!config.window_should_float(Some("editor"), Some("Open"), true));
    }
}
