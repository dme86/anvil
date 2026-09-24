//! Local Unix-socket control server backing the optional `anvilctl` binary.

use std::{
    fs,
    io::{ErrorKind, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    time::Duration,
};

use anvil::ipc::{PROTOCOL_VERSION, Request, Response, SOCKET_NAME};
use anyhow::{Context, Result, bail};
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic};

use crate::{Anvil, CalloopData};

pub fn init(event_loop: &mut EventLoop<CalloopData>, state: &mut Anvil) -> Result<()> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    let path = PathBuf::from(runtime).join(SOCKET_NAME);
    if path.exists() {
        // Never unlink a live compositor's socket. A failed connection identifies a stale inode
        // left by an unclean shutdown, which is safe to replace for this same user runtime dir.
        if UnixStream::connect(&path).is_ok() {
            bail!(
                "another Anvil control socket is active at {}",
                path.display()
            );
        }
        fs::remove_file(&path)
            .with_context(|| format!("cannot remove stale socket {}", path.display()))?;
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("cannot bind control socket {}", path.display()))?;
    listener.set_nonblocking(true)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    state.control_socket_path = Some(path.clone());

    event_loop
        .handle()
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_, listener, data| {
                // SAFETY: Calloop owns the listener for the complete source lifetime and invokes
                // this callback serially, so no second mutable reference can exist.
                let listener = unsafe { listener.get_mut() };
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => handle_connection(stream, &mut data.state),
                        Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                        Err(error) => {
                            tracing::warn!(%error, "anvilctl accept failed");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .context("cannot register anvilctl socket")?;
    tracing::info!(path = %path.display(), "anvilctl socket ready");
    Ok(())
}

fn handle_connection(mut stream: UnixStream, state: &mut Anvil) {
    // CLI requests are intentionally tiny. A fixed cap prevents an accidental or malicious local
    // client from allocating unbounded compositor memory; one read is sufficient because the CLI
    // writes the complete request before shutting down its write half.
    // Accepted Linux sockets may block independently of the nonblocking listener. Bound the wait
    // so a same-user client that connects without sending cannot freeze compositor input forever.
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let mut bytes = vec![0; 64 * 1024];
    let response = match stream.read(&mut bytes) {
        Ok(0) => Response::Error {
            message: "empty request".into(),
        },
        Ok(length) => match serde_json::from_slice::<Request>(&bytes[..length]) {
            Ok(request) => dispatch(request, state),
            Err(error) => Response::Error {
                message: format!("invalid request: {error}"),
            },
        },
        Err(error) => Response::Error {
            message: format!("cannot read request: {error}"),
        },
    };
    if let Err(error) = serde_json::to_writer(&mut stream, &response)
        .and_then(|_| stream.write_all(b"\n").map_err(serde_json::Error::io))
    {
        tracing::warn!(%error, "cannot write anvilctl response");
    }
}

fn dispatch(request: Request, state: &mut Anvil) -> Response {
    if request.version() != PROTOCOL_VERSION {
        return Response::Error {
            message: format!(
                "protocol version {} is unsupported; expected {PROTOCOL_VERSION}",
                request.version()
            ),
        };
    }
    match request {
        Request::WindowList { .. } => Response::Windows {
            windows: state.control_window_list(),
        },
        Request::Spawn { argv, .. } => match state.spawn_argv(&argv) {
            Ok(pid) => Response::Spawned { pid },
            Err(error) => Response::Error {
                message: error.to_string(),
            },
        },
        Request::Reload { .. } => match state.reload_config() {
            Ok(path) => Response::Reloaded {
                config: path.map(|path| path.display().to_string()),
            },
            Err(error) => Response::Error {
                message: error.to_string(),
            },
        },
    }
}
