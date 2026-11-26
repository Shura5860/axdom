use axdom_core::{KeyModifier, NodeRole};

/// Maps macOS Accessibility `AXRole` (and optional `AXSubrole`) strings to `NodeRole`.
pub fn map_ax_role(role: &str, subrole: Option<&str>) -> NodeRole {
    if matches!(subrole, Some("AXSecureTextField")) {
        return NodeRole::PasswordInput;
    }
    if matches!(subrole, Some("AXDialog" | "AXSystemDialog" | "AXSheet")) {
        return NodeRole::Dialog;
    }
    match role {
        "AXWindow" => NodeRole::Window,
        "AXSheet" | "AXDialog" => NodeRole::Dialog,
        "AXButton" | "AXMenuButton" => NodeRole::Button,
        "AXCheckBox" => NodeRole::CheckBox,
        "AXRadioButton" => NodeRole::RadioButton,
        "AXTextField" | "AXTextArea" | "AXSearchField" => NodeRole::Input,
        "AXPopUpButton" | "AXComboBox" => NodeRole::ComboBox,
        "AXLink" => NodeRole::Link,
        "AXMenuBar" | "AXMenu" => NodeRole::Menu,
        "AXMenuBarItem" | "AXMenuItem" => NodeRole::MenuItem,
        "AXTabGroup" => NodeRole::TabList,
        "AXTab" => NodeRole::Tab,
        "AXList" => NodeRole::List,
        "AXTable" => NodeRole::Table,
        "AXRow" | "AXCell" => NodeRole::ListItem,
        "AXOutline" => NodeRole::Tree,
        "AXSlider" => NodeRole::Slider,
        "AXStaticText" => NodeRole::Text,
        "AXToolbar" => NodeRole::Toolbar,
        "AXImage" => NodeRole::Image,
        "AXWebArea" => NodeRole::Container,
        "AXGroup" | "AXScrollArea" | "AXSplitGroup" | "AXLayoutArea" => NodeRole::Container,
        other => NodeRole::Unknown(other.to_string()),
    }
}

/// Maps `KeyModifier` slice to macOS `CGEventFlags` bitmask.
pub fn cg_flags_for_modifiers(modifiers: &[KeyModifier]) -> u64 {
    let mut flags = 0u64;
    for m in modifiers {
        flags |= match m {
            KeyModifier::Shift => 0x0002_0000, // kCGEventFlagMaskShift
            KeyModifier::Ctrl => 0x0004_0000,  // kCGEventFlagMaskControl
            KeyModifier::Alt => 0x0008_0000,   // kCGEventFlagMaskAlternate
            KeyModifier::Super => 0x0010_0000, // kCGEventFlagMaskCommand
        };
    }
    flags
}

/// Maps key names (`Enter`, `Escape`, `Tab`, `F5`, `a`, `S`, `" "`) to macOS virtual keycodes.
pub fn cg_keycode_for_key(key: &str) -> Option<u16> {
    if key == " " {
        return Some(49); // kVK_Space
    }
    let lower = key.trim().to_lowercase();
    let code = match lower.as_str() {
        "enter" | "return" => 36,
        "tab" => 48,
        "space" => 49,
        "backspace" => 51,
        "esc" | "escape" => 53,
        "delete" | "del" => 117,
        "home" => 115,
        "end" => 119,
        "pageup" | "page_up" => 116,
        "pagedown" | "page_down" => 121,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "9" => 25,
        "7" => 26,
        "8" => 28,
        "0" => 29,
        "o" => 31,
        "u" => 32,
        "i" => 34,
        "p" => 35,
        "l" => 37,
        "j" => 38,
        "k" => 40,
        "n" => 45,
        "m" => 46,
        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_macos_ax_roles_and_secure_field() {
        assert_eq!(map_ax_role("AXButton", None), NodeRole::Button);
        assert_eq!(map_ax_role("AXTextField", None), NodeRole::Input);
        assert_eq!(
            map_ax_role("AXTextField", Some("AXSecureTextField")),
            NodeRole::PasswordInput
        );
        assert_eq!(map_ax_role("AXWindow", Some("AXDialog")), NodeRole::Dialog);
    }

    #[test]
    fn maps_macos_keycodes_and_modifiers() {
        assert_eq!(cg_keycode_for_key("Enter"), Some(36));
        assert_eq!(cg_keycode_for_key(" "), Some(49));
        assert_eq!(cg_keycode_for_key("s"), Some(1));
        assert_eq!(
            cg_flags_for_modifiers(&[KeyModifier::Super, KeyModifier::Shift]),
            0x0012_0000
        );
    }
}
