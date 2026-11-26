use std::path::PathBuf;

use axdom_core::{
    ActionResult, AxdomError, Bounds, KeyCombo, KeyModifier, Result, Screenshot, crop_png_bytes,
    encode_base64,
};
use tokio::process::Command;

use crate::portal::{
    BTN_LEFT, BTN_RIGHT, capture_screenshot_via_portal, send_key_via_portal,
    send_pointer_click_via_portal, send_pointer_drag_via_portal, send_pointer_hover_via_portal,
    send_scroll_via_portal,
};

/// Sends a keyboard shortcut via `wtype` (Wayland), native XDG Desktop Portal `RemoteDesktop`
/// (built into KDE KWin / GNOME Mutter / Hyprland — zero external tools), or `xdotool` (X11).
pub async fn send_key_combo(combo: &KeyCombo) -> Result<ActionResult> {
    let is_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();

    if is_wayland && command_exists("wtype") {
        let args = build_wtype_args(combo);
        run_tool("wtype", &args).await?;
        return Ok(ActionResult {
            node_id: "keyboard".to_string(),
            action: "press_key".to_string(),
            method_used: "fallback:wtype".to_string(),
            success: true,
            detail: Some(format!("args={args:?}")),
        });
    }

    if !is_wayland && command_exists("xdotool") {
        let key_spec = build_xdotool_spec(combo);
        let args = vec!["key".to_string(), "--clearmodifiers".to_string(), key_spec];
        run_tool("xdotool", &args).await?;
        return Ok(ActionResult {
            node_id: "keyboard".to_string(),
            action: "press_key".to_string(),
            method_used: "fallback:xdotool".to_string(),
            success: true,
            detail: Some(format!("args={args:?}")),
        });
    }

    // Native pure-Rust XDG Desktop Portal RemoteDesktop (works on Wayland without wtype/xdotool).
    match send_key_via_portal(combo).await {
        Ok(res) => Ok(res),
        Err(portal_err) => {
            if command_exists("xdotool") {
                let key_spec = build_xdotool_spec(combo);
                let args = vec!["key".to_string(), "--clearmodifiers".to_string(), key_spec];
                run_tool("xdotool", &args).await?;
                return Ok(ActionResult {
                    node_id: "keyboard".to_string(),
                    action: "press_key".to_string(),
                    method_used: "fallback:xdotool".to_string(),
                    success: true,
                    detail: Some(format!("args={args:?}")),
                });
            }
            Err(AxdomError::ActionUnsupported(format!(
                "key injection failed via XDG RemoteDesktop portal ({portal_err}) and neither `wtype` nor `xdotool` is in PATH"
            )))
        }
    }
}

/// Sends a coordinate-based mouse click via native XDG `RemoteDesktop` portal (Wayland, zero
/// external tools), falling back to `ydotool` or `xdotool`.
pub async fn send_coord_click(node_id: &str, x: i32, y: i32) -> Result<ActionResult> {
    if let Ok(res) = send_pointer_click_via_portal(node_id, x, y, BTN_LEFT, 1, "click_coords").await
    {
        return Ok(res);
    }

    let is_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();

    if is_wayland && command_exists("ydotool") {
        return run_ydotool_click(node_id, x, y).await;
    }
    if command_exists("xdotool") {
        return run_xdotool_click(node_id, x, y).await;
    }
    if command_exists("ydotool") {
        return run_ydotool_click(node_id, x, y).await;
    }

    Err(AxdomError::ActionUnsupported(format!(
        "coordinate fallback click at ({x}, {y}) failed via XDG portal and neither `ydotool` nor `xdotool` is in PATH"
    )))
}

/// Sends a right-click (context menu) at `(x, y)` via XDG portal, `ydotool`, or `xdotool`.
pub async fn send_right_click(node_id: &str, x: i32, y: i32) -> Result<ActionResult> {
    if let Ok(res) = send_pointer_click_via_portal(node_id, x, y, BTN_RIGHT, 1, "right_click").await
    {
        return Ok(res);
    }
    if command_exists("ydotool") {
        let move_args = vec![
            "mousemove".into(),
            "--absolute".into(),
            "-x".into(),
            x.to_string(),
            "-y".into(),
            y.to_string(),
        ];
        run_tool("ydotool", &move_args).await?;
        run_tool("ydotool", &["click".into(), "0xC1".into()]).await?;
        return Ok(ActionResult {
            node_id: node_id.to_string(),
            action: "right_click".into(),
            method_used: format!("fallback:ydotool-right({x},{y})"),
            success: true,
            detail: None,
        });
    }
    if command_exists("xdotool") {
        let args = vec![
            "mousemove".into(),
            x.to_string(),
            y.to_string(),
            "click".into(),
            "3".into(),
        ];
        run_tool("xdotool", &args).await?;
        return Ok(ActionResult {
            node_id: node_id.to_string(),
            action: "right_click".into(),
            method_used: format!("fallback:xdotool-right({x},{y})"),
            success: true,
            detail: None,
        });
    }
    Err(AxdomError::ActionUnsupported(format!(
        "right_click at ({x}, {y}) requires XDG RemoteDesktop portal, `ydotool`, or `xdotool`"
    )))
}

/// Sends a double-click at `(x, y)` via XDG portal, `ydotool`, or `xdotool`.
pub async fn send_double_click(node_id: &str, x: i32, y: i32) -> Result<ActionResult> {
    if let Ok(res) = send_pointer_click_via_portal(node_id, x, y, BTN_LEFT, 2, "double_click").await
    {
        return Ok(res);
    }
    if command_exists("ydotool") {
        let move_args = vec![
            "mousemove".into(),
            "--absolute".into(),
            "-x".into(),
            x.to_string(),
            "-y".into(),
            y.to_string(),
        ];
        run_tool("ydotool", &move_args).await?;
        run_tool(
            "ydotool",
            &["click".into(), "--repeat".into(), "2".into(), "0xC0".into()],
        )
        .await?;
        return Ok(ActionResult {
            node_id: node_id.to_string(),
            action: "double_click".into(),
            method_used: format!("fallback:ydotool-double({x},{y})"),
            success: true,
            detail: None,
        });
    }
    if command_exists("xdotool") {
        let args = vec![
            "mousemove".into(),
            x.to_string(),
            y.to_string(),
            "click".into(),
            "--repeat".into(),
            "2".into(),
            "1".into(),
        ];
        run_tool("xdotool", &args).await?;
        return Ok(ActionResult {
            node_id: node_id.to_string(),
            action: "double_click".into(),
            method_used: format!("fallback:xdotool-double({x},{y})"),
            success: true,
            detail: None,
        });
    }
    Err(AxdomError::ActionUnsupported(format!(
        "double_click at ({x}, {y}) requires XDG RemoteDesktop portal, `ydotool`, or `xdotool`"
    )))
}

/// Moves the pointer to `(x, y)` (hover) via XDG portal, `ydotool`, or `xdotool`.
pub async fn send_hover(node_id: &str, x: i32, y: i32) -> Result<ActionResult> {
    if let Ok(res) = send_pointer_hover_via_portal(node_id, x, y).await {
        return Ok(res);
    }
    if command_exists("ydotool") {
        let move_args = vec![
            "mousemove".into(),
            "--absolute".into(),
            "-x".into(),
            x.to_string(),
            "-y".into(),
            y.to_string(),
        ];
        run_tool("ydotool", &move_args).await?;
        return Ok(ActionResult {
            node_id: node_id.to_string(),
            action: "hover".into(),
            method_used: format!("fallback:ydotool-move({x},{y})"),
            success: true,
            detail: None,
        });
    }
    if command_exists("xdotool") {
        let args = vec!["mousemove".into(), x.to_string(), y.to_string()];
        run_tool("xdotool", &args).await?;
        return Ok(ActionResult {
            node_id: node_id.to_string(),
            action: "hover".into(),
            method_used: format!("fallback:xdotool-move({x},{y})"),
            success: true,
            detail: None,
        });
    }
    Err(AxdomError::ActionUnsupported(format!(
        "hover at ({x}, {y}) requires XDG RemoteDesktop portal, `ydotool`, or `xdotool`"
    )))
}

/// Drags with left button held from `(from_x, from_y)` to `(to_x, to_y)` via XDG portal, `ydotool`, or `xdotool`.
pub async fn send_drag(
    node_id: &str,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
) -> Result<ActionResult> {
    if let Ok(res) = send_pointer_drag_via_portal(node_id, from_x, from_y, to_x, to_y).await {
        return Ok(res);
    }
    if command_exists("ydotool") {
        run_tool(
            "ydotool",
            &[
                "mousemove".into(),
                "--absolute".into(),
                "-x".into(),
                from_x.to_string(),
                "-y".into(),
                from_y.to_string(),
            ],
        )
        .await?;
        run_tool("ydotool", &["click".into(), "0x40".into()]).await?;
        run_tool(
            "ydotool",
            &[
                "mousemove".into(),
                "--absolute".into(),
                "-x".into(),
                to_x.to_string(),
                "-y".into(),
                to_y.to_string(),
            ],
        )
        .await?;
        run_tool("ydotool", &["click".into(), "0x80".into()]).await?;
        return Ok(ActionResult {
            node_id: node_id.to_string(),
            action: "drag_to".into(),
            method_used: format!("fallback:ydotool-drag({from_x},{from_y}->{to_x},{to_y})"),
            success: true,
            detail: None,
        });
    }
    if command_exists("xdotool") {
        let args = vec![
            "mousemove".into(),
            from_x.to_string(),
            from_y.to_string(),
            "mousedown".into(),
            "1".into(),
            "mousemove".into(),
            to_x.to_string(),
            to_y.to_string(),
            "mouseup".into(),
            "1".into(),
        ];
        run_tool("xdotool", &args).await?;
        return Ok(ActionResult {
            node_id: node_id.to_string(),
            action: "drag_to".into(),
            method_used: format!("fallback:xdotool-drag({from_x},{from_y}->{to_x},{to_y})"),
            success: true,
            detail: None,
        });
    }
    Err(AxdomError::ActionUnsupported(
        "drag_to requires XDG RemoteDesktop portal or `xdotool`".into(),
    ))
}

async fn run_xdotool_click(node_id: &str, x: i32, y: i32) -> Result<ActionResult> {
    let args = vec![
        "mousemove".to_string(),
        x.to_string(),
        y.to_string(),
        "click".to_string(),
        "1".to_string(),
    ];
    run_tool("xdotool", &args).await?;
    Ok(ActionResult {
        node_id: node_id.to_string(),
        action: "click_coords".to_string(),
        method_used: format!("fallback:xdotool({x},{y})"),
        success: true,
        detail: None,
    })
}

async fn run_ydotool_click(node_id: &str, x: i32, y: i32) -> Result<ActionResult> {
    let move_args = vec![
        "mousemove".to_string(),
        "--absolute".to_string(),
        "-x".to_string(),
        x.to_string(),
        "-y".to_string(),
        y.to_string(),
    ];
    run_tool("ydotool", &move_args).await?;
    let click_args = vec!["click".to_string(), "0xC0".to_string()];
    run_tool("ydotool", &click_args).await?;
    Ok(ActionResult {
        node_id: node_id.to_string(),
        action: "click_coords".to_string(),
        method_used: format!("fallback:ydotool({x},{y})"),
        success: true,
        detail: None,
    })
}

/// Captures a PNG screenshot (optionally cropped to `bounds` when supported) and returns raw PNG bytes + tool name.
pub async fn capture_screen_bytes(bounds: Option<Bounds>) -> Result<(Vec<u8>, String)> {
    let tmp_path = PathBuf::from(format!(
        "/tmp/axdom-shot-{}-{}.png",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let out_str = tmp_path.to_string_lossy().to_string();
    let mut tried_method: Option<&str> = None;

    if command_exists("grim") {
        let mut args = Vec::new();
        if let Some(geom) = bounds.and_then(format_grim_geometry) {
            args.push("-g".to_string());
            args.push(geom);
        }
        args.push(out_str.clone());
        if run_tool("grim", &args).await.is_ok() {
            tried_method = Some("grim");
        }
    }

    if tried_method.is_none()
        && command_exists("gnome-screenshot")
        && run_tool("gnome-screenshot", &["-f".to_string(), out_str.clone()])
            .await
            .is_ok()
    {
        tried_method = Some("gnome-screenshot");
    }

    if tried_method.is_none()
        && command_exists("scrot")
        && run_tool("scrot", std::slice::from_ref(&out_str))
            .await
            .is_ok()
    {
        tried_method = Some("scrot");
    }

    if let Some(method) = tried_method {
        let mut bytes = tokio::fs::read(&tmp_path)
            .await
            .map_err(|e| AxdomError::ActionFailed(format!("read captured screenshot: {e}")))?;
        let _ = tokio::fs::remove_file(&tmp_path).await;
        if method != "grim"
            && let Some(b) = bounds
        {
            bytes = crop_png_bytes(&bytes, b)?;
        }
        return Ok((bytes, format!("screenshot:{method}")));
    }

    // Zero-external-tool native XDG Desktop Portal Screenshot fallback (KDE, GNOME, Hyprland).
    let (mut bytes, method) = capture_screenshot_via_portal().await?;
    if let Some(b) = bounds {
        bytes = crop_png_bytes(&bytes, b)?;
    }
    Ok((bytes, method))
}

/// Captures a PNG screenshot (optionally cropped to `bounds`) for blind-spot visual fallback.
pub async fn capture_screen_png(bounds: Option<Bounds>) -> Result<Screenshot> {
    let (bytes, method_used) = capture_screen_bytes(bounds).await?;
    let bytes_len = bytes.len();
    let base64_data = encode_base64(&bytes);

    Ok(Screenshot {
        mime_type: "image/png".to_string(),
        base64_data,
        method_used,
        bytes_len,
    })
}

/// Sends vertical and/or horizontal scroll steps via XDG RemoteDesktop portal or `xdotool` (X11).
pub async fn send_scroll(node_id: &str, delta_x: i32, delta_y: i32) -> Result<ActionResult> {
    let is_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    if !is_wayland && command_exists("xdotool") {
        if delta_y != 0 {
            let button = if delta_y >= 0 { "5" } else { "4" };
            let repeat = delta_y.unsigned_abs().max(1).to_string();
            let args = vec![
                "click".to_string(),
                "--repeat".to_string(),
                repeat,
                button.to_string(),
            ];
            run_tool("xdotool", &args).await?;
        }
        if delta_x != 0 {
            let button = if delta_x >= 0 { "7" } else { "6" };
            let repeat = delta_x.unsigned_abs().max(1).to_string();
            let args = vec![
                "click".to_string(),
                "--repeat".to_string(),
                repeat,
                button.to_string(),
            ];
            run_tool("xdotool", &args).await?;
        }
        return Ok(ActionResult {
            node_id: node_id.to_string(),
            action: "scroll".to_string(),
            method_used: format!("fallback:xdotool(scroll={delta_x},{delta_y})"),
            success: true,
            detail: None,
        });
    }
    send_scroll_via_portal(node_id, delta_x, delta_y).await
}

async fn run_tool(bin: &str, args: &[String]) -> Result<()> {
    let output = Command::new(bin)
        .args(args)
        .output()
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("spawn {bin}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AxdomError::ActionFailed(format!(
            "{bin} exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    Ok(())
}

fn command_exists(bin: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join(bin).is_file())
}

pub fn build_wtype_args(combo: &KeyCombo) -> Vec<String> {
    let mut args = Vec::new();
    for m in &combo.modifiers {
        args.push("-M".to_string());
        args.push(wtype_modifier(*m).to_string());
    }
    args.push("-k".to_string());
    args.push(normalize_keysym(&combo.key));
    for m in combo.modifiers.iter().rev() {
        args.push("-m".to_string());
        args.push(wtype_modifier(*m).to_string());
    }
    args
}

pub fn build_xdotool_spec(combo: &KeyCombo) -> String {
    let mut parts: Vec<String> = combo
        .modifiers
        .iter()
        .map(|m| match m {
            KeyModifier::Ctrl => "ctrl".to_string(),
            KeyModifier::Alt => "alt".to_string(),
            KeyModifier::Shift => "shift".to_string(),
            KeyModifier::Super => "super".to_string(),
        })
        .collect();
    parts.push(normalize_keysym(&combo.key));
    parts.join("+")
}

pub fn format_grim_geometry(b: Bounds) -> Option<String> {
    if b.is_zero_sized() {
        return None;
    }
    Some(format!(
        "{},{} {}x{}",
        b.x.max(0),
        b.y.max(0),
        b.width,
        b.height
    ))
}

fn wtype_modifier(m: KeyModifier) -> &'static str {
    match m {
        KeyModifier::Ctrl => "ctrl",
        KeyModifier::Alt => "alt",
        KeyModifier::Shift => "shift",
        KeyModifier::Super => "logo",
    }
}

fn normalize_keysym(key: &str) -> String {
    match key.to_lowercase().as_str() {
        "enter" | "return" => "Return".to_string(),
        "esc" | "escape" => "Escape".to_string(),
        "tab" => "Tab".to_string(),
        "space" => "space".to_string(),
        "backspace" => "BackSpace".to_string(),
        "delete" | "del" => "Delete".to_string(),
        "up" => "Up".to_string(),
        "down" => "Down".to_string(),
        "left" => "Left".to_string(),
        "right" => "Right".to_string(),
        _ => key.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_wtype_and_xdotool_arguments() {
        let combo = KeyCombo::parse("Ctrl+Shift+Enter").unwrap();
        assert_eq!(
            build_wtype_args(&combo),
            vec![
                "-M", "ctrl", "-M", "shift", "-k", "Return", "-m", "shift", "-m", "ctrl"
            ]
        );
        assert_eq!(build_xdotool_spec(&combo), "ctrl+shift+Return");
    }

    #[test]
    fn formats_grim_geometry_safely() {
        let valid = Bounds {
            x: 100,
            y: 200,
            width: 800,
            height: 600,
        };
        assert_eq!(
            format_grim_geometry(valid).as_deref(),
            Some("100,200 800x600")
        );
        assert_eq!(format_grim_geometry(Bounds::default()), None);
    }
}
