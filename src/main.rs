//! Anvil compositor process entry point.
//!
//! Smithay is intentionally modular rather than a ready-made compositor. Startup therefore wires
//! together three things: the Wayland display, Calloop's event loop, and our global `Anvil` state.
//! Protocol handlers and backend callbacks then receive mutable access to that one state object.

#![allow(irrefutable_let_patterns)]

mod handlers;
mod input;
mod state;
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

    let config_path = parse_config_arg()?;
    let (config, loaded_path) = Config::load(config_path.as_deref())?;
    if let Some(path) = loaded_path {
        tracing::info!(path = %path.display(), "loaded configuration");
    } else {
        tracing::info!("using built-in configuration");
    }

    // The Wayland `Display` owns client protocol objects. Its handle is cheap to clone and is used
    // by handlers for operations such as flushing clients and looking up client metadata.
    let mut event_loop: EventLoop<CalloopData> = EventLoop::try_new()?;
    let display: Display<Anvil> = Display::new()?;
    let display_handle = display.handle();
    let state = Anvil::new(&mut event_loop, display, config);
    let mut data = CalloopData {
        state,
        display_handle,
    };

    // Winit gives the first milestone a nested output and input source. Replacing this call with a
    // DRM/libinput backend later should not require changing layout or protocol policy.
    crate::winit::init(&mut event_loop, &mut data)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    for command in data.state.config.general.startup.clone() {
        data.state.spawn(&command);
    }

    tracing::info!(wayland_display = ?data.state.socket_name, "anvil is running");
    // From here on all work happens in registered Calloop callbacks: client requests, input and
    // redraws. No polling or secondary state-owning thread is needed.
    event_loop.run(None, &mut data, |_| {})?;
    Ok(())
}

fn parse_config_arg() -> Result<Option<PathBuf>> {
    // A deliberately small command line keeps `config.toml` as the only persistent interface.
    let mut args = std::env::args_os().skip(1);
    let Some(flag) = args.next() else {
        return Ok(None);
    };
    if flag != "-c" && flag != "--config" {
        anyhow::bail!("usage: anvil [-c|--config PATH]");
    }
    args.next()
        .map(PathBuf::from)
        .map(Some)
        .context("missing path after --config")
}
