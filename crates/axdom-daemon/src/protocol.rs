use std::path::PathBuf;

use axdom_core::{
    ActionRequest, ActionResult, Bounds, KeyCombo, Screenshot, TreeSnapshot, WindowInfo,
};
use serde::{Deserialize, Serialize};

/// Default Unix domain socket path: `$XDG_RUNTIME_DIR/axdom/axdom.sock` or `/tmp/axdom.sock`.
pub fn default_socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|dir| PathBuf::from(dir).join("axdom").join("axdom.sock"))
        .unwrap_or_else(|| PathBuf::from("/tmp/axdom.sock"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DaemonRequest {
    Ping,
    Status,
    ListWindows,
    GetTree {
        window_filter: Option<String>,
        max_depth: usize,
        max_nodes: usize,
        timeout_ms: u64,
        force_refresh: bool,
    },
    PerformAction {
        node_id: String,
        action: ActionRequest,
        verify: bool,
        #[serde(default)]
        allow_fallback: bool,
    },
    PressKey {
        combo: KeyCombo,
    },
    CaptureScreenshot {
        bounds: Option<Bounds>,
    },
    Invalidate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub invalidations: u64,
    pub cache_dirty: bool,
    pub cached_window: Option<String>,
    pub cached_snapshot_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DaemonResponse {
    Pong,
    Status {
        info: DaemonStatus,
    },
    Windows {
        windows: Vec<WindowInfo>,
    },
    Tree {
        snapshot: TreeSnapshot,
        cache_hit: bool,
        elapsed_ms: u64,
    },
    Action {
        result: ActionResult,
    },
    Screenshot {
        screenshot: Screenshot,
    },
    Ok,
    Error {
        message: String,
    },
}
