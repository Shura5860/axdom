use async_trait::async_trait;

use crate::action::{ActionRequest, ActionResult, KeyCombo, NodeRef, Screenshot};
use crate::error::{AxdomError, Result};
use crate::model::{Bounds, DoctorReport, RawNode, WindowInfo};

/// Parameters controlling how deep and wide the backend should traverse a window tree.
#[derive(Debug, Clone)]
pub struct TreeQuery {
    /// Optional window title substring or backend window ID. `None` targets the active/focused window.
    pub window_filter: Option<String>,
    /// Maximum traversal depth from the window root.
    pub max_depth: usize,
    /// Maximum raw nodes to fetch before truncating.
    pub max_nodes: usize,
    /// Overall timeout budget in milliseconds.
    pub timeout_ms: u64,
}

impl Default for TreeQuery {
    fn default() -> Self {
        Self {
            window_filter: None,
            max_depth: 12,
            max_nodes: 1_500,
            timeout_ms: 2_000,
        }
    }
}

/// Raw tree fetched by a platform backend along with traversal metadata.
#[derive(Debug, Clone)]
pub struct RawWindowTree {
    pub window: WindowInfo,
    pub root: RawNode,
    pub total_raw_nodes: usize,
    pub truncated: bool,
}

/// Platform abstraction for OS accessibility APIs (Linux AT-SPI2, Windows UIA, macOS AXUIElement).
#[async_trait]
pub trait AccessibilityProvider: Send + Sync {
    /// Run platform health diagnostics (e.g., `axdom doctor`).
    async fn doctor(&self) -> DoctorReport;

    /// List open top-level application windows.
    async fn list_windows(&self) -> Result<Vec<WindowInfo>>;

    /// Fetch the raw accessibility tree of the target (or active) window.
    async fn get_window_tree(&self, query: &TreeQuery) -> Result<RawWindowTree>;

    /// Verify that a previously snapshotted node still matches its live role/name.
    async fn verify_node(&self, node: &NodeRef) -> Result<()>;

    /// Execute a programmatic accessibility action on a node.
    async fn perform_action(
        &self,
        node: &NodeRef,
        action: &ActionRequest,
        verify: bool,
    ) -> Result<ActionResult>;

    /// Send a keyboard shortcut (e.g. `Ctrl+S`, `Enter`) to the active desktop session.
    async fn press_key(&self, _combo: &KeyCombo) -> Result<ActionResult> {
        Err(AxdomError::ActionUnsupported(
            "press_key is not supported by this backend".to_string(),
        ))
    }

    /// Capture a PNG screenshot (optionally cropped to `bounds`) for blind-spot fallback.
    async fn capture_screenshot(&self, _bounds: Option<Bounds>) -> Result<Screenshot> {
        Err(AxdomError::ActionUnsupported(
            "screenshot capture is not supported by this backend".to_string(),
        ))
    }
}
