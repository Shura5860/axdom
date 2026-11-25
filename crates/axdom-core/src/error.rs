use thiserror::Error;

#[derive(Debug, Error)]
pub enum AxdomError {
    #[error("accessibility bus unavailable: {0}")]
    BusUnavailable(String),

    #[error("no active window found")]
    NoActiveWindow,

    #[error("window not found: {0}")]
    WindowNotFound(String),

    #[error("node not found in snapshot: {0}")]
    NodeNotFound(String),

    #[error("stale node {id}: expected {expected}, found {actual} (re-run `axdom tree`)")]
    StaleNode {
        id: String,
        expected: String,
        actual: String,
    },

    #[error("action unsupported on node: {0}")]
    ActionUnsupported(String),

    #[error("action failed: {0}")]
    ActionFailed(String),

    #[error("policy denied: {0}")]
    PolicyDenied(String),

    #[error("snapshot store error: {0}")]
    SnapshotStore(String),

    #[error("backend error: {0}")]
    Backend(String),

    #[error("operation timed out after {0}ms")]
    Timeout(u64),
}

pub type Result<T> = std::result::Result<T, AxdomError>;
