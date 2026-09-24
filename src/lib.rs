//! Platform-independent parts of Anvil.
//!
//! Configuration parsing and layout calculation deliberately live in the library crate instead
//! of the compositor binary. Neither module needs a Wayland server or graphics stack, so keeping
//! them separate makes their behavior cheap to unit-test on every development machine.

pub mod config;
#[cfg(feature = "anvilctl")]
pub mod ipc;
pub mod layout;
