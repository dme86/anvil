//! Small command-line client for Anvil's local Unix-socket control protocol.

use std::{
    env,
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::PathBuf,
};

use anvil::ipc::{PROTOCOL_VERSION, Request, Response, SOCKET_NAME};
use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
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
        _ => bail!("usage: anvilctl window list | spawn PROGRAM [ARG ...] | reload"),
    }
}

fn socket_path() -> Result<PathBuf> {
    let runtime = env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(runtime).join(SOCKET_NAME))
}
