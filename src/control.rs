//! Local nonblocking Unix-socket control server backing optional `anvilctl`.

use std::{
    cell::Cell,
    fs,
    io::{self, ErrorKind, Read, Write},
    os::{
        fd::{AsFd, BorrowedFd},
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use anvil::ipc::{PROTOCOL_VERSION, Request, Response, SOCKET_NAME};
use anyhow::{Context, Result, bail};
use smithay::reexports::calloop::{
    self, Dispatcher, EventLoop, EventSource, Interest, LoopHandle, Mode, Poll, PostAction,
    Readiness, RegistrationToken, Token, TokenFactory,
    generic::{Generic, NoIoDrop},
    timer::{TimeoutAction, Timer},
};

use crate::{Anvil, CalloopData};

const MAX_REQUEST: usize = 64 * 1024;
const IO_BUDGET: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 64;
const ACCEPT_BUDGET: usize = 16;
const READ_TIMEOUT: Duration = Duration::from_millis(100);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

struct Connection {
    stream: UnixStream,
    request: Vec<u8>,
    response: Option<Vec<u8>>,
    written: usize,
    life: Rc<ConnectionLife>,
}

impl AsFd for Connection {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }
}

/// The Generic wrapper changes readiness interest after the callback. Keeping
/// WRITE disabled until a response exists avoids an idle writable-socket spin.
struct ConnectionSource(Generic<Connection>);
impl EventSource for ConnectionSource {
    type Event = Readiness;
    type Metadata = NoIoDrop<Connection>;
    type Ret = io::Result<PostAction>;
    type Error = io::Error;
    fn process_events<F>(
        &mut self,
        readiness: Readiness,
        token: Token,
        callback: F,
    ) -> io::Result<PostAction>
    where
        F: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        let writing = self.0.get_ref().response.is_some();
        let action = self.0.process_events(readiness, token, callback)?;
        if matches!(action, PostAction::Continue) && writing != self.0.get_ref().response.is_some()
        {
            self.0.interest = Interest::WRITE;
            Ok(PostAction::Reregister)
        } else {
            Ok(action)
        }
    }
    fn register(&mut self, poll: &mut Poll, tokens: &mut TokenFactory) -> calloop::Result<()> {
        self.0.register(poll, tokens)
    }
    fn reregister(&mut self, poll: &mut Poll, tokens: &mut TokenFactory) -> calloop::Result<()> {
        self.0.reregister(poll, tokens)
    }
    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.0.unregister(poll)
    }
}

struct ConnectionLife {
    source: Cell<Option<RegistrationToken>>,
    timer: Cell<Option<RegistrationToken>>,
    deadline: Cell<Instant>,
    closed: Cell<bool>,
    count: Rc<Cell<usize>>,
}
impl ConnectionLife {
    fn close<Data>(&self, handle: &LoopHandle<'static, Data>, from_timer: bool) {
        if self.closed.replace(true) {
            return;
        }
        self.count.set(self.count.get() - 1);
        // The currently executing source removes itself via its return action.
        let source = self.source.take();
        let timer = self.timer.take();
        if from_timer {
            if let Some(token) = source {
                handle.remove(token);
            }
        } else if let Some(token) = timer {
            handle.remove(token);
        }
    }
}

impl Connection {
    fn service(&mut self, dispatch: impl FnOnce(Request) -> Response) -> io::Result<bool> {
        if self.response.is_none() {
            let response = match read_request(&mut self.request, |buffer| self.stream.read(buffer))
            {
                Ok(false) => return Ok(false),
                Ok(true) if self.request.is_empty() => Response::Error {
                    message: "empty request".into(),
                },
                Ok(true) => match serde_json::from_slice::<Request>(&self.request) {
                    Ok(request) => dispatch(request),
                    Err(error) => Response::Error {
                        message: format!("invalid request: {error}"),
                    },
                },
                Err(error) => Response::Error {
                    message: format!("cannot read request: {error}"),
                },
            };
            self.queue_response(response)?;
        }
        self.write_response()
    }

    fn queue_response(&mut self, response: Response) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(&response).map_err(io::Error::other)?;
        bytes.push(b'\n');
        self.response = Some(bytes);
        self.request = Vec::new();
        self.life.deadline.set(Instant::now() + WRITE_TIMEOUT);
        Ok(())
    }

    fn write_response(&mut self) -> io::Result<bool> {
        let bytes = self.response.as_ref().unwrap();
        let end = (self.written + IO_BUDGET).min(bytes.len());
        for _ in 0..16 {
            if self.written == bytes.len() {
                return Ok(true);
            }
            if self.written == end {
                return Ok(false);
            }
            match self.stream.write(&bytes[self.written..end]) {
                Ok(0) => return Err(io::Error::new(ErrorKind::WriteZero, "IPC response closed")),
                Ok(count) => self.written += count,
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(self.written == bytes.len())
    }
}

fn add_connection<Data: 'static>(
    stream: UnixStream,
    handle: &LoopHandle<'static, Data>,
    count: Rc<Cell<usize>>,
    mut dispatch: impl FnMut(Request, &mut Data) -> Response + 'static,
) -> Result<()> {
    stream.set_nonblocking(true)?;
    let life = Rc::new(ConnectionLife {
        source: Cell::new(None),
        timer: Cell::new(None),
        deadline: Cell::new(Instant::now() + READ_TIMEOUT),
        closed: Cell::new(false),
        count,
    });
    life.count.set(life.count.get() + 1);
    let callback_handle = handle.clone();
    let callback_life = life.clone();
    let connection = Connection {
        stream,
        request: Vec::new(),
        response: None,
        written: 0,
        life: life.clone(),
    };
    let source = Dispatcher::new(
        ConnectionSource(Generic::new(connection, Interest::READ, Mode::Level)),
        move |_, connection, data| {
            // SAFETY: only buffers/state are changed; the registered stream is never replaced or dropped here.
            let connection = unsafe { connection.get_mut() };
            match connection.service(|request| dispatch(request, data)) {
                Ok(false) => Ok(PostAction::Continue),
                result => {
                    if let Err(error) = result {
                        tracing::debug!(%error, "IPC connection closed");
                    }
                    callback_life.close(&callback_handle, false);
                    Ok(PostAction::Remove)
                }
            }
        },
    );
    match handle.register_dispatcher(source.clone()) {
        Ok(token) => life.source.set(Some(token)),
        Err(error) => {
            life.close(handle, false);
            return Err(anyhow::anyhow!("cannot register IPC client: {error}"));
        }
    }
    let timer_life = life.clone();
    let timer_handle = handle.clone();
    match handle.insert_source(Timer::from_deadline(life.deadline.get()), move |_, _, _| {
        let deadline = timer_life.deadline.get();
        if Instant::now() < deadline {
            TimeoutAction::ToInstant(deadline)
        } else if source.as_source_ref().0.get_ref().response.is_none() {
            let queued = {
                let mut source = source.as_source_mut();
                // SAFETY: only response buffers change; the registered stream stays in place.
                let connection = unsafe { source.0.get_mut() };
                let result = connection.queue_response(Response::Error {
                    message: "request timed out".into(),
                });
                source.0.interest = Interest::WRITE;
                result
            };
            // Release the Dispatcher borrow before updating its poll registration.
            if queued.is_ok()
                && timer_life
                    .source
                    .get()
                    .is_some_and(|token| timer_handle.update(&token).is_ok())
            {
                TimeoutAction::ToInstant(timer_life.deadline.get())
            } else {
                timer_life.close(&timer_handle, true);
                TimeoutAction::Drop
            }
        } else {
            timer_life.close(&timer_handle, true);
            TimeoutAction::Drop
        }
    }) {
        Ok(token) => life.timer.set(Some(token)),
        Err(error) => {
            life.close(handle, true);
            return Err(anyhow::anyhow!("cannot register IPC deadline: {error}"));
        }
    }
    Ok(())
}

pub fn init(event_loop: &mut EventLoop<'static, CalloopData>, state: &mut Anvil) -> Result<()> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    let path = PathBuf::from(runtime).join(SOCKET_NAME);
    if path.exists() {
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
    let handle = event_loop.handle();
    let count = Rc::new(Cell::new(0));
    event_loop
        .handle()
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, _| {
                for _ in 0..ACCEPT_BUDGET {
                    match listener.accept() {
                        Ok((stream, _)) if count.get() < MAX_CONNECTIONS => {
                            if let Err(error) =
                                add_connection(stream, &handle, count.clone(), |request, data| {
                                    dispatch(request, &mut data.state)
                                })
                            {
                                tracing::warn!(%error, "cannot accept IPC client");
                            }
                        }
                        Ok(_) => {}
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

/// Process bounded input without waiting for a missing fragment; newline or EOF
/// completes the original framing contract. Returning false preserves partial bytes.
fn read_request(
    request: &mut Vec<u8>,
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
) -> io::Result<bool> {
    let mut chunk = [0; 4096];
    for _ in 0..(IO_BUDGET / 4096) {
        let count = match read(&mut chunk) {
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            return Ok(true);
        }
        let end = chunk[..count].iter().position(|&byte| byte == b'\n');
        let length = end.map_or(count, |index| index + 1);
        if request.len() + length > MAX_REQUEST {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "request exceeds 64 KiB",
            ));
        }
        request.extend_from_slice(&chunk[..length]);
        if end.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
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
        Request::OutputList { .. } => Response::Outputs {
            outputs: state.control_output_list(),
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
        Request::BarRefresh { name, .. } => {
            #[cfg(feature = "bar")]
            {
                match state.bar.jobs.refresh_requested(name.as_deref()) {
                    Ok((started, queued)) => Response::BarRefreshed { started, queued },
                    Err(error) => Response::Error {
                        message: error.to_string(),
                    },
                }
            }
            #[cfg(not(feature = "bar"))]
            {
                let _ = name;
                Response::Error {
                    message: "bar refresh is unavailable: Anvil was built without the bar feature"
                        .into(),
                }
            }
        }
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
    fn open_empty_and_partial_requests_receive_timeout_errors_and_release_connections() {
        for payload in [&b""[..], &b"{"[..]] {
            let (mut client, server) = UnixStream::pair().unwrap();
            client.set_nonblocking(true).unwrap();
            let mut event_loop: EventLoop<usize> = EventLoop::try_new().unwrap();
            let count = Rc::new(Cell::new(0));
            add_connection(server, &event_loop.handle(), count.clone(), |_, _| {
                panic!("incomplete requests must never dispatch a command")
            })
            .unwrap();
            event_loop
                .handle()
                .insert_source(
                    Timer::from_duration(Duration::from_millis(1)),
                    |_, _, heartbeat| {
                        *heartbeat += 1;
                        TimeoutAction::Drop
                    },
                )
                .unwrap();
            // Deliberately keep the write side open, as a stalled IPC client would.
            client.write_all(payload).unwrap();
            let mut response = Vec::new();
            let mut buffer = [0; 4096];
            let mut heartbeat = 0;
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                event_loop
                    .dispatch(Duration::from_millis(5), &mut heartbeat)
                    .unwrap();
                match client.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => response.extend_from_slice(&buffer[..n]),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                    other => panic!("unexpected read {other:?}"),
                }
                assert!(
                    Instant::now() < deadline,
                    "timeout response did not complete"
                );
            }
            assert_eq!(
                heartbeat, 1,
                "waiting for input must not block other timers"
            );
            assert_eq!(count.get(), 0, "completed connection must release its slot");
            assert_eq!(response.last(), Some(&b'\n'));
            let response: serde_json::Value = serde_json::from_slice(&response).unwrap();
            assert_eq!(response["status"], "error");
            assert_eq!(response["message"], "request timed out");
        }
    }

    #[test]
    fn writable_interest_and_stalled_peer_do_not_starve_the_event_loop() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        server.set_nonblocking(true).unwrap();
        let life = Rc::new(ConnectionLife {
            source: Cell::new(None),
            timer: Cell::new(None),
            deadline: Cell::new(Instant::now()),
            closed: Cell::new(false),
            count: Rc::new(Cell::new(1)),
        });
        let expected: Vec<u8> = (0..2_000_000).map(|i| (i % 251) as u8).collect();
        let payload = expected.clone();
        let connection = Connection {
            stream: server,
            request: Vec::new(),
            response: None,
            written: 0,
            life,
        };
        let mut event_loop: EventLoop<usize> = EventLoop::try_new().unwrap();
        let calls = Rc::new(Cell::new(0));
        let callback_calls = calls.clone();
        let complete = Rc::new(Cell::new(false));
        let callback_complete = complete.clone();
        event_loop
            .handle()
            .insert_source(
                ConnectionSource(Generic::new(connection, Interest::READ, Mode::Level)),
                move |_, connection, _| {
                    // SAFETY: this test mutates only buffers, never the registered stream.
                    let connection = unsafe { connection.get_mut() };
                    callback_calls.set(callback_calls.get() + 1);
                    if connection.response.is_none() {
                        assert!(read_request(&mut connection.request, |buf| connection
                            .stream
                            .read(buf))?);
                        connection.response = Some(payload.clone());
                    }
                    if connection.write_response()? {
                        callback_complete.set(true);
                        Ok(PostAction::Remove)
                    } else {
                        Ok(PostAction::Continue)
                    }
                },
            )
            .unwrap();
        event_loop
            .handle()
            .insert_source(
                Timer::from_duration(Duration::from_millis(1)),
                |_, _, heartbeat| {
                    *heartbeat += 1;
                    TimeoutAction::Drop
                },
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        let mut heartbeat = 0;
        for _ in 0..10 {
            event_loop
                .dispatch(Duration::from_millis(2), &mut heartbeat)
                .unwrap();
        }
        assert_eq!(heartbeat, 1);
        assert!(!complete.get());
        let stalled_calls = calls.get();
        for _ in 0..5 {
            event_loop
                .dispatch(Duration::from_millis(1), &mut heartbeat)
                .unwrap();
        }
        assert_eq!(
            calls.get(),
            stalled_calls,
            "writable interest must sleep while peer is stalled"
        );
        let mut received = Vec::new();
        let mut buffer = [0; 8192];
        let deadline = Instant::now() + Duration::from_secs(3);
        while !complete.get() || received.len() < expected.len() {
            loop {
                match client.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => received.extend_from_slice(&buffer[..n]),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    other => panic!("unexpected read {other:?}"),
                }
            }
            event_loop.dispatch(Duration::ZERO, &mut heartbeat).unwrap();
            assert!(Instant::now() < deadline);
        }
        assert_eq!(received, expected);
    }

    #[test]
    fn stalled_response_writes_are_bounded_and_resume_at_the_right_offset() {
        let (mut client, server) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        client.set_nonblocking(true).unwrap();
        let life = Rc::new(ConnectionLife {
            source: Cell::new(None),
            timer: Cell::new(None),
            deadline: Cell::new(Instant::now()),
            closed: Cell::new(false),
            count: Rc::new(Cell::new(1)),
        });
        let expected: Vec<u8> = (0..2_000_000).map(|i| (i % 251) as u8).collect();
        let mut connection = Connection {
            stream: server,
            request: Vec::new(),
            response: Some(expected.clone()),
            written: 0,
            life,
        };
        assert!(!connection.write_response().unwrap());
        assert!(connection.written <= IO_BUDGET);
        for _ in 0..64 {
            connection.write_response().unwrap();
        }
        let stalled = connection.written;
        assert!(stalled < expected.len());
        assert!(!connection.write_response().unwrap());
        assert_eq!(connection.written, stalled);
        let mut received = Vec::new();
        let mut buffer = [0; 8192];
        while received.len() < expected.len() {
            loop {
                match client.read(&mut buffer) {
                    Ok(n) if n > 0 => received.extend_from_slice(&buffer[..n]),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    other => panic!("unexpected read {other:?}"),
                }
            }
            connection.write_response().unwrap();
        }
        assert_eq!(received, expected);
    }

    #[test]
    fn fragmented_request_survives_would_block() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        let mut request = Vec::new();
        for byte in b"{\"command\":\"window_list\",\"version\":1}" {
            client.write_all(&[*byte]).unwrap();
            assert!(!read_request(&mut request, |buf| server.read(buf)).unwrap());
        }
        client.write_all(b"\n").unwrap();
        assert!(read_request(&mut request, |buf| server.read(buf)).unwrap());
        assert!(matches!(
            serde_json::from_slice::<Request>(&request).unwrap(),
            Request::WindowList { .. }
        ));
    }
    #[test]
    fn eof_remains_supported() {
        let mut reader = &b"{\"command\":\"debug_stats\",\"version\":1}"[..];
        let mut request = Vec::new();
        assert!(read_request(&mut request, |buf| reader.read(buf)).unwrap());
        assert!(matches!(
            serde_json::from_slice::<Request>(&request).unwrap(),
            Request::DebugStats { .. }
        ));
    }
    #[test]
    fn oversized_input_is_bounded_and_rejected() {
        let mut request = Vec::new();
        assert!(
            !read_request(&mut request, |buf| {
                buf.fill(b'x');
                Ok(buf.len())
            })
            .unwrap()
        );
        let error = read_request(&mut request, |buf| {
            buf.fill(b'x');
            Ok(buf.len())
        })
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(request.len(), MAX_REQUEST);
    }
}
