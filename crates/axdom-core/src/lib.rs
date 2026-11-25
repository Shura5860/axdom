pub mod action;
pub mod error;
pub mod format;
pub mod model;
pub mod policy;
pub mod provider;
pub mod pruner;
pub mod snapshot;

pub use action::{
    A11yEvent, ActionRequest, ActionResult, KeyCombo, KeyModifier, NodeRef, Screenshot,
    encode_base64,
};
pub use error::{AxdomError, Result};
pub use format::{OutputFormat, format_snapshot, format_windows};
pub use model::{
    Bounds, CheckStatus, DoctorCheck, DoctorReport, NodeRole, NodeState, PrunedNode, RawNode,
    TreeSnapshot, WindowInfo,
};
pub use policy::{AppPolicy, DEFAULT_DENY_APPS};
pub use provider::{AccessibilityProvider, TreeQuery};
pub use pruner::{Pruner, PrunerConfig};
pub use snapshot::{
    SnapshotDiff, SnapshotStore, diff_snapshots, find_node_ref, normalize_node_id, scope_snapshot,
};
