#[cfg(unix)]
pub mod client;
pub mod protocol;
#[cfg(unix)]
pub mod server;

#[cfg(unix)]
pub use client::DaemonClient;
pub use protocol::{DaemonRequest, DaemonResponse, DaemonStatus, default_socket_path};
#[cfg(unix)]
pub use server::{DaemonState, serve_unix};

#[cfg(not(unix))]
mod non_unix {
    use std::io;
    use std::path::Path;
    use std::sync::Arc;

    use axdom_core::{
        AccessibilityProvider, ActionRequest, ActionResult, AppPolicy, AxdomError, Bounds,
        KeyCombo, Result, Screenshot, TreeQuery, TreeSnapshot, WindowInfo,
    };

    use crate::protocol::{DaemonRequest, DaemonResponse, DaemonStatus};

    #[cfg(target_os = "windows")]
    const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\axdom-daemon";

    /// Async client for communicating with a running `axdom daemon` (Windows Named Pipe).
    pub struct DaemonClient {
        #[cfg(target_os = "windows")]
        pipe: tokio::net::windows::named_pipe::NamedPipeClient,
    }

    impl DaemonClient {
        pub async fn connect(_socket_path: &Path) -> io::Result<Self> {
            #[cfg(target_os = "windows")]
            {
                let pipe = tokio::net::windows::named_pipe::ClientOptions::new()
                    .open(DEFAULT_PIPE_NAME)?;
                Ok(Self { pipe })
            }
            #[cfg(not(target_os = "windows"))]
            {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "axdom daemon IPC is only supported on Unix and Windows",
                ))
            }
        }

        pub async fn connect_default() -> io::Result<Self> {
            Self::connect(Path::new("")).await
        }

        pub async fn send(&mut self, req: &DaemonRequest) -> Result<DaemonResponse> {
            #[cfg(target_os = "windows")]
            {
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                let mut payload = serde_json::to_string(req)
                    .map_err(|e| AxdomError::Backend(format!("serialize daemon request: {e}")))?;
                payload.push('\n');
                self.pipe
                    .write_all(payload.as_bytes())
                    .await
                    .map_err(|e| AxdomError::Backend(format!("pipe write: {e}")))?;
                self.pipe
                    .flush()
                    .await
                    .map_err(|e| AxdomError::Backend(format!("pipe flush: {e}")))?;
                let mut reader = BufReader::new(&mut self.pipe);
                let mut line = String::new();
                let n = reader
                    .read_line(&mut line)
                    .await
                    .map_err(|e| AxdomError::Backend(format!("pipe read: {e}")))?;
                if n == 0 {
                    return Err(AxdomError::Backend("daemon closed pipe".into()));
                }
                serde_json::from_str(&line)
                    .map_err(|e| AxdomError::Backend(format!("decode daemon response: {e}")))
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = req;
                Err(AxdomError::ActionUnsupported("daemon unsupported".into()))
            }
        }

        pub async fn status(&mut self) -> Result<DaemonStatus> {
            match self.send(&DaemonRequest::Status).await? {
                DaemonResponse::Status { info } => Ok(info),
                DaemonResponse::Error { message } => Err(AxdomError::Backend(message)),
                other => Err(AxdomError::Backend(format!(
                    "unexpected daemon response: {other:?}"
                ))),
            }
        }

        pub async fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
            match self.send(&DaemonRequest::ListWindows).await? {
                DaemonResponse::Windows { windows } => Ok(windows),
                DaemonResponse::Error { message } => Err(AxdomError::Backend(message)),
                other => Err(AxdomError::Backend(format!(
                    "unexpected daemon response: {other:?}"
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
                    "unexpected daemon response: {other:?}"
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
                DaemonResponse::Error { message } => Err(AxdomError::ActionFailed(message)),
                other => Err(AxdomError::Backend(format!(
                    "unexpected daemon response: {other:?}"
                ))),
            }
        }

        pub async fn press_key(&mut self, combo: KeyCombo) -> Result<ActionResult> {
            match self.send(&DaemonRequest::PressKey { combo }).await? {
                DaemonResponse::Action { result } => Ok(result),
                DaemonResponse::Error { message } => Err(AxdomError::ActionFailed(message)),
                other => Err(AxdomError::Backend(format!(
                    "unexpected daemon response: {other:?}"
                ))),
            }
        }

        pub async fn capture_screenshot(&mut self, bounds: Option<Bounds>) -> Result<Screenshot> {
            match self
                .send(&DaemonRequest::CaptureScreenshot { bounds })
                .await?
            {
                DaemonResponse::Screenshot { screenshot } => Ok(screenshot),
                DaemonResponse::Error { message } => Err(AxdomError::ActionFailed(message)),
                other => Err(AxdomError::Backend(format!(
                    "unexpected daemon response: {other:?}"
                ))),
            }
        }
    }

    pub struct DaemonState<P: AccessibilityProvider> {
        provider: Arc<P>,
        policy: AppPolicy,
        pruner: axdom_core::Pruner,
        last_snapshot:
            tokio::sync::RwLock<Option<(TreeSnapshot, Option<String>, std::time::Instant)>>,
        cache_hits: std::sync::atomic::AtomicU64,
        cache_misses: std::sync::atomic::AtomicU64,
        invalidations: std::sync::atomic::AtomicU64,
    }

    impl<P: AccessibilityProvider + 'static> DaemonState<P> {
        pub fn new(provider: Arc<P>, policy: AppPolicy) -> Self {
            Self {
                provider,
                policy,
                pruner: axdom_core::Pruner::new(axdom_core::PrunerConfig::default()),
                last_snapshot: tokio::sync::RwLock::new(None),
                cache_hits: std::sync::atomic::AtomicU64::new(0),
                cache_misses: std::sync::atomic::AtomicU64::new(0),
                invalidations: std::sync::atomic::AtomicU64::new(0),
            }
        }

        pub async fn invalidate(&self) {
            self.invalidations
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            *self.last_snapshot.write().await = None;
        }
    }

    pub async fn serve_unix<P: AccessibilityProvider + 'static>(
        _socket_path: &Path,
        state: Arc<DaemonState<P>>,
    ) -> io::Result<()> {
        #[cfg(target_os = "windows")]
        {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            use tokio::net::windows::named_pipe::ServerOptions;
            loop {
                let mut server = ServerOptions::new().create(DEFAULT_PIPE_NAME)?;
                server.connect().await?;
                let st = Arc::clone(&state);
                tokio::spawn(async move {
                    let mut reader = BufReader::new(&mut server);
                    let mut line = String::new();
                    while let Ok(n) = reader.read_line(&mut line).await {
                        if n == 0 {
                            break;
                        }
                        let resp = match serde_json::from_str::<DaemonRequest>(&line) {
                            Ok(DaemonRequest::Ping) => DaemonResponse::Pong,
                            Ok(DaemonRequest::Status) => {
                                let guard = st.last_snapshot.read().await;
                                DaemonResponse::Status {
                                    info: DaemonStatus {
                                        pid: std::process::id(),
                                        cache_hits: st
                                            .cache_hits
                                            .load(std::sync::atomic::Ordering::Relaxed),
                                        cache_misses: st
                                            .cache_misses
                                            .load(std::sync::atomic::Ordering::Relaxed),
                                        invalidations: st
                                            .invalidations
                                            .load(std::sync::atomic::Ordering::Relaxed),
                                        cache_dirty: guard.is_none(),
                                        cached_window: guard
                                            .as_ref()
                                            .map(|(s, _, _)| s.window.title.clone()),
                                        cached_snapshot_id: guard
                                            .as_ref()
                                            .map(|(s, _, _)| s.snapshot_id),
                                    },
                                }
                            }
                            Ok(DaemonRequest::Invalidate) => {
                                st.invalidate().await;
                                DaemonResponse::Ok
                            }
                            Ok(DaemonRequest::ListWindows) => {
                                match st.provider.list_windows().await {
                                    Ok(wins) => DaemonResponse::Windows {
                                        windows: wins
                                            .into_iter()
                                            .filter(|w| st.policy.check_read(w).is_ok())
                                            .collect(),
                                    },
                                    Err(e) => DaemonResponse::Error {
                                        message: e.to_string(),
                                    },
                                }
                            }
                            Ok(DaemonRequest::GetTree {
                                window_filter,
                                max_depth,
                                max_nodes,
                                timeout_ms,
                                force_refresh,
                            }) => {
                                let t0 = std::time::Instant::now();
                                if !force_refresh
                                    && let Some((snap, key, ts)) =
                                        st.last_snapshot.read().await.as_ref()
                                    && *key == window_filter
                                    && ts.elapsed() < std::time::Duration::from_secs(5)
                                {
                                    st.cache_hits
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    DaemonResponse::Tree {
                                        snapshot: snap.clone(),
                                        cache_hit: true,
                                        elapsed_ms: t0.elapsed().as_millis() as u64,
                                    }
                                } else {
                                    st.cache_misses
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    let q = TreeQuery {
                                        window_filter: window_filter.clone(),
                                        max_depth,
                                        max_nodes,
                                        timeout_ms,
                                    };
                                    match st.provider.get_window_tree(&q).await {
                                        Ok(raw) => match st.policy.check_read(&raw.window) {
                                            Ok(()) => {
                                                let snap = st.pruner.prune(raw);
                                                *st.last_snapshot.write().await = Some((
                                                    snap.clone(),
                                                    window_filter,
                                                    std::time::Instant::now(),
                                                ));
                                                DaemonResponse::Tree {
                                                    snapshot: snap,
                                                    cache_hit: false,
                                                    elapsed_ms: t0.elapsed().as_millis() as u64,
                                                }
                                            }
                                            Err(e) => DaemonResponse::Error {
                                                message: e.to_string(),
                                            },
                                        },
                                        Err(e) => DaemonResponse::Error {
                                            message: e.to_string(),
                                        },
                                    }
                                }
                            }
                            Ok(DaemonRequest::PerformAction {
                                node_id,
                                action,
                                verify,
                                ..
                            }) => {
                                let snap = st
                                    .last_snapshot
                                    .read()
                                    .await
                                    .as_ref()
                                    .map(|(s, _, _)| s.clone());
                                match snap {
                                    Some(s) => match st.policy.check_write(&s.window) {
                                        Err(e) => DaemonResponse::Error {
                                            message: e.to_string(),
                                        },
                                        Ok(()) => match axdom_core::find_node_ref(&s, &node_id) {
                                            Ok(nref) => {
                                                match st
                                                    .provider
                                                    .perform_action(&nref, &action, verify)
                                                    .await
                                                {
                                                    Ok(res) => {
                                                        st.invalidate().await;
                                                        DaemonResponse::Action { result: res }
                                                    }
                                                    Err(e) => DaemonResponse::Error {
                                                        message: e.to_string(),
                                                    },
                                                }
                                            }
                                            Err(e) => DaemonResponse::Error {
                                                message: e.to_string(),
                                            },
                                        },
                                    },
                                    None => DaemonResponse::Error {
                                        message: "no cached snapshot; call get_tree first".into(),
                                    },
                                }
                            }
                            Ok(DaemonRequest::PressKey { combo }) => {
                                if st.policy.read_only {
                                    DaemonResponse::Error {
                                        message: "policy denied: axdom daemon is running in read-only mode".into(),
                                    }
                                } else {
                                    match st.provider.press_key(&combo).await {
                                        Ok(res) => {
                                            st.invalidate().await;
                                            DaemonResponse::Action { result: res }
                                        }
                                        Err(e) => DaemonResponse::Error {
                                            message: e.to_string(),
                                        },
                                    }
                                }
                            }
                            Ok(DaemonRequest::CaptureScreenshot { bounds }) => {
                                match st.provider.capture_screenshot(bounds).await {
                                    Ok(shot) => DaemonResponse::Screenshot { screenshot: shot },
                                    Err(e) => DaemonResponse::Error {
                                        message: e.to_string(),
                                    },
                                }
                            }
                            Err(e) => DaemonResponse::Error {
                                message: format!("invalid request: {e}"),
                            },
                        };
                        if let Ok(mut out) = serde_json::to_string(&resp) {
                            out.push('\n');
                            let _ = reader.get_mut().write_all(out.as_bytes()).await;
                            let _ = reader.get_mut().flush().await;
                        }
                        line.clear();
                    }
                });
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = state;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "axdom daemon IPC is only supported on Unix and Windows",
            ))
        }
    }
}

#[cfg(not(unix))]
pub use non_unix::{DaemonClient, DaemonState, serve_unix};
