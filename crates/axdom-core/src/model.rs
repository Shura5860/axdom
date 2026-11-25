use serde::{Deserialize, Serialize};

/// Normalized semantic UI roles across OS accessibility APIs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    Window,
    Dialog,
    Menu,
    MenuItem,
    Button,
    ToggleButton,
    CheckBox,
    RadioButton,
    Input,
    PasswordInput,
    ComboBox,
    Link,
    Tab,
    TabList,
    List,
    ListItem,
    Tree,
    TreeItem,
    Table,
    Cell,
    Slider,
    Heading,
    Text,
    Image,
    Toolbar,
    ScrollBar,
    Container,
    Unknown(String),
}

impl NodeRole {
    /// Short, LLM-friendly role label used in compact serialization.
    pub fn compact_label(&self) -> &str {
        match self {
            Self::Window => "window",
            Self::Dialog => "dialog",
            Self::Menu => "menu",
            Self::MenuItem => "menuitem",
            Self::Button => "button",
            Self::ToggleButton => "toggle",
            Self::CheckBox => "checkbox",
            Self::RadioButton => "radio",
            Self::Input => "input",
            Self::PasswordInput => "password",
            Self::ComboBox => "combobox",
            Self::Link => "link",
            Self::Tab => "tab",
            Self::TabList => "tablist",
            Self::List => "list",
            Self::ListItem => "listitem",
            Self::Tree => "tree",
            Self::TreeItem => "treeitem",
            Self::Table => "grid",
            Self::Cell => "cell",
            Self::Slider => "slider",
            Self::Heading => "heading",
            Self::Text => "text",
            Self::Image => "image",
            Self::Toolbar => "toolbar",
            Self::ScrollBar => "scrollbar",
            Self::Container => "container",
            Self::Unknown(raw) => raw.as_str(),
        }
    }

    /// Returns true if the role represents a directly interactive UI control.
    pub fn is_interactive(&self) -> bool {
        matches!(
            self,
            Self::Menu
                | Self::MenuItem
                | Self::Button
                | Self::ToggleButton
                | Self::CheckBox
                | Self::RadioButton
                | Self::Input
                | Self::PasswordInput
                | Self::ComboBox
                | Self::Link
                | Self::Tab
                | Self::ListItem
                | Self::TreeItem
                | Self::Cell
                | Self::Slider
        )
    }

    /// Returns true if the role carries useful structural or textual information
    /// even when not directly interactive.
    pub fn is_semantic(&self) -> bool {
        self.is_interactive()
            || matches!(
                self,
                Self::Window
                    | Self::Dialog
                    | Self::TabList
                    | Self::List
                    | Self::Tree
                    | Self::Table
                    | Self::Heading
                    | Self::Text
                    | Self::Toolbar
            )
    }
}

/// Screen or window-relative bounding box.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Bounds {
    pub fn is_zero_sized(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }
}

/// Normalized accessibility states relevant to an AI agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeState {
    pub enabled: bool,
    pub visible: bool,
    pub showing: bool,
    pub focused: bool,
    pub focusable: bool,
    pub selected: bool,
    pub checked: bool,
    pub expanded: bool,
    pub editable: bool,
    pub sensitive: bool,
}

impl NodeState {
    /// Returns compact state tags for LLM serialization (e.g., `["focused", "checked"]`).
    pub fn active_tags(&self) -> Vec<&'static str> {
        let mut tags = Vec::new();
        if self.focused {
            tags.push("focused");
        }
        if self.selected {
            tags.push("selected");
        }
        if self.checked {
            tags.push("checked");
        }
        if self.expanded {
            tags.push("expanded");
        }
        if self.editable {
            tags.push("editable");
        }
        if !self.enabled || !self.sensitive {
            tags.push("disabled");
        }
        tags
    }
}

/// Raw node collected directly from an OS accessibility backend before pruning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawNode {
    /// Backend-specific locator (e.g. D-Bus bus_name + object_path).
    pub backend_ref: String,
    pub role: NodeRole,
    pub name: String,
    pub description: Option<String>,
    pub value: Option<String>,
    pub state: NodeState,
    pub bounds: Option<Bounds>,
    pub children: Vec<RawNode>,
}

/// Pruned, ID-assigned node ready for LLM consumption or action dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrunedNode {
    /// Short stable hash ID (e.g. `#b4f`). Assigned to interactive / actionable nodes.
    pub id: Option<String>,
    pub backend_ref: String,
    pub role: NodeRole,
    pub name: String,
    pub value: Option<String>,
    pub states: Vec<String>,
    pub bounds: Option<Bounds>,
    pub children: Vec<PrunedNode>,
}

/// Summary of an open desktop window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: String,
    pub title: String,
    pub app_name: String,
    pub pid: Option<u32>,
    pub active: bool,
}

/// Complete pruned snapshot of a window's accessibility tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeSnapshot {
    pub snapshot_id: u64,
    pub window: WindowInfo,
    pub elements: Vec<PrunedNode>,
    pub interactive_count: usize,
    pub total_raw_nodes: usize,
    pub truncated: bool,
    /// True when the window exposes too few interactive nodes (e.g. canvas, game, or a11y disabled).
    pub blind_spot: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn is_healthy(&self) -> bool {
        self.checks.iter().all(|c| c.status != CheckStatus::Fail)
    }
}
