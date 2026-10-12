//! Small command-line client for Anvil's local Unix-socket control protocol.
//!
//! Configuration inspection and initialization deliberately work without a compositor session.

use std::{
    env, fs,
    fs::{File, OpenOptions},
    io::{Read, Write},
    net::Shutdown,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anvil::{
    config::{Config, config_path},
    ipc::{PROTOCOL_VERSION, Request, Response, SOCKET_NAME},
};
use anyhow::{Context, Result, bail};

const DEFAULT_CONFIG: &str = include_str!("../../config.toml");

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    if args.next().as_deref() == Some("config") {
        return run_config(args);
    }

    let request = parse_request()?;
    let socket = socket_path()?;
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("cannot connect to Anvil at {}", socket.display()))?;
    serde_json::to_writer(&mut stream, &request).context("cannot encode request")?;
    stream.write_all(b"\n")?;
    // Anvil handles exactly one request per connection. Closing only the write half makes that
    // boundary explicit while keeping the read half open for the response.
    stream.shutdown(Shutdown::Write)?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let response: Response = serde_json::from_str(&response).context("invalid Anvil response")?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    if matches!(response, Response::Error { .. }) {
        std::process::exit(1);
    }
    Ok(())
}

fn run_config(mut args: impl Iterator<Item = String>) -> Result<()> {
    let command = args.next().context(
        "usage: anvilctl config <path|init|show|validate> [--config FILE] [--force (init only)]",
    )?;
    if !matches!(command.as_str(), "path" | "init" | "show" | "validate") {
        bail!("unknown config command: {command} (expected path, init, show or validate)");
    }

    let mut explicit_path = None;
    let mut force = false;
    while let Some(option) = args.next() {
        match option.as_str() {
            "--config" if explicit_path.is_none() => {
                explicit_path = Some(PathBuf::from(
                    args.next().context("--config requires a file path")?,
                ));
            }
            "--force" if command == "init" && !force => force = true,
            _ => bail!("unexpected option for config {command}: {option}"),
        }
    }
    let path = explicit_path.or_else(config_path).context(
        "cannot determine Anvil config path: set XDG_CONFIG_HOME or HOME, or pass --config FILE",
    )?;
    match command.as_str() {
        "path" => println!("{}", path.display()),
        "init" => {
            initialize_config(&path, force)?;
            println!("initialized {}", path.display());
        }
        "show" => {
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("cannot read config {}", path.display()))?;
            print!("{contents}");
        }
        "validate" => {
            Config::load(Some(&path))?;
            println!("valid config: {}", path.display());
        }
        _ => unreachable!("known config command"),
    }
    Ok(())
}

/// Install the checked-in example verbatim so its comments and formatting are preserved.
/// Create-new uses an exclusive hard link; --force uses an atomic same-directory rename.
/// In either case a complete and fsynced temporary file is prepared before the final write.
fn initialize_config(path: &Path, force: bool) -> Result<()> {
    let config: Config =
        toml::from_str(DEFAULT_CONFIG).context("built-in default configuration is invalid TOML")?;
    config
        .validate()
        .context("built-in default configuration failed validation")?;

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create config directory {}", parent.display()))?;
    if !force && path.exists() {
        bail!(
            "config {} already exists; use --force to replace it",
            path.display()
        );
    }

    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let basename = path
        .file_name()
        .context("config path has no filename")?
        .to_string_lossy();
    let temp = parent.join(format!(
        ".{basename}.{}.{}.tmp",
        std::process::id(),
        timestamp
    ));
    let saved_permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("cannot create temporary config {}", temp.display()))?;
        let permissions = saved_permissions.unwrap_or_else(|| fs::Permissions::from_mode(0o600));
        file.set_permissions(permissions)?;
        file.write_all(DEFAULT_CONFIG.as_bytes())?;
        file.sync_all()?;
        drop(file);

        if force {
            fs::rename(&temp, path)
                .with_context(|| format!("cannot replace config {}", path.display()))?;
        } else {
            fs::hard_link(&temp, path).with_context(|| {
                format!(
                    "cannot initialize config {} (already exists or is unwritable)",
                    path.display()
                )
            })?;
        }
        // Persist the new directory entry when the filesystem supports directory fsync.
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    // A failed write must not leave an unused temp file; after success it is no longer needed.
    let _ = fs::remove_file(&temp);
    result
}

fn parse_request() -> Result<Request> {
    let mut args = env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (Some("window"), Some(action)) if action == "list" && args.next().is_none() => {
            Ok(Request::WindowList {
                version: PROTOCOL_VERSION,
            })
        }
        (Some("spawn"), Some(program)) => {
            let mut argv = vec![program];
            argv.extend(args);
            Ok(Request::Spawn {
                version: PROTOCOL_VERSION,
                argv,
            })
        }
        (Some("reload"), None) => Ok(Request::Reload {
            version: PROTOCOL_VERSION,
        }),
        (Some("debug"), Some(action)) if action == "stats" && args.next().is_none() => {
            Ok(Request::DebugStats {
                version: PROTOCOL_VERSION,
            })
        }
        _ => bail!(
            "usage: anvilctl window list | spawn PROGRAM [ARG ...] | reload | debug stats | config <path|init|show|validate>"
        ),
    }
}

fn socket_path() -> Result<PathBuf> {
    let runtime = env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(runtime).join(SOCKET_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_and_validates_without_anvil_running() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("anvilctl-init-{}-{unique}", std::process::id()));
        let path = dir.join("anvil/config.toml");

        initialize_config(&path, false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG);
        Config::load(Some(&path)).unwrap();

        fs::write(&path, "# keep me\n[layout]\ngap = 3\n").unwrap();
        assert!(initialize_config(&path, false).is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# keep me\n[layout]\ngap = 3\n"
        );

        initialize_config(&path, true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG);
        fs::remove_dir_all(dir).unwrap();
    }
}
