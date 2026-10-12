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

#[cfg(test)]
mod tests {
    use super::*;

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
