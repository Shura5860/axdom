use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::action::NodeRef;
use crate::error::{AxdomError, Result};
use crate::model::{PrunedNode, TreeSnapshot};

/// Persists and queries the most recent `TreeSnapshot` so CLI invocations (`axdom click #b4f`)
/// can resolve short IDs across separate process runs.
pub struct SnapshotStore {
    path: PathBuf,
}

impl SnapshotStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Default path: `$XDG_RUNTIME_DIR/axdom/snapshot.json` or `/tmp/axdom-snapshot.json`.
    pub fn default_location() -> Self {
        let path = std::env::var_os("XDG_RUNTIME_DIR")
            .map(|dir| PathBuf::from(dir).join("axdom").join("snapshot.json"))
            .unwrap_or_else(|| PathBuf::from("/tmp/axdom-snapshot.json"));
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save(&self, snapshot: &TreeSnapshot) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AxdomError::SnapshotStore(format!("create dir: {e}")))?;
        }
        let json = serde_json::to_vec_pretty(snapshot)
            .map_err(|e| AxdomError::SnapshotStore(format!("serialize: {e}")))?;
        std::fs::write(&self.path, json)
            .map_err(|e| AxdomError::SnapshotStore(format!("write {}: {e}", self.path.display())))
    }

    pub fn load(&self) -> Result<TreeSnapshot> {
        let bytes = std::fs::read(&self.path).map_err(|e| {
            AxdomError::SnapshotStore(format!(
                "no cached snapshot at {} (run `axdom tree` first): {e}",
                self.path.display()
            ))
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|e| AxdomError::SnapshotStore(format!("parse snapshot: {e}")))
    }
}

/// Normalizes a user-supplied node ID (`b4f` or `#b4f`) to `#b4f`.
pub fn normalize_node_id(raw: &str) -> String {
    let trimmed = raw.trim().to_lowercase();
    if trimmed.starts_with('#') {
        trimmed
    } else {
        format!("#{trimmed}")
    }
}

/// Finds an interactive node strictly by short ID (`#b4f` or `b4f`).
/// Never executes by label (preventing LLM blind-guess hallucinations), but includes a
/// helpful hint with matching `#id`s if a label was passed by mistake.
pub fn find_node_ref(snapshot: &TreeSnapshot, raw_id: &str) -> Result<NodeRef> {
    let target_id = normalize_node_id(raw_id);
    if let Some(found) = find_in_slice(&snapshot.elements, &target_id) {
        return Ok(found);
    }

    let trimmed = raw_id.trim();
    if !trimmed.is_empty() && !trimmed.starts_with('#') {
        let mut suggestions = Vec::new();
        find_by_name(&snapshot.elements, trimmed, &mut suggestions);
        if !suggestions.is_empty() {
            let hints: Vec<String> = suggestions
                .iter()
                .map(|m| format!("{} ({} {:?})", m.id, m.role.compact_label(), m.name))
                .collect();
            return Err(AxdomError::NodeNotFound(format!(
                "{target_id} (pass the short #id explicitly; did you mean: {}?)",
                hints.join(", ")
            )));
        }
    }

    Err(AxdomError::NodeNotFound(target_id))
}

fn find_by_name(nodes: &[PrunedNode], needle: &str, out: &mut Vec<NodeRef>) {
    for node in nodes {
        if let Some(id) = &node.id
            && node.name.eq_ignore_ascii_case(needle)
        {
            out.push(NodeRef {
                id: id.clone(),
                backend_ref: node.backend_ref.clone(),
                role: node.role.clone(),
                name: node.name.clone(),
                bounds: node.bounds,
            });
        }
        find_by_name(&node.children, needle, out);
    }
}

/// Extracts a subtree rooted at `root_id` from a `TreeSnapshot` to reduce LLM token usage
/// on very large windows.
pub fn scope_snapshot(snapshot: &TreeSnapshot, root_id: &str) -> Result<TreeSnapshot> {
    let target = normalize_node_id(root_id);
    let node = find_pruned_node(&snapshot.elements, &target)
        .ok_or_else(|| AxdomError::NodeNotFound(target.clone()))?
        .clone();
    let interactive_count = count_interactive(&node);

    Ok(TreeSnapshot {
        snapshot_id: snapshot.snapshot_id,
        window: snapshot.window.clone(),
        elements: vec![node],
        interactive_count,
        total_raw_nodes: snapshot.total_raw_nodes,
        truncated: snapshot.truncated,
        blind_spot: false,
        warnings: snapshot.warnings.clone(),
    })
}

fn find_pruned_node<'a>(nodes: &'a [PrunedNode], target_id: &str) -> Option<&'a PrunedNode> {
    for node in nodes {
        if node.id.as_deref() == Some(target_id) {
            return Some(node);
        }
        if let Some(found) = find_pruned_node(&node.children, target_id) {
            return Some(found);
        }
    }
    None
}

fn count_interactive(node: &PrunedNode) -> usize {
    let self_count = usize::from(node.id.is_some());
    self_count + node.children.iter().map(count_interactive).sum::<usize>()
}

fn find_in_slice(nodes: &[PrunedNode], target_id: &str) -> Option<NodeRef> {
    for node in nodes {
        if node.id.as_deref() == Some(target_id) {
            return Some(NodeRef {
                id: target_id.to_string(),
                backend_ref: node.backend_ref.clone(),
                role: node.role.clone(),
                name: node.name.clone(),
                bounds: node.bounds,
            });
        }
        if let Some(found) = find_in_slice(&node.children, target_id) {
            return Some(found);
        }
    }
    None
}

/// Compact diff between two snapshots of a window, designed for low-token agent updates.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

impl SnapshotDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

pub fn diff_snapshots(prev: &TreeSnapshot, next: &TreeSnapshot) -> SnapshotDiff {
    let mut prev_map = HashMap::new();
    let mut next_map = HashMap::new();
    collect_index(&prev.elements, &mut prev_map);
    collect_index(&next.elements, &mut next_map);

    let mut diff = SnapshotDiff::default();

    for (id, next_node) in &next_map {
        match prev_map.get(id) {
            None => diff.added.push(summarize_node(next_node)),
            Some(prev_node) => {
                if prev_node.name != next_node.name
                    || prev_node.value != next_node.value
                    || prev_node.states != next_node.states
                {
                    diff.changed.push(summarize_node(next_node));
                }
            }
        }
    }

    for (id, prev_node) in &prev_map {
        if !next_map.contains_key(id) {
            diff.removed.push(summarize_node(prev_node));
        }
    }

    diff.added.sort();
    diff.removed.sort();
    diff.changed.sort();
    diff
}

fn collect_index<'a>(nodes: &'a [PrunedNode], map: &mut HashMap<&'a str, &'a PrunedNode>) {
    for node in nodes {
        if let Some(id) = node.id.as_deref() {
            map.insert(id, node);
        }
        collect_index(&node.children, map);
    }
}

fn summarize_node(node: &PrunedNode) -> String {
    let id = node.id.as_deref().unwrap_or("");
    let val = node
        .value
        .as_deref()
        .map(|v| format!(" value={v:?}"))
        .unwrap_or_default();
    let states = if node.states.is_empty() {
        String::new()
    } else {
        format!(" [{}]", node.states.join(", "))
    };
    format!(
        "[{id}] {} {:?}{val}{states}",
        node.role.compact_label(),
        node.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NodeRole, WindowInfo};

    fn sample_snapshot(nodes: Vec<PrunedNode>) -> TreeSnapshot {
        TreeSnapshot {
            snapshot_id: 1,
            window: WindowInfo {
                id: "w1".into(),
                title: "Test".into(),
                app_name: "app".into(),
                pid: Some(1),
                active: true,
            },
            interactive_count: nodes.len(),
            elements: nodes,
            total_raw_nodes: 2,
            truncated: false,
            blind_spot: false,
            warnings: vec![],
        }
    }

    fn node(id: &str, role: NodeRole, name: &str, value: Option<&str>) -> PrunedNode {
        PrunedNode {
            id: Some(id.into()),
            backend_ref: format!(":1.1:/node/{id}"),
            role,
            name: name.into(),
            value: value.map(str::to_string),
            states: vec![],
            bounds: None,
            children: vec![],
        }
    }

    #[test]
    fn resolves_node_with_or_without_hash() {
        let snap = sample_snapshot(vec![
            node("#b4f", NodeRole::Button, "Save", None),
            node("#c11", NodeRole::Button, "Delete", None),
            node("#c22", NodeRole::Button, "Delete", None),
        ]);
        assert_eq!(find_node_ref(&snap, "b4f").unwrap().name, "Save");
        assert_eq!(find_node_ref(&snap, "#B4F").unwrap().id, "#b4f");
        // Label must be rejected (no blind-guess clicks), with helpful `#id` hint
        let save_err = find_node_ref(&snap, "save").unwrap_err().to_string();
        assert!(save_err.contains("#b4f") && save_err.contains("did you mean"));
        let del_err = find_node_ref(&snap, "Delete").unwrap_err().to_string();
        assert!(del_err.contains("#c11") && del_err.contains("#c22"));
        assert!(find_node_ref(&snap, "#000").is_err());
    }

    #[test]
    fn computes_added_removed_and_changed_diff() {
        let prev = sample_snapshot(vec![
            node("#a11", NodeRole::Button, "Open", None),
            node("#b22", NodeRole::Input, "Search", Some("old")),
        ]);
        let next = sample_snapshot(vec![
            node("#b22", NodeRole::Input, "Search", Some("new")),
            node("#c33", NodeRole::Button, "Close", None),
        ]);

        let diff = diff_snapshots(&prev, &next);
        assert_eq!(diff.removed.len(), 1);
        assert!(diff.removed[0].contains("#a11"));
        assert_eq!(diff.added.len(), 1);
        assert!(diff.added[0].contains("#c33"));
        assert_eq!(diff.changed.len(), 1);
        assert!(diff.changed[0].contains("value=\"new\""));
    }

    #[test]
    fn scopes_snapshot_to_target_subtree() {
        let mut panel = node("#p01", NodeRole::List, "Toolbar", None);
        panel
            .children
            .push(node("#b02", NodeRole::Button, "Run", None));
        let snap = sample_snapshot(vec![node("#a00", NodeRole::Button, "Outside", None), panel]);

        let scoped = scope_snapshot(&snap, "p01").unwrap();
        assert_eq!(scoped.elements.len(), 1);
        assert_eq!(scoped.elements[0].name, "Toolbar");
        assert_eq!(scoped.interactive_count, 2);
        assert!(scope_snapshot(&snap, "#zzz").is_err());
    }
}
