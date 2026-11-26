use serde::{Deserialize, Serialize};

use crate::error::{AxdomError, Result};
use crate::model::{Bounds, NodeRole};

/// Reference to a previously snapshotted node, used for action dispatch and stale-node checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRef {
    pub id: String,
    pub backend_ref: String,
    pub role: NodeRole,
    pub name: String,
    pub bounds: Option<Bounds>,
}

/// Supported keyboard modifiers for `KeyCombo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyModifier {
    Ctrl,
    Alt,
    Shift,
    Super,
}

/// Parsed keyboard shortcut (e.g. `Ctrl+Shift+S`, `Enter`, `Alt+F4`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyCombo {
    pub modifiers: Vec<KeyModifier>,
    pub key: String,
}

impl KeyCombo {
    pub fn parse(raw: &str) -> Result<Self> {
        let parts: Vec<&str> = raw
            .split('+')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            return Err(AxdomError::ActionFailed(
                "empty key combination".to_string(),
            ));
        }

        let mut modifiers = Vec::new();
        for &part in &parts[..parts.len() - 1] {
            let m = match part.to_lowercase().as_str() {
                "ctrl" | "control" => KeyModifier::Ctrl,
                "alt" | "meta" => KeyModifier::Alt,
                "shift" => KeyModifier::Shift,
                "super" | "win" | "cmd" | "logo" => KeyModifier::Super,
                other => {
                    return Err(AxdomError::ActionFailed(format!(
                        "unknown key modifier '{other}' in '{raw}'"
                    )));
                }
            };
            if !modifiers.contains(&m) {
                modifiers.push(m);
            }
        }

        let key = parts
            .last()
            .ok_or_else(|| AxdomError::ActionFailed("empty key combination".to_string()))?
            .to_string();
        Ok(Self { modifiers, key })
    }
}

/// Supported programmatic UI actions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionRequest {
    Click,
    RightClick,
    DoubleClick,
    Hover,
    ClickCoords {
        x: i32,
        y: i32,
    },
    /// Drag from the target node's center coordinates to `(to_x, to_y)`.
    DragTo {
        to_x: i32,
        to_y: i32,
    },
    Focus,
    SetText {
        text: String,
    },
    SetValue {
        value: f64,
    },
    /// Scroll horizontally/vertically (`delta_x > 0` right, `delta_y > 0` down).
    Scroll {
        #[serde(default)]
        delta_x: i32,
        delta_y: i32,
    },
}

impl ActionRequest {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::RightClick => "right_click",
            Self::DoubleClick => "double_click",
            Self::Hover => "hover",
            Self::ClickCoords { .. } => "click_coords",
            Self::DragTo { .. } => "drag_to",
            Self::Focus => "focus",
            Self::SetText { .. } => "set_text",
            Self::SetValue { .. } => "set_value",
            Self::Scroll { .. } => "scroll",
        }
    }
}

/// Outcome of executing an action on a node or key combo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    pub node_id: String,
    pub action: String,
    pub method_used: String,
    pub success: bool,
    pub detail: Option<String>,
}

/// Captured screenshot returned for blind-spot (canvas/game) visual fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Screenshot {
    pub mime_type: String,
    pub base64_data: String,
    pub method_used: String,
    pub bytes_len: usize,
}

/// Live accessibility event emitted by the OS backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A11yEvent {
    pub category: String,
    pub kind: String,
    pub detail: String,
    pub source_ref: String,
}

/// Zero-dependency RFC 4648 standard Base64 encoder (used for MCP image blocks).
pub fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);

        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_key_combos() {
        let combo = KeyCombo::parse("Ctrl+Shift+S").unwrap();
        assert_eq!(combo.modifiers, vec![KeyModifier::Ctrl, KeyModifier::Shift]);
        assert_eq!(combo.key, "S");

        let single = KeyCombo::parse("Enter").unwrap();
        assert!(single.modifiers.is_empty());
        assert_eq!(single.key, "Enter");

        assert!(KeyCombo::parse("Bogus+A").is_err());
        assert!(KeyCombo::parse("").is_err());
    }

    #[test]
    fn encodes_base64_correctly() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"axdom"), "YXhkb20=");
    }
}
