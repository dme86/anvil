//! Small command-line client for Anvil's local Unix-socket control protocol.
//!
//! Configuration inspection and initialization deliberately work without a compositor session.

use std::{
    env, fs,
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

use anvil::{
    config::{Config, config_path},
    config_edit::{edit_config_key, get_config_key, write_validated_config},
    ipc::{PROTOCOL_VERSION, Request, Response, SOCKET_NAME},
};
use anyhow::{Context, Result, bail};

#[cfg(test)]
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_CONFIG: &str = include_str!("../../config.toml");

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    if args.next().as_deref() == Some("config") {
        return run_config(args);
    }

    let response = send_request(parse_request()?)?;
    print_response(response)
}

fn print_response(response: Response) -> Result<()> {
    let failed = matches!(response, Response::Error { .. });
    println!("{}", serde_json::to_string_pretty(&response)?);
    if failed {
        bail!("Anvil rejected the request");
    }
    Ok(())
}

fn send_request(request: Request) -> Result<Response> {
    let socket = socket_path()?;
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("cannot connect to Anvil at {}", socket.display()))?;
    serde_json::to_writer(&mut stream, &request).context("cannot encode request")?;
    stream.write_all(b"\n")?;
    stream.shutdown(Shutdown::Write)?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    serde_json::from_str(&response).context("invalid Anvil response")
}

fn run_config(mut args: impl Iterator<Item = String>) -> Result<()> {
    let usage = "usage: anvilctl config <path|init|show|validate|get|set|unset> [ARGS] [--config FILE] [--reload]";
    let command = args.next().context(usage)?;
    if !matches!(
        command.as_str(),
        "path" | "init" | "show" | "validate" | "get" | "set" | "unset"
    ) {
        bail!("unknown config command: {command}; {usage}");
    }

    let mut explicit_path = None;
    let mut force = false;
    let mut reload = false;
    let mut positionals = Vec::new();
    while let Some(option) = args.next() {
        match option.as_str() {
            "--config" if explicit_path.is_none() => {
                explicit_path = Some(PathBuf::from(
                    args.next().context("--config requires a file path")?,
                ));
            }
            "--force" if command == "init" && !force => force = true,
            "--reload" if matches!(command.as_str(), "set" | "unset") && !reload => reload = true,
            _ if option.starts_with("--") => {
                bail!("unexpected option for config {command}: {option}")
            }
            _ => positionals.push(option),
        }
    }
    let expected = match command.as_str() {
        "get" | "unset" => 1,
        "set" => 2,
        _ => 0,
    };
    if positionals.len() != expected {
        bail!("config {command} expects {expected} arguments; {usage}");
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
            let contents = read_config_text(&path)?;
            print!("{contents}");
        }
        "validate" => {
            Config::load(Some(&path))?;
            println!("valid config: {}", path.display());
        }
        "get" => {
            let contents = read_config_text(&path)?;
            println!("{}", get_config_key(&contents, &positionals[0])?);
        }
        "set" | "unset" => {
            let contents = read_config_text(&path)?;
            let edited = edit_config_key(
                &contents,
                &positionals[0],
                if command == "set" {
                    Some(positionals[1].as_str())
                } else {
                    None
                },
            )?;
            write_validated_config(&path, &edited, true)?;
            println!("updated config: {}", path.display());
            if reload {
                let response = send_request(Request::Reload {
                    version: PROTOCOL_VERSION,
                })
                .context("config saved, but live reload failed")?;
                print_response(response).context("config saved, but live reload was rejected")?;
            }
        }
        _ => unreachable!("known config command"),
    }
    Ok(())
}

fn read_config_text(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| {
        format!(
            "cannot read config {} (run anvilctl config init first)",
            path.display()
        )
    })
}

fn initialize_config(path: &Path, force: bool) -> Result<()> {
    write_validated_config(path, DEFAULT_CONFIG, force)
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
            "usage: anvilctl window list | spawn PROGRAM [ARG ...] | reload | debug stats | config <path|init|show|validate|get|set|unset>"
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
