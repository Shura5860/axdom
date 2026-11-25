use std::fmt::Write;

use crate::model::{PrunedNode, TreeSnapshot, WindowInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Compact,
    Json,
}

pub fn format_snapshot(snapshot: &TreeSnapshot, format: OutputFormat) -> String {
    match format {
        OutputFormat::Json => {
            serde_json::to_string_pretty(snapshot).unwrap_or_else(|_| "{}".to_string())
        }
        OutputFormat::Compact => format_compact_snapshot(snapshot),
    }
}

pub fn format_windows(windows: &[WindowInfo], format: OutputFormat) -> String {
    match format {
        OutputFormat::Json => {
            serde_json::to_string_pretty(windows).unwrap_or_else(|_| "[]".to_string())
        }
        OutputFormat::Compact => {
            let mut out = String::new();
            for win in windows {
                let active = if win.active { " [active]" } else { "" };
                let pid = win.pid.map(|p| format!(" (pid: {p})")).unwrap_or_default();
                let _ = writeln!(
                    out,
                    "- [{}] {}: {:?}{}{}",
                    win.id, win.app_name, win.title, pid, active
                );
            }
            out
        }
    }
}

fn format_compact_snapshot(snapshot: &TreeSnapshot) -> String {
    let mut out = String::new();
    let pid = snapshot
        .window
        .pid
        .map(|p| format!(" (pid: {p})"))
        .unwrap_or_default();

    let _ = writeln!(
        out,
        "window: {:?}{} [app: {}, snapshot: #{}]",
        snapshot.window.title, pid, snapshot.window.app_name, snapshot.snapshot_id
    );

    if !snapshot.warnings.is_empty() {
        let _ = writeln!(out, "warnings:");
        for w in &snapshot.warnings {
            let _ = writeln!(out, "  - {w}");
        }
    }

    let _ = writeln!(out, "elements:");
    for node in &snapshot.elements {
        write_node(&mut out, node, 1);
    }

    out
}

fn write_node(out: &mut String, node: &PrunedNode, indent: usize) {
    let pad = "  ".repeat(indent);
    let _ = write!(out, "{pad}- ");

    if let Some(id) = &node.id {
        let _ = write!(out, "[{id}] ");
    }

    let _ = write!(out, "{}", node.role.compact_label());

    if !node.name.is_empty() {
        let _ = write!(out, " {:?}", node.name);
    }

    if let Some(val) = &node.value {
        let _ = write!(out, " value={val:?}");
    }

    if !node.states.is_empty() {
        let _ = write!(out, " [{}]", node.states.join(", "));
    }

    let _ = writeln!(out);

    for child in &node.children {
        write_node(out, child, indent + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NodeRole, WindowInfo};

    #[test]
    fn formats_compact_yaml_cleanly() {
        let snapshot = TreeSnapshot {
            snapshot_id: 1,
            window: WindowInfo {
                id: "w1".into(),
                title: "LibreOffice Calc - budget.xlsx".into(),
                app_name: "soffice.bin".into(),
                pid: Some(4120),
                active: true,
            },
            elements: vec![PrunedNode {
                id: Some("#b4f".into()),
                backend_ref: "ref".into(),
                role: NodeRole::Input,
                name: "Formula Bar".into(),
                value: Some("=SUM(A1:A10)".into()),
                states: vec!["focused".into()],
                bounds: None,
                children: vec![],
            }],
            interactive_count: 1,
            total_raw_nodes: 5,
            truncated: false,
            blind_spot: false,
            warnings: vec![],
        };

        let text = format_snapshot(&snapshot, OutputFormat::Compact);
        assert!(text.contains("window: \"LibreOffice Calc - budget.xlsx\" (pid: 4120)"));
        assert!(text.contains("- [#b4f] input \"Formula Bar\" value=\"=SUM(A1:A10)\" [focused]"));
    }
}
