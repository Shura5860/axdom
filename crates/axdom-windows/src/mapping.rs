use axdom_core::{KeyModifier, NodeRole};

/// Maps Windows UI Automation `CONTROLTYPEID` constants to platform-neutral `NodeRole`.
pub fn map_uia_control_type(control_type: i32, is_password: bool) -> NodeRole {
    if is_password {
        return NodeRole::PasswordInput;
    }
    match control_type {
        50000 => NodeRole::Button,      // UIA_ButtonControlTypeId
        50001 => NodeRole::Container,   // UIA_CalendarControlTypeId
        50002 => NodeRole::CheckBox,    // UIA_CheckBoxControlTypeId
        50003 => NodeRole::ComboBox,    // UIA_ComboBoxControlTypeId
        50004 => NodeRole::Input,       // UIA_EditControlTypeId
        50005 => NodeRole::Link,        // UIA_HyperlinkControlTypeId
        50006 => NodeRole::Image,       // UIA_ImageControlTypeId
        50007 => NodeRole::ListItem,    // UIA_ListItemControlTypeId
        50008 => NodeRole::List,        // UIA_ListControlTypeId
        50009 => NodeRole::Menu,        // UIA_MenuControlTypeId
        50010 => NodeRole::Menu,        // UIA_MenuBarControlTypeId
        50011 => NodeRole::MenuItem,    // UIA_MenuItemControlTypeId
        50013 => NodeRole::RadioButton, // UIA_RadioButtonControlTypeId
        50015 => NodeRole::Slider,      // UIA_SliderControlTypeId
        50016 => NodeRole::Input,       // UIA_SpinnerControlTypeId
        50018 => NodeRole::TabList,     // UIA_TabControlTypeId
        50019 => NodeRole::Tab,         // UIA_TabItemControlTypeId
        50020 => NodeRole::Text,        // UIA_TextControlTypeId
        50021 => NodeRole::Toolbar,     // UIA_ToolBarControlTypeId
        50023 => NodeRole::Tree,        // UIA_TreeControlTypeId
        50024 => NodeRole::TreeItem,    // UIA_TreeItemControlTypeId
        50026 => NodeRole::Container,   // UIA_GroupControlTypeId
        50028 => NodeRole::Table,       // UIA_DataGridControlTypeId
        50029 => NodeRole::ListItem,    // UIA_DataItemControlTypeId
        50030 => NodeRole::Container,   // UIA_DocumentControlTypeId
        50032 => NodeRole::Window,      // UIA_WindowControlTypeId
        50033 => NodeRole::Container,   // UIA_PaneControlTypeId
        50036 => NodeRole::Table,       // UIA_TableControlTypeId
        other => NodeRole::Unknown(format!("uia:{other}")),
    }
}

/// Formats a UIA runtime ID slice as a stable `backend_ref` string (e.g. `uia:42.1048576.4`).
pub fn format_runtime_id(ids: &[i32]) -> String {
    let joined: Vec<String> = ids.iter().map(ToString::to_string).collect();
    format!("uia:{}", joined.join("."))
}

/// Parses a `uia:...` backend reference back into a UIA runtime ID vector.
pub fn parse_runtime_id(backend_ref: &str) -> Option<Vec<i32>> {
    let rest = backend_ref.strip_prefix("uia:")?;
    if rest.is_empty() {
        return None;
    }
    rest.split('.')
        .map(|part| part.parse::<i32>().ok())
        .collect()
}

/// Maps `KeyModifier` to Win32 Virtual-Key code (`VK_*`).
pub fn vk_for_modifier(m: KeyModifier) -> u16 {
    match m {
        KeyModifier::Ctrl => 0x11,  // VK_CONTROL
        KeyModifier::Alt => 0x12,   // VK_MENU
        KeyModifier::Shift => 0x10, // VK_SHIFT
        KeyModifier::Super => 0x5B, // VK_LWIN
    }
}

/// Maps a key name (`Enter`, `Escape`, `Tab`, `F5`, `a`, `S`, `" "`) to a Win32 Virtual-Key code.
pub fn vk_for_key(key: &str) -> Option<u16> {
    if key == " " {
        return Some(0x20); // VK_SPACE
    }
    let lower = key.trim().to_lowercase();
    let vk = match lower.as_str() {
        "enter" | "return" => 0x0D,
        "esc" | "escape" => 0x1B,
        "tab" => 0x09,
        "space" => 0x20,
        "backspace" => 0x08,
        "delete" | "del" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "page_up" => 0x21,
        "pagedown" | "page_down" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "f1" => 0x70,
        "f2" => 0x71,
        "f3" => 0x72,
        "f4" => 0x73,
        "f5" => 0x74,
        "f6" => 0x75,
        "f7" => 0x76,
        "f8" => 0x77,
        "f9" => 0x78,
        "f10" => 0x79,
        "f11" => 0x7A,
        "f12" => 0x7B,
        _ => {
            let mut chars = key.trim().chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            match ch.to_ascii_uppercase() {
                '0'..='9' | 'A'..='Z' => ch.to_ascii_uppercase() as u16,
                _ => return None,
            }
        }
    };
    Some(vk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_uia_control_types() {
        assert_eq!(map_uia_control_type(50000, false), NodeRole::Button);
        assert_eq!(map_uia_control_type(50004, false), NodeRole::Input);
        assert_eq!(map_uia_control_type(50004, true), NodeRole::PasswordInput);
        assert_eq!(map_uia_control_type(50032, false), NodeRole::Window);
    }

    #[test]
    fn roundtrips_runtime_id() {
        let encoded = format_runtime_id(&[42, 1048576, 4]);
        assert_eq!(encoded, "uia:42.1048576.4");
        assert_eq!(parse_runtime_id(&encoded), Some(vec![42, 1048576, 4]));
        assert_eq!(parse_runtime_id("invalid"), None);
    }

    #[test]
    fn maps_win32_virtual_keys() {
        assert_eq!(vk_for_key("Enter"), Some(0x0D));
        assert_eq!(vk_for_key(" "), Some(0x20));
        assert_eq!(vk_for_key("s"), Some(b'S' as u16));
        assert_eq!(vk_for_modifier(KeyModifier::Ctrl), 0x11);
    }
}
