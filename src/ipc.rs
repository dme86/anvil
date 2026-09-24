//! Versioned data model shared by Anvil's Unix-socket server and the `anvilctl` client.
//!
//! The transport is one newline-terminated JSON request followed by one JSON response. Keeping the
//! wire types in the platform-independent library prevents the CLI and compositor from drifting
//! into subtly incompatible ad-hoc string parsing as more commands are added.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const SOCKET_NAME: &str = "anvil.sock";

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    WindowList { version: u32 },
    Spawn { version: u32, argv: Vec<String> },
    Reload { version: u32 },
}

impl Request {
    pub const fn version(&self) -> u32 {
        match self {
            Self::WindowList { version }
            | Self::Spawn { version, .. }
            | Self::Reload { version } => *version,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Windows { windows: Vec<WindowInfo> },
    Spawned { pid: u32 },
    Reloaded { config: Option<String> },
    Error { message: String },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct WindowInfo {
    pub index: usize,
    pub title: String,
    pub app_id: String,
    /// Human-facing one-based tag numbers rather than internal mask bit positions.
    pub tags: Vec<usize>,
    pub focused: bool,
    pub floating: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip_preserves_argv_boundaries() {
        let request = Request::Spawn {
            version: PROTOCOL_VERSION,
            argv: vec!["firefox".into(), "two words".into()],
        };
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: Request = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            decoded,
            Request::Spawn { argv, .. } if argv == ["firefox", "two words"]
        ));
    }
}
