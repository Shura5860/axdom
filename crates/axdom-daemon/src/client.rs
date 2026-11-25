use std::io;
use std::path::Path;

use axdom_core::{
    ActionRequest, ActionResult, AxdomError, Bounds, KeyCombo, Result, Screenshot, TreeQuery,
    TreeSnapshot, WindowInfo,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::protocol::{DaemonRequest, DaemonResponse, DaemonStatus, default_socket_path};

/// Lightweight async client for communicating with a running `axdom daemon` over Unix socket.
pub struct DaemonClient {
    stream: UnixStream,
}

impl DaemonClient {
    pub async fn connect(socket_path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(socket_path).await?;
        Ok(Self { stream })
    }

    pub async fn connect_default() -> io::Result<Self> {
        Self::connect(&default_socket_path()).await
    }

    pub async fn send(&mut self, req: &DaemonRequest) -> Result<DaemonResponse> {
        let mut payload = serde_json::to_string(req)
            .map_err(|e| AxdomError::Backend(format!("serialize daemon request: {e}")))?;
        payload.push('\n');

        self.stream
            .write_all(payload.as_bytes())
            .await
            .map_err(|e| AxdomError::Backend(format!("daemon write: {e}")))?;
        self.stream
            .flush()
            .await
            .map_err(|e| AxdomError::Backend(format!("daemon flush: {e}")))?;

        let mut reader = BufReader::new(&mut self.stream);
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| AxdomError::Backend(format!("daemon read: {e}")))?;
        if n == 0 {
            return Err(AxdomError::Backend("daemon closed connection".to_string()));
        }

        serde_json::from_str::<DaemonResponse>(line.trim())
            .map_err(|e| AxdomError::Backend(format!("parse daemon response: {e}")))
    }

    pub async fn ping(&mut self) -> Result<()> {
        match self.send(&DaemonRequest::Ping).await? {
            DaemonResponse::Pong => Ok(()),
            DaemonResponse::Error { message } => Err(AxdomError::Backend(message)),
            other => Err(AxdomError::Backend(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub async fn status(&mut self) -> Result<DaemonStatus> {
        match self.send(&DaemonRequest::Status).await? {
            DaemonResponse::Status { info } => Ok(info),
            DaemonResponse::Error { message } => Err(AxdomError::Backend(message)),
            other => Err(AxdomError::Backend(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub async fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        match self.send(&DaemonRequest::ListWindows).await? {
            DaemonResponse::Windows { windows } => Ok(windows),
            DaemonResponse::Error { message } => Err(AxdomError::Backend(message)),
            other => Err(AxdomError::Backend(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub async fn get_tree(
        &mut self,
        query: &TreeQuery,
        force_refresh: bool,
    ) -> Result<(TreeSnapshot, bool, u64)> {
        let req = DaemonRequest::GetTree {
            window_filter: query.window_filter.clone(),
            max_depth: query.max_depth,
            max_nodes: query.max_nodes,
            timeout_ms: query.timeout_ms,
            force_refresh,
        };
        match self.send(&req).await? {
            DaemonResponse::Tree {
                snapshot,
                cache_hit,
                elapsed_ms,
            } => Ok((snapshot, cache_hit, elapsed_ms)),
            DaemonResponse::Error { message } => Err(AxdomError::Backend(message)),
            other => Err(AxdomError::Backend(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub async fn perform_action(
        &mut self,
        node_id: &str,
        action: ActionRequest,
        verify: bool,
        allow_fallback: bool,
    ) -> Result<ActionResult> {
        let req = DaemonRequest::PerformAction {
            node_id: node_id.to_string(),
            action,
            verify,
            allow_fallback,
        };
        match self.send(&req).await? {
            DaemonResponse::Action { result } => Ok(result),
            DaemonResponse::Error { message } => Err(AxdomError::Backend(message)),
            other => Err(AxdomError::Backend(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub async fn press_key(&mut self, combo: KeyCombo) -> Result<ActionResult> {
        match self.send(&DaemonRequest::PressKey { combo }).await? {
            DaemonResponse::Action { result } => Ok(result),
            DaemonResponse::Error { message } => Err(AxdomError::Backend(message)),
            other => Err(AxdomError::Backend(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub async fn capture_screenshot(&mut self, bounds: Option<Bounds>) -> Result<Screenshot> {
        match self
            .send(&DaemonRequest::CaptureScreenshot { bounds })
            .await?
        {
            DaemonResponse::Screenshot { screenshot } => Ok(screenshot),
            DaemonResponse::Error { message } => Err(AxdomError::Backend(message)),
            other => Err(AxdomError::Backend(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use axdom_core::provider::RawWindowTree;
    use axdom_core::{
        AccessibilityProvider, AppPolicy, DoctorReport, NodeRef, NodeRole, NodeState, RawNode,
    };

    use super::*;
    use crate::server::{DaemonState, serve_unix};

    struct MockProvider;

    #[async_trait]
    impl AccessibilityProvider for MockProvider {
        async fn doctor(&self) -> DoctorReport {
            DoctorReport { checks: vec![] }
        }

        async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
            Ok(vec![WindowInfo {
                id: "w1".into(),
                title: "Notes".into(),
                app_name: "notes".into(),
                pid: Some(10),
                active: true,
            }])
        }

        async fn get_window_tree(&self, _query: &TreeQuery) -> Result<RawWindowTree> {
            Ok(RawWindowTree {
                window: WindowInfo {
                    id: "w1".into(),
                    title: "Notes".into(),
                    app_name: "notes".into(),
                    pid: Some(10),
                    active: true,
                },
                root: RawNode {
                    backend_ref: ":1.1:/root".into(),
                    role: NodeRole::Window,
                    name: "Notes".into(),
                    description: None,
                    value: None,
                    state: NodeState {
                        enabled: true,
                        visible: true,
                        showing: true,
                        sensitive: true,
                        ..Default::default()
                    },
                    bounds: None,
                    children: vec![RawNode {
                        backend_ref: ":1.1:/btn/save".into(),
                        role: NodeRole::Button,
                        name: "Save".into(),
                        description: None,
                        value: None,
                        state: NodeState {
                            enabled: true,
                            visible: true,
                            showing: true,
                            sensitive: true,
                            ..Default::default()
                        },
                        bounds: None,
                        children: vec![],
                    }],
                },
                total_raw_nodes: 2,
                truncated: false,
            })
        }

        async fn verify_node(&self, _node: &NodeRef) -> Result<()> {
            Ok(())
        }

        async fn perform_action(
            &self,
            node: &NodeRef,
            action: &ActionRequest,
            _verify: bool,
        ) -> Result<ActionResult> {
            Ok(ActionResult {
                node_id: node.id.clone(),
                action: action.name().to_string(),
                method_used: "mock:action".to_string(),
                success: true,
                detail: None,
            })
        }
    }

    #[tokio::test]
    async fn unix_socket_cache_hit_dirty_invalidation_and_depth_key() {
        let sock_path = PathBuf::from(format!("/tmp/axdom-test-{}.sock", std::process::id()));
        let state = Arc::new(DaemonState::new(
            Arc::new(MockProvider),
            AppPolicy::default(),
        ));

        let server_sock = sock_path.clone();
        let server_state = Arc::clone(&state);
        let handle = tokio::spawn(async move {
            let _ = serve_unix(&server_sock, server_state).await;
        });

        let mut client = loop {
            if let Ok(c) = DaemonClient::connect(&sock_path).await {
                break c;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        client.ping().await.unwrap();

        let (snap1, hit1, _) = client.get_tree(&TreeQuery::default(), false).await.unwrap();
        assert!(!hit1);

        let (snap2, hit2, _) = client.get_tree(&TreeQuery::default(), false).await.unwrap();
        assert!(hit2);
        assert_eq!(snap1.snapshot_id, snap2.snapshot_id);

        // Different max_depth must miss cache.
        let shallow = TreeQuery {
            max_depth: 3,
            ..Default::default()
        };
        let (_, hit_shallow, _) = client.get_tree(&shallow, false).await.unwrap();
        assert!(!hit_shallow);

        let save_id = snap1.elements[0].id.clone().unwrap();
        let act = client
            .perform_action(&save_id, ActionRequest::Click, true, false)
            .await
            .unwrap();
        assert!(act.success);

        // Even while dirty, a second action can still resolve IDs from last_snapshot!
        let act2 = client
            .perform_action(&save_id, ActionRequest::Focus, true, false)
            .await
            .unwrap();
        assert!(act2.success);

        // Next get_tree must miss because cache is dirty.
        let (_, hit3, _) = client.get_tree(&shallow, false).await.unwrap();
        assert!(!hit3);

        let st = client.status().await.unwrap();
        assert_eq!(st.cache_hits, 1);
        assert_eq!(st.cache_misses, 3);
        assert_eq!(st.invalidations, 1);

        handle.abort();
        let _ = std::fs::remove_file(&sock_path);
    }
}
