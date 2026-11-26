use axdom_core::{ActionRequest, ActionResult, AxdomError, NodeRef, Result};
use zbus::Connection;
use zbus::zvariant::OwnedObjectPath;

use crate::input::{
    send_coord_click, send_double_click, send_drag, send_hover, send_right_click, send_scroll,
};
use crate::portal::type_text_via_portal;
use crate::provider::{has_interface, map_atspi_role};
use crate::proxy::{
    accessible_proxy, action_proxy, component_proxy, editable_text_proxy, value_proxy,
};

/// Priority order for normalizing "click" across GTK, Qt, Firefox, Electron, and LibreOffice.
const CLICK_ACTION_PRIORITY: &[&str] = &[
    "click", "press", "activate", "jump", "open", "toggle", "select",
];

/// Non-click helper actions to skip when falling back to localized action names (e.g. "Tıkla").
const NON_CLICK_ACTIONS: &[&str] = &[
    "setfocus",
    "set focus",
    "showmenu",
    "show menu",
    "showlongdesc",
];

/// Splits a Linux `backend_ref` formatted as `<bus_name>:<object_path>`.
pub fn parse_backend_ref(backend_ref: &str) -> Result<(String, OwnedObjectPath)> {
    let idx = backend_ref
        .find(":/")
        .ok_or_else(|| AxdomError::Backend(format!("invalid backend_ref: {backend_ref}")))?;
    let dest = backend_ref[..idx].to_string();
    let path_str = &backend_ref[idx + 1..];
    let path = OwnedObjectPath::try_from(path_str)
        .map_err(|e| AxdomError::Backend(format!("invalid object path '{path_str}': {e}")))?;
    Ok((dest, path))
}

/// Verifies that the live AT-SPI2 object still matches the snapshotted role and name.
pub async fn verify_live_node(conn: &Connection, node: &NodeRef) -> Result<()> {
    let (dest, path) = parse_backend_ref(&node.backend_ref)?;
    let proxy = accessible_proxy(conn, dest, path)
        .await
        .map_err(|e| AxdomError::Backend(e.to_string()))?;

    let (role_res, name_res, desc_res) =
        tokio::join!(proxy.get_role(), proxy.name(), proxy.description());
    let live_role = map_atspi_role(role_res.map_err(|_| AxdomError::StaleNode {
        id: node.id.clone(),
        expected: format!("{} {:?}", node.role.compact_label(), node.name),
        actual: "<unreachable>".to_string(),
    })?);
    let raw_name = name_res.unwrap_or_default();
    let raw_desc = desc_res.unwrap_or_default();
    let live_name = if !raw_name.trim().is_empty() {
        raw_name.trim().to_string()
    } else {
        raw_desc.trim().to_string()
    };

    if live_role != node.role || (!node.name.is_empty() && live_name != node.name) {
        return Err(AxdomError::StaleNode {
            id: node.id.clone(),
            expected: format!("{} {:?}", node.role.compact_label(), node.name),
            actual: format!("{} {:?}", live_role.compact_label(), live_name),
        });
    }

    Ok(())
}

/// Fetches live screen bounds for a node via `Component.GetExtents(0)` (useful when the
/// snapshot came from `Cache.GetItems`, which omits bounding boxes).
pub async fn fetch_live_bounds(conn: &Connection, backend_ref: &str) -> Option<axdom_core::Bounds> {
    let (dest, path) = parse_backend_ref(backend_ref).ok()?;
    let comp = component_proxy(conn, dest, path).await.ok()?;
    comp.get_extents(0)
        .await
        .ok()
        .map(|(x, y, width, height)| axdom_core::Bounds {
            x,
            y,
            width,
            height,
        })
        .filter(|b| !b.is_zero_sized())
}

async fn resolve_node_center(conn: &Connection, node: &NodeRef) -> Result<(i32, i32)> {
    let bounds = match node.bounds.filter(|b| !b.is_zero_sized()) {
        Some(b) => Some(b),
        None => fetch_live_bounds(conn, &node.backend_ref).await,
    };
    let Some(b) = bounds else {
        return Err(AxdomError::ActionUnsupported(format!(
            "node {} has no valid screen bounds for pointer action",
            node.id
        )));
    };
    Ok((b.x + b.width / 2, b.y + b.height / 2))
}

/// Dispatches an `ActionRequest` directly over AT-SPI2 (coordinate-free, Wayland-safe).
pub async fn dispatch_action(
    conn: &Connection,
    node: &NodeRef,
    action: &ActionRequest,
    verify: bool,
) -> Result<ActionResult> {
    if let ActionRequest::ClickCoords { x, y } = action
        && (*x != 0 || *y != 0)
        && node.backend_ref.is_empty()
    {
        return send_coord_click(&node.id, *x, *y).await;
    }

    if verify {
        verify_live_node(conn, node).await?;
    }

    let (dest, path) = parse_backend_ref(&node.backend_ref)?;

    match action {
        ActionRequest::Click => dispatch_click(conn, node, &dest, &path).await,
        ActionRequest::RightClick => {
            let (x, y) = resolve_node_center(conn, node).await?;
            send_right_click(&node.id, x, y).await
        }
        ActionRequest::DoubleClick => {
            let (x, y) = resolve_node_center(conn, node).await?;
            send_double_click(&node.id, x, y).await
        }
        ActionRequest::Hover => {
            let (x, y) = resolve_node_center(conn, node).await?;
            send_hover(&node.id, x, y).await
        }
        ActionRequest::DragTo { to_x, to_y } => {
            let (from_x, from_y) = resolve_node_center(conn, node).await?;
            send_drag(&node.id, from_x, from_y, *to_x, *to_y).await
        }
        ActionRequest::ClickCoords { x, y } => {
            if *x == 0 && *y == 0 {
                let (cx, cy) = resolve_node_center(conn, node).await?;
                return send_coord_click(&node.id, cx, cy).await;
            }
            send_coord_click(&node.id, *x, *y).await
        }
        ActionRequest::Focus => dispatch_focus(conn, node, &dest, &path).await,
        ActionRequest::SetText { text } => dispatch_set_text(conn, node, &dest, &path, text).await,
        ActionRequest::SetValue { value } => {
            dispatch_set_value(conn, node, &dest, &path, *value).await
        }
        ActionRequest::Scroll { delta_x, delta_y } => {
            // Focus the target widget first (best-effort) so scroll goes to the right pane.
            if let Ok(comp) = component_proxy(conn, dest, path).await {
                let _ = comp.grab_focus().await;
            }
            send_scroll(&node.id, *delta_x, *delta_y).await
        }
    }
}

async fn dispatch_click(
    conn: &Connection,
    node: &NodeRef,
    dest: &str,
    path: &OwnedObjectPath,
) -> Result<ActionResult> {
    let proxy = action_proxy(conn, dest.to_string(), path.clone())
        .await
        .map_err(|e| AxdomError::Backend(e.to_string()))?;

    let count = proxy.n_actions().await.unwrap_or(0).max(0);
    if count == 0 {
        let fallback_hint = node.bounds.map(|b| {
            format!(
                " (center fallback coordinates: {}, {})",
                b.x + b.width / 2,
                b.y + b.height / 2
            )
        });
        return Err(AxdomError::ActionUnsupported(format!(
            "node {} exposes 0 AT-SPI2 actions{}",
            node.id,
            fallback_hint.unwrap_or_default()
        )));
    }

    let mut names = Vec::with_capacity(count as usize);
    for idx in 0..count {
        let name = proxy.get_name(idx).await.unwrap_or_default();
        names.push(name);
    }

    let chosen_idx = pick_click_action_index(&names).unwrap_or(0);
    let chosen_name = names
        .get(chosen_idx)
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("#{chosen_idx}"));

    let ok = proxy
        .do_action(chosen_idx as i32)
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("DoAction({chosen_name}): {e}")))?;

    Ok(ActionResult {
        node_id: node.id.clone(),
        action: "click".to_string(),
        method_used: format!("atspi:Action.DoAction({chosen_name})"),
        success: ok,
        detail: Some(format!("available actions: [{}]", names.join(", "))),
    })
}

async fn dispatch_focus(
    conn: &Connection,
    node: &NodeRef,
    dest: &str,
    path: &OwnedObjectPath,
) -> Result<ActionResult> {
    let comp_proxy = component_proxy(conn, dest.to_string(), path.clone())
        .await
        .map_err(|e| AxdomError::Backend(e.to_string()))?;

    let ok = comp_proxy
        .grab_focus()
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("Component.GrabFocus: {e}")))?;

    Ok(ActionResult {
        node_id: node.id.clone(),
        action: "focus".to_string(),
        method_used: "atspi:Component.GrabFocus".to_string(),
        success: ok,
        detail: None,
    })
}

async fn dispatch_set_text(
    conn: &Connection,
    node: &NodeRef,
    dest: &str,
    path: &OwnedObjectPath,
    text: &str,
) -> Result<ActionResult> {
    // Verify the target actually implements EditableText (Qt buttons otherwise silently return true).
    let acc = accessible_proxy(conn, dest.to_string(), path.clone())
        .await
        .map_err(|e| AxdomError::Backend(e.to_string()))?;
    let ifaces = acc.get_interfaces().await.unwrap_or_default();
    if !has_interface(&ifaces, "EditableText") {
        // For rich contenteditable / JS editors (e.g. Monaco, Notion) that expose Input role
        // without AT-SPI2 EditableText, focus and fall back to XDG RemoteDesktop portal typing.
        if matches!(
            node.role,
            axdom_core::NodeRole::Input
                | axdom_core::NodeRole::PasswordInput
                | axdom_core::NodeRole::ComboBox
        ) {
            if let Ok(comp) = component_proxy(conn, dest.to_string(), path.clone()).await {
                let _ = comp.grab_focus().await;
            }
            if let Ok(res) = type_text_via_portal(&node.id, text, true).await {
                return Ok(res);
            }
        }
        return Err(AxdomError::ActionUnsupported(format!(
            "node {} ({} {:?}) does not implement AT-SPI2 EditableText",
            node.id,
            node.role.compact_label(),
            node.name
        )));
    }

    // Best-effort focus before setting text contents.
    if let Ok(comp) = component_proxy(conn, dest.to_string(), path.clone()).await {
        let _ = comp.grab_focus().await;
    }

    let edit_proxy = editable_text_proxy(conn, dest.to_string(), path.clone())
        .await
        .map_err(|e| AxdomError::Backend(e.to_string()))?;

    let char_count = text.chars().count();
    match edit_proxy.set_text_contents(text).await {
        Ok(true) => Ok(ActionResult {
            node_id: node.id.clone(),
            action: "set_text".to_string(),
            method_used: "atspi:EditableText.SetTextContents".to_string(),
            success: true,
            detail: Some(format!("wrote {char_count} chars")),
        }),
        _ => {
            // Fallback: DeleteText + InsertText for toolkits where SetTextContents is a no-op.
            let _ = edit_proxy.delete_text(0, i32::MAX).await;
            let ok = edit_proxy
                .insert_text(0, text, char_count as i32)
                .await
                .map_err(|e| {
                    AxdomError::ActionFailed(format!("EditableText.InsertText fallback: {e}"))
                })?;
            Ok(ActionResult {
                node_id: node.id.clone(),
                action: "set_text".to_string(),
                method_used: "atspi:EditableText.Delete+InsertText".to_string(),
                success: ok,
                detail: Some(format!("wrote {char_count} chars")),
            })
        }
    }
}

async fn dispatch_set_value(
    conn: &Connection,
    node: &NodeRef,
    dest: &str,
    path: &OwnedObjectPath,
    value: f64,
) -> Result<ActionResult> {
    let val_proxy = value_proxy(conn, dest.to_string(), path.clone())
        .await
        .map_err(|e| AxdomError::Backend(e.to_string()))?;

    val_proxy
        .set_current_value(value)
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("Value.SetCurrentValue: {e}")))?;

    Ok(ActionResult {
        node_id: node.id.clone(),
        action: "set_value".to_string(),
        method_used: "atspi:Value.SetCurrentValue".to_string(),
        success: true,
        detail: Some(format!("value={value}")),
    })
}

/// Picks the best AT-SPI2 action index for a semantic "click" across different toolkits and locales.
pub fn pick_click_action_index(action_names: &[String]) -> Option<usize> {
    if action_names.is_empty() {
        return None;
    }
    for wanted in CLICK_ACTION_PRIORITY {
        if let Some((idx, _)) = action_names
            .iter()
            .enumerate()
            .find(|(_, name)| name.trim().eq_ignore_ascii_case(wanted))
        {
            return Some(idx);
        }
    }
    // Localized fallback (e.g. Turkish "Tıkla"): pick the first action that isn't SetFocus/ShowMenu.
    if let Some((idx, _)) = action_names.iter().enumerate().find(|(_, name)| {
        let lower = name.trim().to_lowercase();
        !lower.is_empty() && !NON_CLICK_ACTIONS.iter().any(|skip| lower == *skip)
    }) {
        return Some(idx);
    }
    Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_backend_ref() {
        let (dest, path) =
            parse_backend_ref(":1.42:/org/a11y/atspi/accessible/123").expect("valid ref");
        assert_eq!(dest, ":1.42");
        assert_eq!(path.as_str(), "/org/a11y/atspi/accessible/123");
        assert!(parse_backend_ref("invalid-ref").is_err());
    }

    #[test]
    fn normalizes_toolkit_click_actions() {
        let firefox_link = vec!["showlongdesc".to_string(), "jump".to_string()];
        assert_eq!(pick_click_action_index(&firefox_link), Some(1));

        let gtk_btn = vec!["press".to_string(), "click".to_string()];
        assert_eq!(pick_click_action_index(&gtk_btn), Some(1));

        let custom = vec!["custom-action".to_string()];
        assert_eq!(pick_click_action_index(&custom), Some(0));

        assert_eq!(pick_click_action_index(&[]), None);
    }
}
