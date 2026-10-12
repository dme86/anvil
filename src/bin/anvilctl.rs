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
    config_edit::{
        OutputChanges, edit_config_key, edit_output_config, get_config_key, write_validated_config,
    },
    ipc::{PROTOCOL_VERSION, Request, Response, SOCKET_NAME},
};
use anyhow::{Context, Result, bail};

#[cfg(test)]
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_CONFIG: &str = include_str!("../../config.toml");

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("config") => return run_config(args),
        Some("output") => return run_output(args),
        _ => {}
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

fn run_output(mut args: impl Iterator<Item = String>) -> Result<()> {
    let usage = "usage: anvilctl output list | configure NAME [--mode WxH@Hz] [--position X,Y] [--scale FLOAT] [--transform NAME] [--reload] [--config FILE] | reset NAME [--reload] [--config FILE]";
    let command = args.next().context(usage)?;
    if command == "list" {
        if args.next().is_some() {
            bail!("{usage}");
        }
        return print_response(send_request(Request::OutputList {
            version: PROTOCOL_VERSION,
        })?);
    }
    if !matches!(command.as_str(), "configure" | "reset") {
        bail!("{usage}");
    }
    let name = args.next().context(usage)?;
    if name.starts_with("--") || name.trim().is_empty() {
        bail!("{usage}");
    }

    let mut path = None;
    let mut reload = false;
    let mut mode = None;
    let mut position = None;
    let mut scale = None;
    let mut transform = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--config" if path.is_none() => {
                path = Some(PathBuf::from(args.next().context("--config needs a path")?));
            }
            "--reload" if !reload => reload = true,
            "--mode" if command == "configure" && mode.is_none() => {
                mode = Some(args.next().context("--mode needs WIDTHxHEIGHT@HZ")?);
            }
            "--position" if command == "configure" && position.is_none() => {
                let pair = args.next().context("--position needs X,Y")?;
                let (x, y) = pair.split_once(',').context("--position needs X,Y")?;
                position = Some([
                    x.parse().context("invalid X position")?,
                    y.parse().context("invalid Y position")?,
                ]);
            }
            "--scale" if command == "configure" && scale.is_none() => {
                scale = Some(
                    args.next()
                        .context("--scale needs a number")?
                        .parse::<f64>()
                        .context("invalid scale")?,
                );
            }
            "--transform" if command == "configure" && transform.is_none() => {
                transform = Some(args.next().context("--transform needs a name")?);
            }
            _ => bail!("unexpected output option: {flag}; {usage}"),
        }
    }
    if command == "configure"
        && mode.is_none()
        && position.is_none()
        && scale.is_none()
        && transform.is_none()
    {
        bail!("output configure requires at least one setting");
    }
    let path = path
        .or_else(config_path)
        .context("cannot locate config (set HOME or XDG_CONFIG_HOME)")?;
    let original = read_config_text(&path)?;
    let changes = (command == "configure").then_some(OutputChanges {
        mode: mode.as_deref(),
        position,
        scale,
        transform: transform.as_deref(),
    });
    let edited = edit_output_config(&original, &name, changes)?;
    if edited != original {
        write_validated_config(&path, &edited, true)?;
    }
    println!("output {name} configuration saved: {}", path.display());
    if reload {
        let response = send_request(Request::Reload {
            version: PROTOCOL_VERSION,
        })
        .context("output config saved, but live reload failed")?;
        print_response(response).context("output config saved, but live reload was rejected")?;
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
            "usage: anvilctl window list | spawn PROGRAM [ARG ...] | output <list|configure|reset> | reload | debug stats | config <path|init|show|validate|get|set|unset>"
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
