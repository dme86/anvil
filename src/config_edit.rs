//! Validated, atomic writes for anvilctl's local configuration editor.
//!
//! The file is never truncated in place. Edits are validated against the same Config model
//! as compositor startup, and published only after the replacement is complete and synced.

use std::{
    fs::{self, File, OpenOptions, Permissions},
    io::{ErrorKind, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table};

use crate::config::Config;

/// Validate an edited TOML document, then safely install it.
/// When overwrite is false, an existing config cannot be replaced even in a race.
pub fn write_validated_config(path: &Path, contents: &str, overwrite: bool) -> Result<()> {
    write_with_hook(path, contents, overwrite, || Ok(()))
}

/// Hook is used by tests to simulate interrupted writes before the atomic publish step.
fn write_with_hook(
    path: &Path,
    contents: &str,
    overwrite: bool,
    before_publish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let config: Config = toml::from_str(contents)
        .with_context(|| format!("invalid edited TOML for {}", path.display()))?;
    config
        .validate()
        .with_context(|| format!("invalid edited config {}", path.display()))?;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create config directory {}", parent.display()))?;

    let permissions = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!(
                    "refusing to replace symlinked config {} (edit its target directly)",
                    path.display()
                );
            }
            if !overwrite {
                bail!(
                    "config {} already exists; use --force to replace it",
                    path.display()
                );
            }
            Some(metadata.permissions())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("cannot stat config {}", path.display()));
        }
    };

    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let name = path
        .file_name()
        .context("config path has no filename")?
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), timestamp));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .with_context(|| format!("cannot create temporary config {}", temporary.display()))?;
        file.set_permissions(permissions.unwrap_or_else(|| Permissions::from_mode(0o600)))?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);

        before_publish()?;

        if overwrite {
            fs::rename(&temporary, path)
                .with_context(|| format!("cannot atomically replace config {}", path.display()))?;
        } else {
            fs::hard_link(&temporary, path).with_context(|| {
                format!(
                    "cannot create config {} (it may already exist)",
                    path.display()
                )
            })?;
        }
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    write_result
}

/// Return the stored TOML value at a dotted key path, without synthesizing defaults.
pub fn get_config_key(contents: &str, key: &str) -> Result<String> {
    let keys = key_segments(key)?;
    let doc: DocumentMut = contents
        .parse()
        .context("cannot parse current config TOML")?;
    let mut current = doc.as_item();
    for part in keys {
        current = current
            .get(part)
            .with_context(|| format!("config key not found: {key}"))?;
    }
    if !current.is_value() {
        bail!("config path {key} identifies a table, not a value");
    }
    let mut value = current
        .as_value()
        .context("config value unexpectedly missing")?
        .clone();
    *value.decor_mut() = Default::default();
    Ok(value.to_string())
}

/// Edit a dotted key in place, preserving unrelated comments and the original TOML order.
/// A missing intermediate table is created for set; unset requires an existing key.
pub fn edit_config_key(contents: &str, key: &str, new_value: Option<&str>) -> Result<String> {
    let mut keys = key_segments(key)?;
    let field = keys.pop().context("config key must not be empty")?;
    let mut doc: DocumentMut = contents
        .parse()
        .context("cannot parse current config TOML")?;
    let mut parent = doc.as_table_mut();

    for part in keys {
        if parent.get(part).is_none() {
            if new_value.is_none() {
                bail!("config key not found: {key}");
            }
            parent.insert(part, Item::Table(Table::new()));
        }
        parent = parent
            .get_mut(part)
            .and_then(Item::as_table_mut)
            .with_context(|| format!("config path {key} traverses a non-table at {part}"))?;
    }

    if let Some(value) = new_value {
        // Accept TOML literals (integers, floats, booleans, arrays, quoted strings) and also
        // shell-friendly unquoted strings such as de, alacritty and #707070.
        let snippet = format!("replacement = {value}\n");
        let mut replacement = match snippet.parse::<DocumentMut>() {
            Ok(parsed) => parsed["replacement"].clone(),
            Err(_) => toml_edit::value(value),
        };
        // Preserve the edited item's trailing inline comment and spacing where possible.
        if let Some(decor) = parent
            .get(field)
            .and_then(Item::as_value)
            .map(|existing| existing.decor().clone())
        {
            if let Some(new) = replacement.as_value_mut() {
                *new.decor_mut() = decor;
            }
        }
        parent.insert(field, replacement);
    } else if parent.remove(field).is_none() {
        bail!("config key not found: {key}");
    }

    let edited = doc.to_string();
    let config: Config = toml::from_str(&edited)
        .with_context(|| format!("config key {key} produces invalid TOML or an unknown setting"))?;
    config
        .validate()
        .with_context(|| format!("config key {key} fails validation"))?;
    Ok(edited)
}

fn key_segments(key: &str) -> Result<Vec<&str>> {
    let segments: Vec<&str> = key.split('.').collect();
    if segments.iter().any(|part| {
        part.is_empty()
            || !part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    }) {
        bail!("invalid dotted config key: {key}");
    }
    Ok(segments)
}

#[derive(Default)]
pub struct OutputChanges<'a> {
    pub mode: Option<&'a str>,
    pub position: Option<[i32; 2]>,
    pub scale: Option<f64>,
    pub transform: Option<&'a str>,
}

/// Update the matching [[outputs]] table without rewriting unrelated TOML.
pub fn edit_output_config(
    contents: &str,
    name: &str,
    changes: Option<OutputChanges<'_>>,
) -> Result<String> {
    if name.trim().is_empty() {
        bail!("output name must not be empty");
    }
    let mut document: DocumentMut = contents.parse().context("invalid config TOML")?;
    let original: Config = toml::from_str(contents)?;
    original.validate()?;

    if document.get("outputs").is_none() {
        if changes.is_none() {
            return Ok(contents.to_owned());
        }
        document["outputs"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    let tables = document["outputs"]
        .as_array_of_tables_mut()
        .context("outputs must be an array of tables")?;
    let matches: Vec<usize> = tables
        .iter()
        .enumerate()
        .filter_map(|(index, table)| {
            (table.get("name").and_then(Item::as_str) == Some(name)).then_some(index)
        })
        .collect();
    if matches.len() > 1 {
        bail!("duplicate outputs entry for {name}");
    }
    match changes {
        None => {
            if let Some(index) = matches.first() {
                tables.remove(*index);
            }
        }
        Some(changes) => {
            if let Some(mode) = changes.mode {
                mode.parse::<crate::config::OutputMode>()
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
            }
            if let Some(scale) = changes.scale {
                if !scale.is_finite() || !(0.25..=8.0).contains(&scale) {
                    bail!("output scale must be between 0.25 and 8.0");
                }
            }
            if let Some(transform) = changes.transform {
                if !matches!(
                    transform,
                    "normal"
                        | "90"
                        | "180"
                        | "270"
                        | "flipped"
                        | "flipped-90"
                        | "flipped-180"
                        | "flipped-270"
                ) {
                    bail!("invalid output transform: {transform}");
                }
            }
            if matches.is_empty() {
                let mut table = Table::new();
                table["name"] = toml_edit::value(name);
                tables.push(table);
            }
            let index = matches.first().copied().unwrap_or(tables.len() - 1);
            let table = tables.get_mut(index).context("missing output table")?;
            if let Some(mode) = changes.mode {
                table["mode"] = toml_edit::value(mode);
            }
            if let Some([x, y]) = changes.position {
                let mut array = Array::new();
                array.push(i64::from(x));
                array.push(i64::from(y));
                table["position"] = toml_edit::value(array);
            }
            if let Some(scale) = changes.scale {
                table["scale"] = toml_edit::value(scale);
            }
            if let Some(transform) = changes.transform {
                table["transform"] = toml_edit::value(transform);
            }
        }
    }
    let edited = document.to_string();
    let parsed: Config = toml::from_str(&edited)
        .context("edited output configuration has unknown or invalid settings")?;
    parsed.validate()?;
    Ok(edited)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manages_outputs_without_duplicates_or_losing_comments() {
        let original = "# header\n[layout]\ngap = 8 # keep\n";
        let configured = edit_output_config(
            original,
            "DP-1",
            Some(OutputChanges {
                mode: Some("2560x1440@144"),
                position: Some([-1920, 0]),
                scale: Some(1.25),
                transform: Some("90"),
            }),
        )
        .unwrap();
        assert!(configured.contains("# header"));
        assert!(configured.contains("gap = 8 # keep"));
        let changed = edit_output_config(
            &configured,
            "DP-1",
            Some(OutputChanges {
                scale: Some(1.5),
                ..Default::default()
            }),
        )
        .unwrap();
        let parsed: Config = toml::from_str(&changed).unwrap();
        assert_eq!(parsed.outputs.len(), 1);
        assert_eq!(parsed.outputs[0].scale, Some(1.5));
        assert_eq!(parsed.outputs[0].position, Some([-1920, 0]));
        assert!(
            edit_output_config(
                &changed,
                "DP-1",
                Some(OutputChanges {
                    mode: Some("1920x1080"),
                    ..Default::default()
                }),
            )
            .is_err()
        );
        assert!(
            edit_output_config(
                &changed,
                "DP-1",
                Some(OutputChanges {
                    transform: Some("sideways"),
                    ..Default::default()
                }),
            )
            .is_err()
        );
        let reset = edit_output_config(&changed, "DP-1", None).unwrap();
        assert!(toml::from_str::<Config>(&reset).unwrap().outputs.is_empty());
        assert!(reset.contains("# header"));
    }

    #[test]
    fn edit_dotted_keys_and_preserve_existing_comments() {
        let original =
            "# retain this\n[layout]\ngap = 8 # keep this too\n[general]\nterminal = 'alacritty'\n";
        assert_eq!(get_config_key(original, "layout.gap").unwrap(), "8");
        let edited = edit_config_key(original, "layout.gap", Some("12")).unwrap();
        assert!(edited.contains("# retain this"));
        assert!(edited.contains("# keep this too"));
        assert!(edited.contains("terminal = 'alacritty'"));
        assert_eq!(get_config_key(&edited, "layout.gap").unwrap(), "12");
        let nested = edit_config_key(&edited, "input.keyboard.layout", Some("de")).unwrap();
        assert_eq!(
            get_config_key(&nested, "input.keyboard.layout").unwrap(),
            "\"de\""
        );
        let removed = edit_config_key(&nested, "input.keyboard.layout", None).unwrap();
        assert!(get_config_key(&removed, "input.keyboard.layout").is_err());
    }

    #[test]
    fn rejects_invalid_paths_and_nonexistent_keys() {
        let sample = "[layout]\ngap = 8\n";
        assert!(get_config_key(sample, "layout").is_err());
        assert!(get_config_key(sample, "layout.missing").is_err());
        assert!(edit_config_key(sample, "layout.bad.key", Some("2")).is_err());
        assert!(edit_config_key(sample, "layout.missing", None).is_err());
        assert!(edit_config_key(sample, "layout..gap", Some("3")).is_err());
    }

    fn test_config_path() -> std::path::PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "anvil-config-write-{}-{timestamp}/config.toml",
            std::process::id()
        ))
    }

    #[test]
    fn preserves_existing_file_on_invalid_edits_and_rejected_overwrite() {
        let path = test_config_path();
        write_validated_config(&path, "[layout]\ngap = 4 # leave intact\n", false).unwrap();
        assert!(write_validated_config(&path, "[layout]\ngap = -2\n", true).is_err());
        assert!(write_validated_config(&path, "[layout]\nbad_key = 5\n", true).is_err());
        assert!(write_validated_config(&path, "[layout]\ngap = 5\n", false).is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[layout]\ngap = 4 # leave intact\n"
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn simulated_interruption_before_publish_leaves_original_untouched() {
        let path = test_config_path();
        write_validated_config(&path, "[layout]\ngap = 3\n", false).unwrap();
        let error = write_with_hook(&path, "[layout]\ngap = 7\n", true, || {
            bail!("simulated interrupted write")
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("simulated interrupted write"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "[layout]\ngap = 3\n");
        let files = fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(files, 1, "temporary write must be cleaned up");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn replacement_preserves_permissions_and_unrelated_text() {
        let path = test_config_path();
        write_validated_config(&path, "# note\n[layout]\ngap = 3\n", false).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
        write_validated_config(&path, "# note\n[layout]\ngap = 8\n", true).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# note\n[layout]\ngap = 8\n"
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
