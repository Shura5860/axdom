use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axdom_core::{
    AccessibilityProvider, ActionRequest, AppPolicy, AxdomError, Bounds, KeyCombo, Pruner,
    PrunerConfig, TreeQuery, TreeSnapshot, find_node_ref,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::RwLock;

use crate::protocol::{DaemonRequest, DaemonResponse, DaemonStatus};

/// Default TTL safety net for cached snapshots even if an app misses an AT-SPI2 event.
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(5);
/// Maximum number of distinct window queries held simultaneously in the hot cache.
const MAX_CACHED_WINDOWS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    filter_key: Option<String>,
    max_depth: usize,
    max_nodes: usize,
}

struct CachedEntry {
    snapshot: TreeSnapshot,
    cached_at: Instant,
    dirty: bool,
}

#[derive(Default)]
struct CacheStore {
    entries: HashMap<CacheKey, CachedEntry>,
    last_snapshot: Option<TreeSnapshot>,
}

/// Shared daemon state holding the hot in-memory multi-window cache and metrics.
pub struct DaemonState<P: AccessibilityProvider> {
    provider: Arc<P>,
    policy: AppPolicy,
    pruner: Pruner,
    cache: RwLock<CacheStore>,
    cache_ttl: Duration,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    invalidations: AtomicU64,
}

impl<P: AccessibilityProvider> DaemonState<P> {
    pub fn new(provider: Arc<P>, policy: AppPolicy) -> Self {
        Self {
            provider,
            policy,
            pruner: Pruner::new(PrunerConfig::default()),
            cache: RwLock::new(CacheStore::default()),
            cache_ttl: DEFAULT_CACHE_TTL,
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            invalidations: AtomicU64::new(0),
        }
    }

    /// Marks all cached window snapshots dirty without discarding `last_snapshot` for ID lookup.
    pub async fn invalidate(&self) {
        let mut guard = self.cache.write().await;
        let mut any_marked = false;
        for entry in guard.entries.values_mut() {
            if !entry.dirty {
                entry.dirty = true;
                any_marked = true;
            }
        }
        if any_marked {
            self.invalidations.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub async fn handle(&self, req: DaemonRequest) -> DaemonResponse {
        match req {
            DaemonRequest::Ping => DaemonResponse::Pong,
            DaemonRequest::Status => self.handle_status().await,
            DaemonRequest::ListWindows => self.handle_list_windows().await,
            DaemonRequest::GetTree {
                window_filter,
                max_depth,
                max_nodes,
                timeout_ms,
                force_refresh,
            } => {
                self.handle_get_tree(
                    window_filter,
                    max_depth,
                    max_nodes,
                    timeout_ms,
                    force_refresh,
                )
                .await
            }
            DaemonRequest::PerformAction {
                node_id,
                action,
                verify,
                allow_fallback,
            } => {
                self.handle_action(&node_id, &action, verify, allow_fallback)
                    .await
            }
            DaemonRequest::PressKey { combo } => self.handle_press_key(&combo).await,
            DaemonRequest::CaptureScreenshot { bounds } => self.handle_screenshot(bounds).await,
            DaemonRequest::Invalidate => {
                self.invalidate().await;
                DaemonResponse::Ok
            }
        }
    }

    async fn handle_status(&self) -> DaemonResponse {
        let guard = self.cache.read().await;
        let (cached_window, cached_snapshot_id) = match guard.last_snapshot.as_ref() {
            Some(snap) => (Some(snap.window.title.clone()), Some(snap.snapshot_id)),
            None => (None, None),
        };
        let cache_dirty = !guard.entries.is_empty() && guard.entries.values().all(|e| e.dirty);
        DaemonResponse::Status {
            info: DaemonStatus {
                pid: std::process::id(),
                cache_hits: self.cache_hits.load(Ordering::Relaxed),
                cache_misses: self.cache_misses.load(Ordering::Relaxed),
                invalidations: self.invalidations.load(Ordering::Relaxed),
                cache_dirty,
                cached_window,
                cached_snapshot_id,
            },
        }
    }

    async fn handle_list_windows(&self) -> DaemonResponse {
        match self.provider.list_windows().await {
            Ok(windows) => {
                let allowed = windows
                    .into_iter()
                    .filter(|w| self.policy.check_read(w).is_ok())
                    .collect();
                DaemonResponse::Windows { windows: allowed }
            }
            Err(e) => DaemonResponse::Error {
                message: e.to_string(),
            },
        }
    }

    async fn handle_get_tree(
        &self,
        window_filter: Option<String>,
        max_depth: usize,
        max_nodes: usize,
        timeout_ms: u64,
        force_refresh: bool,
    ) -> DaemonResponse {
        let started = Instant::now();
        let key = CacheKey {
            filter_key: window_filter.clone(),
            max_depth,
            max_nodes,
        };

        if !force_refresh {
            let guard = self.cache.read().await;
            if let Some(entry) = guard.entries.get(&key)
                && !entry.dirty
                && entry.cached_at.elapsed() < self.cache_ttl
            {
                self.cache_hits.fetch_add(1, Ordering::Relaxed);
                return DaemonResponse::Tree {
                    snapshot: entry.snapshot.clone(),
                    cache_hit: true,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                };
            }
        }

        let query = TreeQuery {
            window_filter,
            max_depth,
            max_nodes,
            timeout_ms,
        };

        let raw = match self.provider.get_window_tree(&query).await {
            Ok(raw) => raw,
            Err(e) => {
                return DaemonResponse::Error {
                    message: e.to_string(),
                };
            }
        };

        if let Err(e) = self.policy.check_read(&raw.window) {
            return DaemonResponse::Error {
                message: e.to_string(),
            };
        }

        let snapshot = self.pruner.prune(raw);
        self.cache_misses.fetch_add(1, Ordering::Relaxed);

        let mut guard = self.cache.write().await;
        if guard.entries.len() >= MAX_CACHED_WINDOWS
            && !guard.entries.contains_key(&key)
            && let Some(oldest_key) = guard
                .entries
                .iter()
                .min_by_key(|(_, v)| v.cached_at)
                .map(|(k, _)| k.clone())
        {
            guard.entries.remove(&oldest_key);
        }
        guard.entries.insert(
            key,
            CachedEntry {
                snapshot: snapshot.clone(),
                cached_at: Instant::now(),
                dirty: false,
            },
        );
        guard.last_snapshot = Some(snapshot.clone());

        DaemonResponse::Tree {
            snapshot,
            cache_hit: false,
            elapsed_ms: started.elapsed().as_millis() as u64,
        }
    }

    async fn handle_action(
        &self,
        node_id: &str,
        action: &ActionRequest,
        verify: bool,
        allow_fallback: bool,
    ) -> DaemonResponse {
        let snapshot = {
            let guard = self.cache.read().await;
            match guard.last_snapshot.as_ref() {
                Some(snap) => snap.clone(),
                None => {
                    return DaemonResponse::Error {
                        message: "no cached tree snapshot in daemon; run `axdom tree` first"
                            .to_string(),
                    };
                }
            }
        };

        if let Err(e) = self.policy.check_write(&snapshot.window) {
            return DaemonResponse::Error {
                message: e.to_string(),
            };
        }

        let node_ref = match find_node_ref(&snapshot, node_id) {
            Ok(r) => r,
            Err(e) => {
                return DaemonResponse::Error {
                    message: e.to_string(),
                };
            }
        };

        let res = match self
            .provider
            .perform_action(&node_ref, action, verify)
            .await
        {
            Ok(result) => Ok(result),
            Err(AxdomError::ActionUnsupported(_))
                if allow_fallback && matches!(action, ActionRequest::Click) =>
            {
                let (x, y) = node_ref
                    .bounds
                    .filter(|b| !b.is_zero_sized())
                    .map(|b| (b.x + b.width / 2, b.y + b.height / 2))
                    .unwrap_or((0, 0));
                let fallback = ActionRequest::ClickCoords { x, y };
                self.provider
                    .perform_action(&node_ref, &fallback, false)
                    .await
            }
            Err(e) => Err(e),
        };

        match res {
            Ok(result) => {
                self.invalidate().await;
                DaemonResponse::Action { result }
            }
            Err(e) => DaemonResponse::Error {
                message: e.to_string(),
            },
        }
    }

    async fn handle_press_key(&self, combo: &KeyCombo) -> DaemonResponse {
        if self.policy.read_only {
            return DaemonResponse::Error {
                message: "policy denied: daemon is running in read-only mode".to_string(),
            };
        }
        if let Some(snap) = self.cache.read().await.last_snapshot.as_ref()
            && let Err(e) = self.policy.check_write(&snap.window)
        {
            return DaemonResponse::Error {
                message: e.to_string(),
            };
        }

        match self.provider.press_key(combo).await {
            Ok(result) => {
                self.invalidate().await;
                DaemonResponse::Action { result }
            }
            Err(e) => DaemonResponse::Error {
                message: e.to_string(),
            },
        }
    }

    async fn handle_screenshot(&self, bounds: Option<Bounds>) -> DaemonResponse {
        if let Some(snap) = self.cache.read().await.last_snapshot.as_ref()
            && let Err(e) = self.policy.check_read(&snap.window)
        {
            return DaemonResponse::Error {
                message: e.to_string(),
            };
        }
        match self.provider.capture_screenshot(bounds).await {
            Ok(screenshot) => DaemonResponse::Screenshot { screenshot },
            Err(e) => DaemonResponse::Error {
                message: e.to_string(),
            },
        }
    }
}

/// Binds a Unix domain socket with strict `0700`/`0600` permissions, symlink protection,
/// live-daemon collision detection, and `peer_cred` UID verification on every connection.
pub async fn serve_unix<P: AccessibilityProvider + 'static>(
    socket_path: &Path,
    state: Arc<DaemonState<P>>,
) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }

    if let Ok(meta) = std::fs::symlink_metadata(socket_path) {
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refusing to bind over symlink at {}", socket_path.display()),
            ));
        }
        // Check if another live daemon is already listening before removing the socket file.
        if UnixStream::connect(socket_path).await.is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "another axdom daemon is already listening on {}",
                    socket_path.display()
                ),
            ));
        }
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    let expected_uid = std::fs::symlink_metadata(socket_path)?.uid();
    tracing::info!(
        "axdom daemon listening on {} (mode 0600, uid {})",
        socket_path.display(),
        expected_uid
    );

    let shutdown = async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm = signal(SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async {
                    if let Some(s) = sigterm.as_mut() {
                        s.recv().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };

    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("axdom daemon shutting down, removing {}", socket_path.display());
                break;
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                match stream.peer_cred() {
                    Ok(cred) if cred.uid() == expected_uid => {}
                    Ok(cred) => {
                        tracing::warn!(
                            "rejected daemon connection from uid {} (expected {})",
                            cred.uid(),
                            expected_uid
                        );
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!("rejected daemon connection (peer_cred failed: {e})");
                        continue;
                    }
                }
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, state).await {
                        tracing::debug!("daemon client disconnected: {e}");
                    }
                });
            }
        }
    }

    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

async fn handle_connection<P: AccessibilityProvider>(
    stream: UnixStream,
    state: Arc<DaemonState<P>>,
) -> io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<DaemonRequest>(trimmed) {
            Ok(req) => state.handle(req).await,
            Err(e) => DaemonResponse::Error {
                message: format!("invalid request: {e}"),
            },
        };
        let mut payload = serde_json::to_string(&resp)
            .map_err(|e| io::Error::other(format!("serialize response: {e}")))?;
        payload.push('\n');
        writer.write_all(payload.as_bytes()).await?;
        writer.flush().await?;
    }

    Ok(())
}
