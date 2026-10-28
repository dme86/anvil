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
    WindowList {
        version: u32,
    },
    OutputList {
        version: u32,
    },
    Spawn {
        version: u32,
        argv: Vec<String>,
    },
    Reload {
        version: u32,
    },
    /// None means all blocks; Some(name) targets one configured named block.
    BarRefresh {
        version: u32,
        name: Option<String>,
    },
    DebugStats {
        version: u32,
    },
}

impl Request {
    pub const fn version(&self) -> u32 {
        match self {
            Self::WindowList { version }
            | Self::OutputList { version }
            | Self::Spawn { version, .. }
            | Self::Reload { version }
            | Self::BarRefresh { version, .. }
            | Self::DebugStats { version } => *version,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Windows { windows: Vec<WindowInfo> },
    Outputs { outputs: Vec<OutputInfo> },
    Spawned { pid: u32 },
    Reloaded { config: Option<String> },
    BarRefreshed { started: usize, queued: usize },
    Stats { stats: RuntimeStats },
    Error { message: String },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct OutputInfo {
    pub name: String,
    pub current_mode: Option<String>,
    pub preferred_mode: Option<String>,
    pub position: [i32; 2],
    pub scale: f64,
    pub transform: String,
}

/// A point-in-time snapshot. Counters are cumulative since startup; unavailable OS data is null.
#[derive(Debug, Deserialize, Serialize)]
pub struct RuntimeStats {
    pub uptime_seconds: f64,
    pub connected_clients: usize,
    pub managed_windows: usize,
    pub outputs: usize,
    pub session_locked: bool,
    pub requested_repaints: u64,
    pub render_attempts: u64,
    pub rendered_frames: u64,
    pub render_failures: u64,
    pub average_render_time_ms: Option<f64>,
    pub dmabuf_imports: u64,
    pub dmabuf_import_failures: u64,
    pub open_file_descriptors: Option<usize>,
    pub rss_bytes: Option<u64>,
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
    fn bar_refresh_round_trip_preserves_name_and_all_target() {
        for name in [Some("updates".to_string()), None] {
            let request = Request::BarRefresh {
                version: PROTOCOL_VERSION,
                name: name.clone(),
            };
            let decoded: Request =
                serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
            assert!(matches!(decoded, Request::BarRefresh { name: actual, .. } if actual == name));
        }
        let response = Response::BarRefreshed {
            started: 1,
            queued: 2,
        };
        let encoded = serde_json::to_value(response).unwrap();
        assert_eq!(encoded["status"], "bar_refreshed");
        assert_eq!(encoded["queued"], 2);
    }

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
