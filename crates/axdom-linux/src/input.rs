use std::path::PathBuf;

use axdom_core::{
    ActionResult, AxdomError, Bounds, KeyCombo, KeyModifier, Result, Screenshot, encode_base64,
};
use tokio::process::Command;

/// Sends a keyboard shortcut via the first available Linux input tool (`wtype`, `xdotool`, `ydotool`).
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

    Err(AxdomError::ActionUnsupported(
        "no supported Linux key-injection tool found in PATH (install `wtype` on Wayland or `xdotool` on X11)"
            .to_string(),
    ))
}

/// Sends a coordinate-based mouse click fallback via `ydotool` (Wayland priority) or `xdotool`.
pub async fn send_coord_click(node_id: &str, x: i32, y: i32) -> Result<ActionResult> {
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
        "coordinate fallback click at ({x}, {y}) requires `ydotool` (Wayland/uinput) or `xdotool` (X11) in PATH"
    )))
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

/// Captures a PNG screenshot (optionally cropped to `bounds`) and returns raw PNG bytes + tool name.
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

    let method = if command_exists("grim") {
        let mut args = Vec::new();
        if let Some(geom) = bounds.and_then(format_grim_geometry) {
            args.push("-g".to_string());
            args.push(geom);
        }
        args.push(out_str.clone());
        run_tool("grim", &args).await?;
        "grim"
    } else if command_exists("gnome-screenshot") {
        run_tool("gnome-screenshot", &["-f".to_string(), out_str.clone()]).await?;
        "gnome-screenshot"
    } else if command_exists("scrot") {
        run_tool("scrot", std::slice::from_ref(&out_str)).await?;
        "scrot"
    } else {
        return Err(AxdomError::ActionUnsupported(
            "no screenshot tool found in PATH (install `grim` on Wayland, or `gnome-screenshot`/`scrot`)"
                .to_string(),
        ));
    };

    let bytes = tokio::fs::read(&tmp_path)
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("read captured screenshot: {e}")))?;
    let _ = tokio::fs::remove_file(&tmp_path).await;

    Ok((bytes, format!("screenshot:{method}")))
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
