use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axdom_core::{
    AccessibilityProvider, ActionRequest, AppPolicy, AxdomError, KeyCombo, OutputFormat, Pruner,
    PrunerConfig, SnapshotDiff, TreeQuery, TreeSnapshot, diff_snapshots, find_node_ref,
    format_snapshot, format_windows, scope_snapshot,
};
use axdom_daemon::{DaemonClient, DaemonRequest};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::protocol::{
    JsonRpcRequest, JsonRpcResponse, MCP_PROTOCOL_VERSION, tool_text_and_image_result,
    tool_text_result,
};

/// Short settle window after a mutating action so newly opened dialogs/toolbars render before diffing.
const POST_ACTION_SETTLE: Duration = Duration::from_millis(50);

/// Generic MCP server wrapping any `AccessibilityProvider`, with optional daemon fast-path.
pub struct McpServer<P: AccessibilityProvider> {
    provider: Arc<P>,
    policy: AppPolicy,
    pruner: Pruner,
    daemon_socket: Option<PathBuf>,
    last_snapshot: Mutex<Option<TreeSnapshot>>,
}

impl<P: AccessibilityProvider> McpServer<P> {
    pub fn new(provider: Arc<P>, policy: AppPolicy) -> Self {
        Self {
            provider,
            policy,
            pruner: Pruner::new(PrunerConfig::default()),
            daemon_socket: None,
            last_snapshot: Mutex::new(None),
        }
    }

    /// Enables transparent routing through a running `axdom daemon` Unix socket when reachable.
    pub fn with_daemon_socket(mut self, socket_path: PathBuf) -> Self {
        self.daemon_socket = Some(socket_path);
        self
    }

    async fn try_daemon(&self) -> Option<DaemonClient> {
        let path = self.daemon_socket.as_deref()?;
        DaemonClient::connect(path).await.ok()
    }

    /// Handles a single JSON-RPC 2.0 message. Returns `None` for notifications (no `id`).
    pub async fn handle_request(&self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        // Notifications have no `id` and must not receive a response.
        let id = req.id.clone()?;

        let response = match req.method.as_str() {
            "initialize" => JsonRpcResponse::ok(
                id,
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "axdom",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            ),
            "ping" => JsonRpcResponse::ok(id, json!({})),
            "tools/list" => JsonRpcResponse::ok(id, json!({ "tools": tool_definitions() })),
            "tools/call" => self.handle_tool_call(id, req.params).await,
            other => JsonRpcResponse::err(id, -32601, format!("method not found: {other}")),
        };

        Some(response)
    }

    async fn handle_tool_call(&self, id: Value, params: Option<Value>) -> JsonRpcResponse {
        let Some(params) = params else {
            return JsonRpcResponse::err(id, -32602, "missing params for tools/call");
        };
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return JsonRpcResponse::err(id, -32602, "missing tool name");
        };
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let payload = match name {
            "list_windows" => wrap_text(self.tool_list_windows().await),
            "get_desktop_tree" => self.tool_get_desktop_tree(&args).await,
            "click_node" => wrap_text(self.tool_click_node(&args).await),
            "right_click_node" => wrap_text(self.tool_right_click_node(&args).await),
            "double_click_node" => wrap_text(self.tool_double_click_node(&args).await),
            "hover_node" => wrap_text(self.tool_hover_node(&args).await),
            "drag_node" => wrap_text(self.tool_drag_node(&args).await),
            "click_coords" => wrap_text(self.tool_click_coords(&args).await),
            "type_text" => wrap_text(self.tool_type_text(&args).await),
            "set_value" => wrap_text(self.tool_set_value(&args).await),
            "scroll_node" => wrap_text(self.tool_scroll_node(&args).await),
            "focus_node" => wrap_text(self.tool_focus_node(&args).await),
            "press_key" => wrap_text(self.tool_press_key(&args).await),
            "capture_screenshot" => self.tool_capture_screenshot(&args).await,
            unknown => return JsonRpcResponse::err(id, -32602, format!("unknown tool: {unknown}")),
        };

        JsonRpcResponse::ok(id, payload)
    }

    async fn tool_list_windows(&self) -> Result<String, String> {
        let windows = if let Some(mut daemon) = self.try_daemon().await {
            daemon.list_windows().await.map_err(|e| e.to_string())?
        } else {
            self.provider
                .list_windows()
                .await
                .map_err(|e| e.to_string())?
        };
        let allowed: Vec<_> = windows
            .into_iter()
            .filter(|w| self.policy.check_read(w).is_ok())
            .collect();
        if allowed.is_empty() {
            return Ok("No accessible windows found.".to_string());
        }
        Ok(format_windows(&allowed, OutputFormat::Compact))
    }

    async fn tool_get_desktop_tree(&self, args: &Value) -> Value {
        let query = TreeQuery {
            window_filter: args
                .get("window")
                .and_then(Value::as_str)
                .map(str::to_string),
            max_depth: args
                .get("depth")
                .and_then(Value::as_u64)
                .map(|v| v as usize)
                .unwrap_or(12),
            max_nodes: args
                .get("max_nodes")
                .and_then(Value::as_u64)
                .map(|v| v as usize)
                .unwrap_or(1500),
            timeout_ms: args
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(2000),
        };
        let root_scope = args.get("root").and_then(Value::as_str);
        let fallback_image = args
            .get("include_screenshot_on_blind_spot")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let (snapshot, root_bounds) = if let Some(mut daemon) = self.try_daemon().await {
            match daemon.get_tree(&query, false).await {
                Ok((snap, _, _)) => {
                    if let Err(e) = self.policy.check_read(&snap.window) {
                        return tool_text_result(e.to_string(), true);
                    }
                    (snap, None)
                }
                Err(e) => return tool_text_result(e.to_string(), true),
            }
        } else {
            let raw = match self.provider.get_window_tree(&query).await {
                Ok(r) => r,
                Err(e) => return tool_text_result(e.to_string(), true),
            };
            if let Err(e) = self.policy.check_read(&raw.window) {
                return tool_text_result(e.to_string(), true);
            }
            let bounds = raw.root.bounds;
            (self.pruner.prune(raw), bounds)
        };

        let blind_spot = snapshot.blind_spot;
        *self.last_snapshot.lock().await = Some(snapshot.clone());

        let display_snapshot = match root_scope {
            Some(root_id) => match scope_snapshot(&snapshot, root_id) {
                Ok(scoped) => scoped,
                Err(e) => return tool_text_result(e.to_string(), true),
            },
            None => snapshot,
        };

        let text = format_snapshot(&display_snapshot, OutputFormat::Compact);

        if blind_spot
            && fallback_image
            && let Ok(shot) = self.provider.capture_screenshot(root_bounds).await
        {
            return tool_text_and_image_result(text, &shot.mime_type, &shot.base64_data);
        }

        tool_text_result(text, false)
    }

    async fn tool_click_node(&self, args: &Value) -> Result<String, String> {
        let node_id = require_str(args, "node_id")?;
        let return_diff = args
            .get("return_diff")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let allow_fallback = args
            .get("allow_fallback")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.execute_node_action(node_id, ActionRequest::Click, return_diff, allow_fallback)
            .await
    }

    async fn tool_type_text(&self, args: &Value) -> Result<String, String> {
        let node_id = require_str(args, "node_id")?;
        let text = require_str(args, "text")?.to_string();
        let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
        let return_diff = args
            .get("return_diff")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        let mut out = self
            .execute_node_action(
                node_id,
                ActionRequest::SetText { text },
                return_diff && !submit,
                false,
            )
            .await?;

        if submit {
            let enter = KeyCombo::parse("Enter").map_err(|e| e.to_string())?;
            let key_res = if let Some(mut daemon) = self.try_daemon().await {
                daemon.press_key(enter).await.map_err(|e| e.to_string())?
            } else {
                self.provider
                    .press_key(&enter)
                    .await
                    .map_err(|e| e.to_string())?
            };
            let _ = write!(out, "\nsubmit(Enter) -> {}", key_res.method_used);
            if return_diff {
                tokio::time::sleep(POST_ACTION_SETTLE).await;
                let prev = self.last_snapshot.lock().await.clone();
                if let Some(prev) = prev {
                    let query = TreeQuery {
                        window_filter: Some(prev.window.id.clone()),
                        ..Default::default()
                    };
                    let refreshed = if let Some(mut daemon) = self.try_daemon().await {
                        daemon
                            .get_tree(&query, true)
                            .await
                            .ok()
                            .map(|(snap, _, _)| snap)
                    } else {
                        self.provider
                            .get_window_tree(&query)
                            .await
                            .ok()
                            .map(|raw| self.pruner.prune(raw))
                    };
                    if let Some(next_snapshot) = refreshed {
                        let diff = diff_snapshots(&prev, &next_snapshot);
                        *self.last_snapshot.lock().await = Some(next_snapshot);
                        append_diff(&mut out, &diff);
                    }
                }
            }
        }

        Ok(out)
    }

    async fn tool_right_click_node(&self, args: &Value) -> Result<String, String> {
        let node_id = require_str(args, "node_id")?;
        let return_diff = args
            .get("return_diff")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        self.execute_node_action(node_id, ActionRequest::RightClick, return_diff, false)
            .await
    }

    async fn tool_double_click_node(&self, args: &Value) -> Result<String, String> {
        let node_id = require_str(args, "node_id")?;
        let return_diff = args
            .get("return_diff")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        self.execute_node_action(node_id, ActionRequest::DoubleClick, return_diff, false)
            .await
    }

    async fn tool_hover_node(&self, args: &Value) -> Result<String, String> {
        let node_id = require_str(args, "node_id")?;
        self.execute_node_action(node_id, ActionRequest::Hover, false, false)
            .await
    }

    async fn tool_drag_node(&self, args: &Value) -> Result<String, String> {
        let from_id = require_str(args, "from_node_id")?;
        let return_diff = args
            .get("return_diff")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        let (to_x, to_y) =
            if let Some(to_id) = args.get("to_node_id").and_then(Value::as_str) {
                let snap = self.last_snapshot.lock().await.clone().ok_or_else(|| {
                    "no cached tree snapshot; call get_desktop_tree first".to_string()
                })?;
                let target_ref = find_node_ref(&snap, to_id).map_err(|e| e.to_string())?;
                let b = target_ref
                    .bounds
                    .filter(|b| !b.is_zero_sized())
                    .ok_or_else(|| {
                        format!("target node {to_id} has no valid screen bounds for drag_node")
                    })?;
                (b.x + b.width / 2, b.y + b.height / 2)
            } else {
                let x = args.get("to_x").and_then(Value::as_i64).ok_or_else(|| {
                    "provide either 'to_node_id' or ('to_x' and 'to_y')".to_string()
                })? as i32;
                let y = args.get("to_y").and_then(Value::as_i64).ok_or_else(|| {
                    "provide either 'to_node_id' or ('to_x' and 'to_y')".to_string()
                })? as i32;
                (x, y)
            };

        self.execute_node_action(
            from_id,
            ActionRequest::DragTo { to_x, to_y },
            return_diff,
            false,
        )
        .await
    }

    async fn tool_click_coords(&self, args: &Value) -> Result<String, String> {
        if self.policy.read_only {
            return Err(
                "policy denied: axdom is running in read-only mode; click_coords is disabled"
                    .to_string(),
            );
        }
        if let Some(prev) = self.last_snapshot.lock().await.as_ref() {
            self.policy
                .check_write(&prev.window)
                .map_err(|e| e.to_string())?;
        }
        let x = args
            .get("x")
            .and_then(Value::as_i64)
            .ok_or_else(|| "missing required integer argument 'x'".to_string())?
            as i32;
        let y = args
            .get("y")
            .and_then(Value::as_i64)
            .ok_or_else(|| "missing required integer argument 'y'".to_string())?
            as i32;
        let dummy = axdom_core::NodeRef {
            id: format!("({x},{y})"),
            backend_ref: String::new(),
            role: axdom_core::NodeRole::Container,
            name: String::new(),
            bounds: None,
        };
        let res = self
            .provider
            .perform_action(&dummy, &ActionRequest::ClickCoords { x, y }, false)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(mut daemon) = self.try_daemon().await {
            let _ = daemon.send(&DaemonRequest::Invalidate).await;
        }
        Ok(format!(
            "click_coords ({x},{y}) -> {} (success={})",
            res.method_used, res.success
        ))
    }

    async fn tool_scroll_node(&self, args: &Value) -> Result<String, String> {
        let node_id = require_str(args, "node_id")?;
        let delta_x = args.get("delta_x").and_then(Value::as_i64).unwrap_or(0) as i32;
        let delta_y = args.get("delta_y").and_then(Value::as_i64).unwrap_or(0) as i32;
        if delta_x == 0 && delta_y == 0 {
            return Err("provide a non-zero 'delta_y' and/or 'delta_x'".to_string());
        }
        let return_diff = args
            .get("return_diff")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        self.execute_node_action(
            node_id,
            ActionRequest::Scroll { delta_x, delta_y },
            return_diff,
            false,
        )
        .await
    }

    async fn tool_capture_screenshot(&self, args: &Value) -> Value {
        let full_screen = args
            .get("full_screen")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let som_grid = args
            .get("som_grid")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let node_id = args.get("node_id").and_then(Value::as_str);

        let snap = self.last_snapshot.lock().await.clone();
        if let Some(prev) = snap.as_ref() {
            if let Err(e) = self.policy.check_read(&prev.window) {
                return tool_text_result(e.to_string(), true);
            }
        } else if let Ok(wins) = self.provider.list_windows().await
            && let Some(active) = wins.iter().find(|w| w.active)
            && let Err(e) = self.policy.check_read(active)
        {
            return tool_text_result(e.to_string(), true);
        }

        // Privacy + token optimization: crop to target node or active window bounds by default.
        let crop_bounds = if full_screen {
            None
        } else if let Some(nid) = node_id {
            match snap.as_ref() {
                Some(s) => match find_node_ref(s, nid) {
                    Ok(nref) => nref.bounds.filter(|b| !b.is_zero_sized()),
                    Err(e) => return tool_text_result(e.to_string(), true),
                },
                None => {
                    return tool_text_result(
                        "no cached tree snapshot; call get_desktop_tree before cropping to node_id"
                            .to_string(),
                        true,
                    );
                }
            }
        } else {
            snap.as_ref()
                .and_then(|s| bounding_box_of_elements(&s.elements))
        };

        let shot_res = if let Some(mut daemon) = self.try_daemon().await {
            daemon.capture_screenshot(crop_bounds).await
        } else {
            self.provider.capture_screenshot(crop_bounds).await
        };

        match shot_res {
            Ok(shot) => {
                if som_grid {
                    let origin_x = crop_bounds.map(|b| b.x).unwrap_or(0);
                    let origin_y = crop_bounds.map(|b| b.y).unwrap_or(0);
                    if let Ok(raw_png) = axdom_core::decode_base64(&shot.base64_data)
                        && let Ok((annotated, cells)) =
                            axdom_core::annotate_som_grid(&raw_png, origin_x, origin_y, 4, 4)
                    {
                        let grid_map: Vec<String> = cells
                            .iter()
                            .map(|c| format!("{}=({},{})", c.label, c.center_x, c.center_y))
                            .collect();
                        let b64 = axdom_core::encode_base64(&annotated);
                        return tool_text_and_image_result(
                            format!(
                                "captured {} bytes via {} (SoM grid 4x4; use click_coords):\n{}",
                                annotated.len(),
                                shot.method_used,
                                grid_map.join(" ")
                            ),
                            &shot.mime_type,
                            &b64,
                        );
                    }
                }
                let scope_note = if crop_bounds.is_some() {
                    " (cropped)"
                } else {
                    " (full_screen)"
                };
                tool_text_and_image_result(
                    format!(
                        "captured {} bytes via {}{scope_note}",
                        shot.bytes_len, shot.method_used
                    ),
                    &shot.mime_type,
                    &shot.base64_data,
                )
            }
            Err(e) => tool_text_result(e.to_string(), true),
        }
    }

    async fn tool_focus_node(&self, args: &Value) -> Result<String, String> {
        let node_id = require_str(args, "node_id")?;
        self.execute_node_action(node_id, ActionRequest::Focus, false, false)
            .await
    }

    async fn tool_set_value(&self, args: &Value) -> Result<String, String> {
        let node_id = require_str(args, "node_id")?;
        let value = args
            .get("value")
            .and_then(Value::as_f64)
            .ok_or_else(|| "missing required numeric argument 'value'".to_string())?;
        let return_diff = args
            .get("return_diff")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        self.execute_node_action(
            node_id,
            ActionRequest::SetValue { value },
            return_diff,
            false,
        )
        .await
    }

    async fn tool_press_key(&self, args: &Value) -> Result<String, String> {
        if self.policy.read_only {
            return Err(
                "policy denied: axdom is running in read-only mode; press_key is disabled"
                    .to_string(),
            );
        }
        if let Some(prev) = self.last_snapshot.lock().await.as_ref() {
            self.policy
                .check_write(&prev.window)
                .map_err(|e| e.to_string())?;
        }
        let key_str = require_str(args, "key")?;
        let combo = KeyCombo::parse(key_str).map_err(|e| e.to_string())?;
        let result = self
            .provider
            .press_key(&combo)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(mut daemon) = self.try_daemon().await {
            let _ = daemon.send(&DaemonRequest::Invalidate).await;
        }
        Ok(format!(
            "press_key {:?} -> {} (success={})",
            key_str, result.method_used, result.success
        ))
    }

    async fn execute_node_action(
        &self,
        raw_id: &str,
        action: ActionRequest,
        return_diff: bool,
        allow_coord_fallback: bool,
    ) -> Result<String, String> {
        let prev_snapshot =
            self.last_snapshot.lock().await.clone().ok_or_else(|| {
                "no cached tree snapshot; call get_desktop_tree first".to_string()
            })?;

        self.policy
            .check_write(&prev_snapshot.window)
            .map_err(|e| e.to_string())?;

        let node_ref = find_node_ref(&prev_snapshot, raw_id).map_err(|e| e.to_string())?;
        let result = match self.provider.perform_action(&node_ref, &action, true).await {
            Ok(res) => res,
            Err(AxdomError::ActionUnsupported(_))
                if allow_coord_fallback && matches!(action, ActionRequest::Click) =>
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
                    .map_err(|e| e.to_string())?
            }
            Err(e) => return Err(e.to_string()),
        };

        let mut out = format!(
            "{} {} -> {} (success={})",
            result.action, result.node_id, result.method_used, result.success
        );
        if let Some(detail) = &result.detail {
            let _ = write!(out, " | {detail}");
        }

        if return_diff {
            tokio::time::sleep(POST_ACTION_SETTLE).await;
            let query = TreeQuery {
                window_filter: Some(prev_snapshot.window.id.clone()),
                ..Default::default()
            };
            let next_opt = if let Some(mut daemon) = self.try_daemon().await {
                daemon.get_tree(&query, true).await.ok().map(|(s, _, _)| s)
            } else {
                self.provider
                    .get_window_tree(&query)
                    .await
                    .ok()
                    .map(|raw| self.pruner.prune(raw))
            };
            if let Some(next_snapshot) = next_opt {
                let diff = diff_snapshots(&prev_snapshot, &next_snapshot);
                *self.last_snapshot.lock().await = Some(next_snapshot);
                append_diff(&mut out, &diff);
            }
        } else if let Some(mut daemon) = self.try_daemon().await {
            let _ = daemon.send(&DaemonRequest::Invalidate).await;
        }

        Ok(out)
    }
}

fn wrap_text(res: Result<String, String>) -> Value {
    match res {
        Ok(text) => tool_text_result(text, false),
        Err(err) => tool_text_result(err, true),
    }
}

fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing required string argument '{key}'"))
}

fn append_diff(out: &mut String, diff: &SnapshotDiff) {
    if diff.is_empty() {
        let _ = write!(out, "\ndiff: (no visible UI changes)");
        return;
    }
    let _ = write!(out, "\ndiff:");
    for item in &diff.added {
        let _ = write!(out, "\n  + {item}");
    }
    for item in &diff.changed {
        let _ = write!(out, "\n  ~ {item}");
    }
    for item in &diff.removed {
        let _ = write!(out, "\n  - {item}");
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "list_windows",
            "description": "List open desktop windows accessible via the OS accessibility tree (filtered by security policy).",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "get_desktop_tree",
            "description": "Read and prune the accessibility tree of the active (or specified) desktop window into a compact DOM with short stable IDs (#xxx). Passwords are automatically redacted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": {
                        "type": "string",
                        "description": "Optional window title substring, app name, or window ID. Defaults to the currently active window."
                    },
                    "root": {
                        "type": "string",
                        "description": "Optional node ID (#xxx) to scope the output to a specific subtree."
                    },
                    "depth": {
                        "type": "integer",
                        "description": "Maximum tree traversal depth (default: 12)."
                    },
                    "max_nodes": {
                        "type": "integer",
                        "description": "Maximum raw nodes to inspect before truncating (default: 1500)."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Traversal timeout budget in milliseconds (default: 2000)."
                    },
                    "include_screenshot_on_blind_spot": {
                        "type": "boolean",
                        "description": "When true and the window exposes too few accessibility nodes (canvas/game), attach a PNG screenshot image block."
                    }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "click_node",
            "description": "Programmatically click/activate a UI node from the latest get_desktop_tree snapshot without moving the mouse cursor.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_id": {
                        "type": "string",
                        "description": "Short node ID from get_desktop_tree (e.g. '#b4f')."
                    },
                    "return_diff": {
                        "type": "boolean",
                        "description": "Whether to re-inspect the window and append a compact UI diff (default: true)."
                    },
                    "allow_fallback": {
                        "type": "boolean",
                        "description": "If AT-SPI2 exposes no programmatic actions on the node, fall back to clicking its center coordinates (default: false)."
                    }
                },
                "required": ["node_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "type_text",
            "description": "Set text contents of an editable input node from the latest get_desktop_tree snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_id": {
                        "type": "string",
                        "description": "Short node ID from get_desktop_tree (e.g. '#a12')."
                    },
                    "text": {
                        "type": "string",
                        "description": "Text contents to write into the input field."
                    },
                    "submit": {
                        "type": "boolean",
                        "description": "When true, automatically press Enter right after typing (useful for search boxes and forms; default: false)."
                    },
                    "return_diff": {
                        "type": "boolean",
                        "description": "Whether to re-inspect the window and append a compact UI diff (default: true)."
                    }
                },
                "required": ["node_id", "text"],
                "additionalProperties": false
            }
        },
        {
            "name": "set_value",
            "description": "Set the numeric value of a slider or range UI control from the latest get_desktop_tree snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_id": {
                        "type": "string",
                        "description": "Short node ID from get_desktop_tree (e.g. '#c41')."
                    },
                    "value": {
                        "type": "number",
                        "description": "Target numeric value."
                    },
                    "return_diff": {
                        "type": "boolean",
                        "description": "Whether to re-inspect the window and append a compact UI diff (default: true)."
                    }
                },
                "required": ["node_id", "value"],
                "additionalProperties": false
            }
        },
        {
            "name": "right_click_node",
            "description": "Right-click a UI node (opens context menus) using its screen bounds.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_id": { "type": "string", "description": "Short node ID (e.g. '#b4f')." },
                    "return_diff": { "type": "boolean", "description": "Append compact UI diff after clicking (default: true)." }
                },
                "required": ["node_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "double_click_node",
            "description": "Double-click a UI node (e.g. open file/item) using its screen bounds.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_id": { "type": "string", "description": "Short node ID (e.g. '#b4f')." },
                    "return_diff": { "type": "boolean", "description": "Append compact UI diff after clicking (default: true)." }
                },
                "required": ["node_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "hover_node",
            "description": "Move the mouse cursor over a UI node to reveal hover menus or tooltips.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_id": { "type": "string", "description": "Short node ID (e.g. '#b4f')." }
                },
                "required": ["node_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "drag_node",
            "description": "Drag from a source UI node to either another node ('to_node_id') or screen coordinates ('to_x', 'to_y').",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from_node_id": { "type": "string", "description": "Source node ID (e.g. '#a12')." },
                    "to_node_id": { "type": "string", "description": "Optional destination node ID." },
                    "to_x": { "type": "integer", "description": "Optional destination screen X coordinate." },
                    "to_y": { "type": "integer", "description": "Optional destination screen Y coordinate." },
                    "return_diff": { "type": "boolean", "description": "Append compact UI diff after dragging (default: true)." }
                },
                "required": ["from_node_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "click_coords",
            "description": "Click at absolute screen coordinates (x, y) — designed for canvas/blind-spot Set-of-Mark grid cells.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": { "type": "integer", "description": "Screen X coordinate." },
                    "y": { "type": "integer", "description": "Screen Y coordinate." }
                },
                "required": ["x", "y"],
                "additionalProperties": false
            }
        },
        {
            "name": "scroll_node",
            "description": "Scroll a scrollable container or virtual-scrolled list vertically and/or horizontally and return the updated DOM diff.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_id": {
                        "type": "string",
                        "description": "Short node ID from get_desktop_tree to focus before scrolling."
                    },
                    "delta_x": {
                        "type": "integer",
                        "description": "Horizontal scroll steps (positive = right, negative = left, default: 0)."
                    },
                    "delta_y": {
                        "type": "integer",
                        "description": "Vertical scroll steps (positive = down, negative = up, default: 0)."
                    },
                    "return_diff": {
                        "type": "boolean",
                        "description": "Whether to re-inspect the window and append a compact UI diff (default: true)."
                    }
                },
                "required": ["node_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "capture_screenshot",
            "description": "Capture a PNG screenshot cropped to the active window (or a specific node_id) by default for privacy and token savings. Set som_grid=true to overlay a Set-of-Mark A1..D4 coordinate grid for canvas/blind-spot clicking.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_id": {
                        "type": "string",
                        "description": "Optional node ID (#xxx) to crop the screenshot tightly to that element."
                    },
                    "full_screen": {
                        "type": "boolean",
                        "description": "When true, capture the entire desktop instead of cropping to the active window (default: false)."
                    },
                    "som_grid": {
                        "type": "boolean",
                        "description": "When true, overlay a 4x4 Set-of-Mark grid (A1..D4) and return cell center coordinates for click_coords (default: false)."
                    }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "focus_node",
            "description": "Move keyboard focus to a UI node from the latest get_desktop_tree snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node_id": {
                        "type": "string",
                        "description": "Short node ID from get_desktop_tree (e.g. '#a12')."
                    }
                },
                "required": ["node_id"],
                "additionalProperties": false
            }
        },
        {
            "name": "press_key",
            "description": "Send a keyboard shortcut (e.g. 'Enter', 'Escape', 'Ctrl+S', 'Alt+F4') to the active desktop session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Key or modifier+key combination (e.g. 'Ctrl+S', 'Return', 'Tab')."
                    }
                },
                "required": ["key"],
                "additionalProperties": false
            }
        }
    ])
}

fn bounding_box_of_elements(nodes: &[axdom_core::PrunedNode]) -> Option<axdom_core::Bounds> {
    fn visit(nodes: &[axdom_core::PrunedNode], acc: &mut Option<(i32, i32, i32, i32)>) {
        for n in nodes {
            if let Some(b) = n.bounds.filter(|b| !b.is_zero_sized()) {
                let x2 = b.x + b.width;
                let y2 = b.y + b.height;
                *acc = Some(match *acc {
                    Some((min_x, min_y, max_x, max_y)) => {
                        (min_x.min(b.x), min_y.min(b.y), max_x.max(x2), max_y.max(y2))
                    }
                    None => (b.x, b.y, x2, y2),
                });
            }
            visit(&n.children, acc);
        }
    }
    let mut acc = None;
    visit(nodes, &mut acc);
    acc.map(|(min_x, min_y, max_x, max_y)| axdom_core::Bounds {
        x: min_x,
        y: min_y,
        width: (max_x - min_x).max(1),
        height: (max_y - min_y).max(1),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use axdom_core::provider::RawWindowTree;
    use axdom_core::{
        ActionResult, Bounds, DoctorReport, NodeRef, NodeRole, NodeState, RawNode, Result,
        Screenshot, WindowInfo,
    };

    use super::*;

    struct MockProvider {
        clicked: AtomicBool,
        app_name: String,
        empty_tree: bool,
    }

    impl MockProvider {
        fn new(app_name: &str) -> Self {
            Self {
                clicked: AtomicBool::new(false),
                app_name: app_name.to_string(),
                empty_tree: false,
            }
        }

        fn blind_spot(app_name: &str) -> Self {
            Self {
                clicked: AtomicBool::new(false),
                app_name: app_name.to_string(),
                empty_tree: true,
            }
        }
    }

    fn visible() -> NodeState {
        NodeState {
            enabled: true,
            visible: true,
            showing: true,
            sensitive: true,
            ..Default::default()
        }
    }

    #[async_trait]
    impl AccessibilityProvider for MockProvider {
        async fn doctor(&self) -> DoctorReport {
            DoctorReport { checks: vec![] }
        }

        async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
            Ok(vec![WindowInfo {
                id: "w1".into(),
                title: "Calc".into(),
                app_name: self.app_name.clone(),
                pid: Some(1234),
                active: true,
            }])
        }

        async fn get_window_tree(&self, _query: &TreeQuery) -> Result<RawWindowTree> {
            let mut children = if self.empty_tree {
                vec![]
            } else {
                vec![RawNode {
                    backend_ref: ":1.1:/btn/save".into(),
                    role: NodeRole::Button,
                    name: "Save".into(),
                    description: None,
                    value: None,
                    state: visible(),
                    bounds: Some(Bounds {
                        x: 10,
                        y: 10,
                        width: 80,
                        height: 30,
                    }),
                    children: vec![],
                }]
            };

            if self.clicked.load(Ordering::Relaxed) {
                children.push(RawNode {
                    backend_ref: ":1.1:/dlg/ok".into(),
                    role: NodeRole::Button,
                    name: "Confirm".into(),
                    description: None,
                    value: None,
                    state: visible(),
                    bounds: Some(Bounds {
                        x: 20,
                        y: 50,
                        width: 80,
                        height: 30,
                    }),
                    children: vec![],
                });
            }

            Ok(RawWindowTree {
                window: WindowInfo {
                    id: "w1".into(),
                    title: "Calc".into(),
                    app_name: self.app_name.clone(),
                    pid: Some(1234),
                    active: true,
                },
                root: RawNode {
                    backend_ref: ":1.1:/root".into(),
                    role: NodeRole::Window,
                    name: "Calc".into(),
                    description: None,
                    value: None,
                    state: visible(),
                    bounds: None,
                    children,
                },
                total_raw_nodes: 3,
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
            if matches!(action, ActionRequest::Click) {
                self.clicked.store(true, Ordering::Relaxed);
            }
            Ok(ActionResult {
                node_id: node.id.clone(),
                action: action.name().to_string(),
                method_used: "mock:click".to_string(),
                success: true,
                detail: None,
            })
        }

        async fn press_key(&self, combo: &KeyCombo) -> Result<ActionResult> {
            Ok(ActionResult {
                node_id: "keyboard".into(),
                action: "press_key".into(),
                method_used: format!("mock:key({})", combo.key),
                success: true,
                detail: None,
            })
        }

        async fn capture_screenshot(&self, _bounds: Option<Bounds>) -> Result<Screenshot> {
            Ok(Screenshot {
                mime_type: "image/png".into(),
                base64_data: "iVBORw0KGgo=".into(),
                method_used: "mock:screenshot".into(),
                bytes_len: 8,
            })
        }
    }

    #[tokio::test]
    async fn mcp_initialize_tree_and_click_with_diff() {
        let server = McpServer::new(
            Arc::new(MockProvider::new("libreoffice")),
            AppPolicy::default(),
        );

        let init_resp = server
            .handle_request(JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "initialize".into(),
                params: None,
            })
            .await
            .unwrap();
        assert!(init_resp.error.is_none());

        let tree_resp = server
            .handle_request(JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(2)),
                method: "tools/call".into(),
                params: Some(json!({ "name": "get_desktop_tree", "arguments": {} })),
            })
            .await
            .unwrap();
        let tree_text = tree_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(tree_text.contains("button \"Save\""));

        let start = tree_text.find("[#").unwrap() + 1;
        let end = tree_text[start..].find(']').unwrap() + start;
        let save_id = &tree_text[start..end];

        let click_resp = server
            .handle_request(JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(3)),
                method: "tools/call".into(),
                params: Some(json!({
                    "name": "click_node",
                    "arguments": { "node_id": save_id }
                })),
            })
            .await
            .unwrap();
        let click_text = click_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(click_text.contains("mock:click"));
        assert!(click_text.contains("button \"Confirm\""));
    }

    #[tokio::test]
    async fn mcp_blind_spot_attaches_screenshot_and_press_key_works() {
        let server = McpServer::new(
            Arc::new(MockProvider::blind_spot("blender")),
            AppPolicy::default(),
        );

        let resp = server
            .handle_request(JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "tools/call".into(),
                params: Some(json!({
                    "name": "get_desktop_tree",
                    "arguments": { "include_screenshot_on_blind_spot": true }
                })),
            })
            .await
            .unwrap();
        let content = resp.result.unwrap()["content"].as_array().unwrap().clone();
        assert_eq!(content.len(), 2);
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["mimeType"], "image/png");

        let key_resp = server
            .handle_request(JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(2)),
                method: "tools/call".into(),
                params: Some(json!({
                    "name": "press_key",
                    "arguments": { "key": "Ctrl+S" }
                })),
            })
            .await
            .unwrap();
        assert!(
            key_resp.result.unwrap()["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("mock:key(S)")
        );
    }

    #[tokio::test]
    async fn mcp_blocks_denied_password_manager() {
        let server = McpServer::new(
            Arc::new(MockProvider::new("keepassxc")),
            AppPolicy::default(),
        );
        let resp = server
            .handle_request(JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "tools/call".into(),
                params: Some(json!({ "name": "get_desktop_tree", "arguments": {} })),
            })
            .await
            .unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("policy denied")
        );
    }

    #[tokio::test]
    async fn mcp_type_submit_scroll_set_value_and_capture_screenshot() {
        let server = McpServer::new(
            Arc::new(MockProvider::new("libreoffice")),
            AppPolicy::default(),
        );

        let tree_resp = server
            .handle_request(JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "tools/call".into(),
                params: Some(json!({ "name": "get_desktop_tree", "arguments": {} })),
            })
            .await
            .unwrap();
        let tree_text = tree_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let start = tree_text.find("[#").unwrap() + 1;
        let end = tree_text[start..].find(']').unwrap() + start;
        let node_id = tree_text[start..end].to_string();

        // type_text with submit: true
        let type_resp = server
            .handle_request(JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(2)),
                method: "tools/call".into(),
                params: Some(json!({
                    "name": "type_text",
                    "arguments": { "node_id": node_id, "text": "hello world", "submit": true }
                })),
            })
            .await
            .unwrap();
        let type_text = type_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(type_text.contains("submit(Enter) -> mock:key(Enter)"));

        // scroll_node
        let scroll_resp = server
            .handle_request(JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(3)),
                method: "tools/call".into(),
                params: Some(json!({
                    "name": "scroll_node",
                    "arguments": { "node_id": node_id, "delta_y": 5, "return_diff": false }
                })),
            })
            .await
            .unwrap();
        assert_eq!(scroll_resp.result.unwrap()["isError"], false);

        // set_value
        let val_resp = server
            .handle_request(JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(4)),
                method: "tools/call".into(),
                params: Some(json!({
                    "name": "set_value",
                    "arguments": { "node_id": node_id, "value": 42.5, "return_diff": false }
                })),
            })
            .await
            .unwrap();
        assert_eq!(val_resp.result.unwrap()["isError"], false);

        // capture_screenshot
        let shot_resp = server
            .handle_request(JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(5)),
                method: "tools/call".into(),
                params: Some(json!({
                    "name": "capture_screenshot",
                    "arguments": {}
                })),
            })
            .await
            .unwrap();
        let shot_content = shot_resp.result.unwrap()["content"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(shot_content.len(), 2);
        assert_eq!(shot_content[1]["type"], "image");
        assert_eq!(shot_content[1]["mimeType"], "image/png");

        // right_click_node, double_click_node, hover_node, drag_node, click_coords
        for (idx, (tool, args)) in [
            (
                "right_click_node",
                json!({ "node_id": node_id, "return_diff": false }),
            ),
            (
                "double_click_node",
                json!({ "node_id": node_id, "return_diff": false }),
            ),
            ("hover_node", json!({ "node_id": node_id })),
            (
                "drag_node",
                json!({ "from_node_id": node_id, "to_x": 120, "to_y": 240, "return_diff": false }),
            ),
            ("click_coords", json!({ "x": 50, "y": 60 })),
            (
                "scroll_node",
                json!({ "node_id": node_id, "delta_x": 3, "delta_y": -2, "return_diff": false }),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let resp = server
                .handle_request(JsonRpcRequest {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(10 + idx)),
                    method: "tools/call".into(),
                    params: Some(json!({ "name": tool, "arguments": args })),
                })
                .await
                .unwrap();
            assert_eq!(resp.result.unwrap()["isError"], false, "tool {tool} failed");
        }
    }
}
