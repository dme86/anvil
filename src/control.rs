//! Local Unix-socket control server backing the optional `anvilctl` binary.

use std::{
    fs,
    io::{self, ErrorKind, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    time::{Duration, Instant},
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
    // Unix streams preserve bytes, not individual writes. serde_json::to_writer may send
    // several fragments, so wait for the CLI's newline/EOF within a total time/size budget.
    let deadline = Instant::now() + Duration::from_millis(100);
    let request = read_request(|bytes| {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "request deadline exceeded",
            ));
        }
        stream.set_read_timeout(Some(remaining))?;
        stream.read(bytes)
    });
    let response = match request {
        Ok(bytes) if bytes.is_empty() => Response::Error {
            message: "empty request".into(),
        },
        Ok(bytes) => match serde_json::from_slice::<Request>(&bytes) {
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

/// Read one framed request independently of stream packet boundaries, capped at 64 KiB.
fn read_request(mut read: impl FnMut(&mut [u8]) -> io::Result<usize>) -> io::Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = match read(&mut chunk) {
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            return Ok(request);
        }
        let end = chunk[..count].iter().position(|&byte| byte == b'\n');
        let length = end.map_or(count, |index| index + 1);
        if request.len() + length > 64 * 1024 {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "request exceeds 64 KiB",
            ));
        }
        request.extend_from_slice(&chunk[..length]);
        if end.is_some() {
            return Ok(request);
        }
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
        Request::DebugStats { .. } => Response::Stats {
            stats: state.runtime_stats(),
        },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragmented_json_is_read_through_the_frame_boundary() {
        let bytes = b"{\"command\":\"window_list\",\"version\":1}\n";
        let mut fragments = bytes.iter();
        let request = read_request(|buffer| {
            if let Some(byte) = fragments.next() {
                buffer[0] = *byte;
                Ok(1)
            } else {
                panic!("reader must stop at the newline");
            }
        })
        .unwrap();
        assert_eq!(request, bytes);
        assert!(matches!(
            serde_json::from_slice::<Request>(&request).unwrap(),
            Request::WindowList { .. }
        ));
    }

    #[test]
    fn eof_remains_a_supported_request_boundary() {
        let mut reader = &b"{\"command\":\"debug_stats\",\"version\":1}"[..];
        let request = read_request(|buffer| reader.read(buffer)).unwrap();
        assert!(matches!(
            serde_json::from_slice::<Request>(&request).unwrap(),
            Request::DebugStats { .. }
        ));
    }

    #[test]
    fn oversized_unterminated_requests_are_rejected() {
        let error = read_request(|buffer| {
            buffer.fill(b'x');
            Ok(buffer.len())
        })
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }
}
