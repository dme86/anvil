//! Loading and validation of Anvil's single `config.toml` interface.
//!
//! The structs in this module mirror the TOML sections. Every section has defaults, which means a
//! user may override one value without copying the complete example configuration. Unknown keys
//! are rejected on purpose: a typo in a window manager config should fail loudly instead of being
//! silently ignored and leaving the user with surprising behavior.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Complete runtime configuration assembled from defaults and an optional TOML file.
pub struct Config {
    pub general: General,
    pub layout: Layout,
    pub appearance: Appearance,
    pub keys: Keys,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
/// Programs started by the compositor itself.
pub struct General {
    pub terminal: String,
    pub startup: Vec<String>,
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
    pub swap_master: String,
    pub master_grow: String,
    pub master_shrink: String,
    pub close: String,
}

impl Default for General {
    fn default() -> Self {
        Self {
            terminal: "foot".into(),
            startup: Vec::new(),
        }
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
        if self
            .appearance
            .background
            .iter()
            .any(|v| !(0.0..=1.0).contains(v))
        {
            bail!("background components must be between 0.0 and 1.0");
        }
        Ok(())
    }
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
        assert_eq!(config.general.terminal, "foot");
    }

    #[test]
    fn rejects_invalid_factor() {
        let mut config = Config::default();
        config.layout.master_factor = 1.0;
        assert!(config.validate().is_err());
    }
}
