use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use axdom_core::provider::RawWindowTree;
use axdom_core::{
    AccessibilityProvider, ActionRequest, ActionResult, AxdomError, Bounds, DoctorReport, KeyCombo,
    NodeRef, NodeRole, NodeState, RawNode, Result, Screenshot, TreeQuery, WindowInfo,
};
use futures::future::{BoxFuture, join_all};
use tokio::sync::Semaphore;
use zbus::Connection;
use zbus::fdo::DBusProxy;
use zbus::zvariant::OwnedObjectPath;

use crate::action::{dispatch_action, verify_live_node};
use crate::bus::connect_a11y_bus;
use crate::cache::fetch_window_tree_via_cache;
use crate::doctor::run_doctor;
use crate::input::{capture_screen_png, send_key_combo};
use crate::proxy::{accessible_proxy, component_proxy, text_proxy, value_proxy};

const REGISTRY_DEST: &str = "org.a11y.atspi.Registry";
const REGISTRY_ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";

/// Max concurrent in-flight D-Bus calls during tree traversal to avoid saturating the bus.
const MAX_CONCURRENT_CALLS: usize = 32;

/// Linux AT-SPI2 implementation of `AccessibilityProvider` with automatic D-Bus reconnection.
pub struct LinuxProvider {
    conn: RwLock<Connection>,
}

impl LinuxProvider {
    pub async fn connect() -> Result<Self> {
        let conn = connect_a11y_bus().await?;
        Ok(Self {
            conn: RwLock::new(conn),
        })
    }

    pub fn connection(&self) -> Connection {
        self.conn.read().expect("conn lock poisoned").clone()
    }

    /// Re-establishes the AT-SPI2 D-Bus connection (e.g. if the bus restarted).
    pub async fn reconnect(&self) -> Result<Connection> {
        let fresh = connect_a11y_bus().await?;
        *self.conn.write().expect("conn lock poisoned") = fresh.clone();
        Ok(fresh)
    }

    async fn enumerate_window_handles(&self) -> Result<Vec<(WindowInfo, String, OwnedObjectPath)>> {
        let conn = self.connection();
        match Self::enumerate_on_conn(&conn).await {
            Ok(handles) => Ok(handles),
            Err(first_err) => {
                tracing::warn!("AT-SPI2 enumeration failed ({first_err}); attempting reconnect");
                let fresh = self.reconnect().await?;
                Self::enumerate_on_conn(&fresh).await
            }
        }
    }

    async fn enumerate_on_conn(
        conn: &Connection,
    ) -> Result<Vec<(WindowInfo, String, OwnedObjectPath)>> {
        let root = accessible_proxy(conn, REGISTRY_DEST, REGISTRY_ROOT_PATH)
            .await
            .map_err(|e| AxdomError::Backend(e.to_string()))?;

        let app_count = root.child_count().await.unwrap_or(0).max(0);
        let app_futs = (0..app_count).map(|i| {
            let root = root.clone();
            async move { root.get_child_at_index(i).await.ok() }
        });
        let apps: Vec<_> = join_all(app_futs).await.into_iter().flatten().collect();
        let dbus = DBusProxy::new(conn).await.ok();

        let per_app_futs = apps.into_iter().map(|(app_dest, app_path)| {
            let conn = conn.clone();
            let dbus = dbus.clone();
            async move {
                let mut app_windows = Vec::new();
                if app_dest.is_empty() {
                    return app_windows;
                }
                let Ok(app_proxy) = accessible_proxy(&conn, app_dest.clone(), app_path).await
                else {
                    return app_windows;
                };

                let pid_fut = async {
                    let dbus = dbus.as_ref()?;
                    let name = zbus::names::BusName::try_from(app_dest.as_str()).ok()?;
                    dbus.get_connection_unix_process_id(name).await.ok()
                };
                let (app_name_res, pid, win_count_res) =
                    tokio::join!(app_proxy.name(), pid_fut, app_proxy.child_count());
                let app_name = app_name_res.unwrap_or_default();
                let win_count = win_count_res.unwrap_or(0).max(0);
                if win_count == 0 {
                    return app_windows;
                }

                let win_futs = (0..win_count).map(|w_idx| {
                    let conn = conn.clone();
                    let app_proxy = app_proxy.clone();
                    let app_name = app_name.clone();
                    async move {
                        let (win_dest, win_path) =
                            app_proxy.get_child_at_index(w_idx).await.ok()?;
                        if win_dest.is_empty() {
                            return None;
                        }
                        let win_proxy = accessible_proxy(&conn, win_dest.clone(), win_path.clone())
                            .await
                            .ok()?;
                        let (title_res, states_res) =
                            tokio::join!(win_proxy.name(), win_proxy.get_state());
                        let title = title_res.unwrap_or_default();
                        let states = states_res.unwrap_or_default();
                        let active = has_state(&states, ATSPI_STATE_ACTIVE);
                        let visible = has_state(&states, ATSPI_STATE_VISIBLE)
                            || has_state(&states, ATSPI_STATE_SHOWING);

                        if title.is_empty() && !visible {
                            return None;
                        }

                        let id = format!("{}:{}", win_dest, win_path.as_str());
                        Some((
                            WindowInfo {
                                id,
                                title,
                                app_name,
                                pid,
                                active,
                            },
                            win_dest,
                            win_path,
                        ))
                    }
                });

                app_windows.extend(join_all(win_futs).await.into_iter().flatten());
                app_windows
            }
        });

        let results = join_all(per_app_futs).await.into_iter().flatten().collect();
        Ok(results)
    }
}

#[async_trait]
impl AccessibilityProvider for LinuxProvider {
    async fn doctor(&self) -> DoctorReport {
        run_doctor().await
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let handles = self.enumerate_window_handles().await?;
        Ok(handles.into_iter().map(|(info, _, _)| info).collect())
    }

    async fn get_window_tree(&self, query: &TreeQuery) -> Result<RawWindowTree> {
        let handles = self.enumerate_window_handles().await?;
        let target = match &query.window_filter {
            Some(filter) => {
                let needle = filter.to_lowercase();
                handles
                    .into_iter()
                    .find(|(w, _, _)| {
                        w.id == *filter
                            || w.title.to_lowercase().contains(&needle)
                            || w.app_name.to_lowercase().contains(&needle)
                    })
                    .ok_or_else(|| AxdomError::WindowNotFound(filter.clone()))?
            }
            None => handles
                .into_iter()
                .find(|(w, _, _)| w.active)
                .ok_or(AxdomError::NoActiveWindow)?,
        };

        let (window, dest, path) = target;
        let conn = self.connection();

        // Fast path: single D-Bus round-trip via `org.a11y.atspi.Cache.GetItems`.
        if let Ok(Some(cached_tree)) = tokio::time::timeout(
            Duration::from_millis(query.timeout_ms),
            fetch_window_tree_via_cache(&conn, &window, &dest, &path, query),
        )
        .await
        {
            return Ok(cached_tree);
        }

        let ctx = Arc::new(WalkContext {
            conn,
            semaphore: Semaphore::new(MAX_CONCURRENT_CALLS),
            visited: AtomicUsize::new(0),
            truncated: AtomicBool::new(false),
            max_depth: query.max_depth,
            max_nodes: query.max_nodes,
        });

        let walk_fut = walk_node(Arc::clone(&ctx), dest, path, 0);
        let root = tokio::time::timeout(Duration::from_millis(query.timeout_ms), walk_fut)
            .await
            .map_err(|_| AxdomError::Timeout(query.timeout_ms))?
            .ok_or_else(|| AxdomError::Backend("failed to inspect window root".to_string()))?;

        Ok(RawWindowTree {
            window,
            root,
            total_raw_nodes: ctx.visited.load(Ordering::Relaxed),
            truncated: ctx.truncated.load(Ordering::Relaxed),
        })
    }

    async fn verify_node(&self, node: &NodeRef) -> Result<()> {
        let conn = self.connection();
        verify_live_node(&conn, node).await
    }

    async fn perform_action(
        &self,
        node: &NodeRef,
        action: &ActionRequest,
        verify: bool,
    ) -> Result<ActionResult> {
        let conn = self.connection();
        dispatch_action(&conn, node, action, verify).await
    }

    async fn press_key(&self, combo: &KeyCombo) -> Result<ActionResult> {
        send_key_combo(combo).await
    }

    async fn capture_screenshot(&self, bounds: Option<Bounds>) -> Result<Screenshot> {
        capture_screen_png(bounds).await
    }
}

struct WalkContext {
    conn: Connection,
    semaphore: Semaphore,
    visited: AtomicUsize,
    truncated: AtomicBool,
    max_depth: usize,
    max_nodes: usize,
}

fn walk_node(
    ctx: Arc<WalkContext>,
    dest: String,
    path: OwnedObjectPath,
    depth: usize,
) -> BoxFuture<'static, Option<RawNode>> {
    Box::pin(async move {
        if dest.is_empty() {
            return None;
        }
        let count = ctx.visited.fetch_add(1, Ordering::Relaxed);
        if count >= ctx.max_nodes {
            ctx.truncated.store(true, Ordering::Relaxed);
            return None;
        }

        let _permit = ctx.semaphore.acquire().await.ok()?;

        let proxy = accessible_proxy(&ctx.conn, dest.clone(), path.clone())
            .await
            .ok()?;

        let (name_res, desc_res, role_res, state_res, ifaces_res, child_count_res) = tokio::join!(
            proxy.name(),
            proxy.description(),
            proxy.get_role(),
            proxy.get_state(),
            proxy.get_interfaces(),
            proxy.child_count(),
        );

        let state_bits = state_res.unwrap_or_default();
        let state = decode_state(&state_bits);

        // Early pruning: skip descending into hidden/offscreen subtrees to save D-Bus round-trips.
        if depth > 0 && !state.visible && !state.showing {
            return None;
        }

        let name = name_res.unwrap_or_default();
        let description = desc_res.ok().filter(|d| !d.trim().is_empty());
        let role_id = role_res.unwrap_or(0);
        let role = map_atspi_role(role_id);
        let interfaces = ifaces_res.unwrap_or_default();
        let child_count = child_count_res.unwrap_or(0).max(0);

        // Lazy bounds: only fetch root window extents during tree walk (for blind-spot screenshot
        // cropping). Per-node bounds are fetched on demand via `fetch_live_bounds` during fallback.
        let bounds = if depth == 0
            && has_interface(&interfaces, "Component")
            && let Ok(comp) = component_proxy(&ctx.conn, dest.clone(), path.clone()).await
        {
            comp.get_extents(0)
                .await
                .ok()
                .map(|(x, y, width, height)| Bounds {
                    x,
                    y,
                    width,
                    height,
                })
        } else {
            None
        };

        // Fetch text/numeric value for inputs and sliders (password redaction happens in Pruner)
        let value = fetch_value(&ctx.conn, &dest, &path, &interfaces, &role).await;

        // Collect child references before releasing the semaphore permit
        let mut child_refs = Vec::new();
        if depth < ctx.max_depth && child_count > 0 {
            let cap = (child_count as usize).min(256);
            if (child_count as usize) > cap {
                ctx.truncated.store(true, Ordering::Relaxed);
            }
            let futs = (0..cap as i32).map(|i| {
                let p = proxy.clone();
                async move { p.get_child_at_index(i).await.ok() }
            });
            child_refs = join_all(futs).await.into_iter().flatten().collect();
        } else if child_count > 0 {
            ctx.truncated.store(true, Ordering::Relaxed);
        }

        drop(_permit);

        let children: Vec<RawNode> = if !child_refs.is_empty() {
            let child_futs = child_refs
                .into_iter()
                .map(|(c_dest, c_path)| walk_node(Arc::clone(&ctx), c_dest, c_path, depth + 1));
            join_all(child_futs).await.into_iter().flatten().collect()
        } else {
            Vec::new()
        };

        Some(RawNode {
            backend_ref: format!("{}:{}", dest, path.as_str()),
            role,
            name,
            description,
            value,
            state,
            bounds,
            children,
        })
    })
}

/// Matches both short (`"Text"`) and full AT-SPI2 D-Bus interface names (`"org.a11y.atspi.Text"`).
pub(crate) fn has_interface(interfaces: &[String], short: &str) -> bool {
    let suffix = format!(".{short}");
    interfaces
        .iter()
        .any(|i| i == short || i.ends_with(&suffix))
}

pub(crate) async fn fetch_value(
    conn: &Connection,
    dest: &str,
    path: &OwnedObjectPath,
    interfaces: &[String],
    role: &NodeRole,
) -> Option<String> {
    // Only query Text/Value on roles where values are meaningful to avoid extra D-Bus chatter.
    let wants_text = matches!(
        role,
        NodeRole::Input | NodeRole::PasswordInput | NodeRole::ComboBox | NodeRole::Text
    );
    if wants_text
        && has_interface(interfaces, "Text")
        && let Ok(tp) = text_proxy(conn, dest.to_string(), path.clone()).await
    {
        let count = tp.character_count().await.unwrap_or(0);
        if count > 0 {
            let end = count.min(256);
            if let Ok(text) = tp.get_text(0, end).await
                && !text.trim().is_empty()
            {
                return Some(text);
            }
        }
    }

    if matches!(role, NodeRole::Slider)
        && has_interface(interfaces, "Value")
        && let Ok(vp) = value_proxy(conn, dest.to_string(), path.clone()).await
        && let Ok(v) = vp.current_value().await
    {
        return Some(format!("{v}"));
    }

    None
}

// Exact AT-SPI2 state enum bit indices from `/usr/include/at-spi-2.0/atspi/atspi-constants.h`
const ATSPI_STATE_ACTIVE: u32 = 1;
const ATSPI_STATE_CHECKED: u32 = 4;
const ATSPI_STATE_EDITABLE: u32 = 7;
const ATSPI_STATE_ENABLED: u32 = 8;
const ATSPI_STATE_EXPANDED: u32 = 10;
const ATSPI_STATE_FOCUSABLE: u32 = 11;
const ATSPI_STATE_FOCUSED: u32 = 12;
const ATSPI_STATE_SELECTED: u32 = 23;
const ATSPI_STATE_SENSITIVE: u32 = 24;
const ATSPI_STATE_SHOWING: u32 = 25;
const ATSPI_STATE_VISIBLE: u32 = 30;

fn has_state(bits: &[u32], state_idx: u32) -> bool {
    let word = (state_idx / 32) as usize;
    let bit = state_idx % 32;
    bits.get(word).is_some_and(|w| (w & (1 << bit)) != 0)
}

pub(crate) fn decode_state(bits: &[u32]) -> NodeState {
    NodeState {
        enabled: has_state(bits, ATSPI_STATE_ENABLED),
        visible: has_state(bits, ATSPI_STATE_VISIBLE),
        showing: has_state(bits, ATSPI_STATE_SHOWING),
        focused: has_state(bits, ATSPI_STATE_FOCUSED),
        focusable: has_state(bits, ATSPI_STATE_FOCUSABLE),
        selected: has_state(bits, ATSPI_STATE_SELECTED),
        checked: has_state(bits, ATSPI_STATE_CHECKED),
        expanded: has_state(bits, ATSPI_STATE_EXPANDED),
        editable: has_state(bits, ATSPI_STATE_EDITABLE),
        sensitive: has_state(bits, ATSPI_STATE_SENSITIVE),
    }
}

/// Maps exact AT-SPI2 `AtspiRole` numeric IDs (from `atspi-constants.h`) to `NodeRole`.
pub(crate) fn map_atspi_role(role: u32) -> NodeRole {
    match role {
        7 | 8 | 130 => NodeRole::CheckBox, // CheckBox, CheckMenuItem, Switch
        11 => NodeRole::ComboBox,
        16 => NodeRole::Dialog,
        23 | 69 => NodeRole::Window, // Frame, Window
        27 => NodeRole::Image,
        29 | 73 | 116 => NodeRole::Text, // Label, Paragraph, Static
        31 | 98 => NodeRole::List,       // List, ListBox
        32 => NodeRole::ListItem,
        33 | 34 | 41 => NodeRole::Menu, // Menu, MenuBar, PopupMenu
        35 => NodeRole::MenuItem,
        37 => NodeRole::Tab,     // PageTab
        38 => NodeRole::TabList, // PageTabList
        40 => NodeRole::PasswordInput,
        43 | 129 => NodeRole::Button, // Button/PushButton, PushButtonMenu
        44 | 45 => NodeRole::RadioButton, // RadioButton, RadioMenuItem
        48 => NodeRole::ScrollBar,
        51 => NodeRole::Slider,
        52 | 60 | 61 | 79 => NodeRole::Input, // SpinButton, Terminal, Text, Entry
        55 => NodeRole::Table,
        56 => NodeRole::Cell, // TableCell
        62 => NodeRole::ToggleButton,
        63 => NodeRole::Toolbar,
        65 | 66 => NodeRole::Tree, // Tree, TreeTable
        83 => NodeRole::Heading,
        88 => NodeRole::Link,
        91 => NodeRole::TreeItem,
        // Pure layout/filler/document containers (Filler, Panel, ScrollPane, Separator, SplitPane, Viewport, DocumentFrame, Section, DocumentWeb, Grouping)
        20 | 39 | 49 | 50 | 53 | 68 | 82 | 85 | 95 | 99 => NodeRole::Container,
        other => NodeRole::Unknown(format!("atspi_role_{other}")),
    }
}
