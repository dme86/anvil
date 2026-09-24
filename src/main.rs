//! Anvil compositor process entry point.
//!
//! Smithay is intentionally modular rather than a ready-made compositor. Startup therefore wires
//! together three things: the Wayland display, Calloop's event loop, and our global `Anvil` state.
//! Protocol handlers and backend callbacks then receive mutable access to that one state object.

#![allow(irrefutable_let_patterns)]

#[cfg(any(feature = "bar", feature = "launcher"))]
mod bar;
#[cfg(feature = "anvilctl")]
mod control;
mod handlers;
mod input;
#[cfg(feature = "launcher")]
mod launcher;
mod render;
mod state;
mod udev;
mod winit;

use std::path::PathBuf;

use anvil::config::Config;
use anyhow::{Context, Result};
use smithay::reexports::{
    calloop::EventLoop,
    wayland_server::{Display, DisplayHandle},
};

pub use state::Anvil;

/// Data Calloop passes to every registered event source.
///
/// Keeping compositor state here avoids `Arc<Mutex<_>>`: Calloop dispatches callbacks serially on
/// the compositor thread, so ordinary exclusive Rust borrowing is sufficient.
pub struct CalloopData {
    state: Anvil,
    display_handle: DisplayHandle,
}

fn main() -> Result<()> {
    // Respect RUST_LOG for development while providing useful, quiet startup messages by default.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "anvil=info".into()),
        )
        .init();

    let args = parse_args()?;
    let (config, loaded_path) = Config::load(args.config.as_deref())?;
    if let Some(ref path) = loaded_path {
        tracing::info!(path = %path.display(), "loaded configuration");
    } else {
        tracing::info!("using built-in configuration");
    }

    // The Wayland `Display` owns client protocol objects. Its handle is cheap to clone and is used
    // by handlers for operations such as flushing clients and looking up client metadata.
    let mut event_loop: EventLoop<CalloopData> = EventLoop::try_new()?;
    let display: Display<Anvil> = Display::new()?;
    let display_handle = display.handle();
    let state = Anvil::new(&mut event_loop, display, config, loaded_path)?;
    #[cfg(feature = "anvilctl")]
    let mut state = state;
    #[cfg(feature = "anvilctl")]
    crate::control::init(&mut event_loop, &mut state)?;
    let mut data = CalloopData {
        state,
        display_handle,
    };

    // Direct DRM is the normal session mode. Winit remains available explicitly because a nested
    // compositor is invaluable for development without taking ownership of the current TTY.
    if args.nested {
        crate::winit::init(&mut event_loop, &mut data)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    } else {
        crate::udev::init(&mut event_loop, &mut data)?;
    }
    for command in data.state.config.general.startup.clone() {
        data.state.spawn(&command);
    }

    tracing::info!(wayland_display = ?data.state.socket_name, "anvil is running");
    // From here on all work happens in registered Calloop callbacks: client requests, input and
    // redraws. No polling or secondary state-owning thread is needed.
    event_loop.run(None, &mut data, |data| {
        // Backends drive presentation differently, but both need the same protocol housekeeping.
        // Running it at the common event-loop boundary also flushes clients after pure Wayland
        // requests that do not happen to coincide with an input or display event.
        data.state.space.refresh();
        data.state.popups.cleanup();
        data.state.reap_children();
        let _ = data.display_handle.flush_clients();
    })?;
    Ok(())
}

struct Args {
    config: Option<PathBuf>,
    nested: bool,
}

fn parse_args() -> Result<Args> {
    // Backend selection is a runtime concern, while all persistent policy remains in config.toml.
    // Defaulting to DRM makes an ordinary display-manager session launch the real compositor;
    // developers opt into a host window with `--nested`.
    let mut parsed = Args {
        config: None,
        nested: false,
    };
    let mut args = std::env::args_os().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--nested" {
            parsed.nested = true;
        } else if flag == "-c" || flag == "--config" {
            parsed.config = Some(
                args.next()
                    .map(PathBuf::from)
                    .context("missing path after --config")?,
            );
        } else {
            anyhow::bail!("usage: anvil [--nested] [-c|--config PATH]");
        }
    }
    Ok(parsed)
}
