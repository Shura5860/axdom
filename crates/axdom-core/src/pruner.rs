use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::model::{NodeRole, PrunedNode, RawNode, TreeSnapshot};
use crate::provider::RawWindowTree;

static NEXT_SNAPSHOT_ID: AtomicU64 = AtomicU64::new(1);

pub const REDACTED_VALUE: &str = "[REDACTED]";

#[derive(Debug, Clone)]
pub struct PrunerConfig {
    /// Drop nodes whose bounding box is 0x0 (when bounds are reported).
    pub drop_zero_sized: bool,
    /// Drop nodes that are neither visible nor showing.
    pub drop_hidden: bool,
    /// Minimum interactive elements expected before flagging a canvas/blind-spot warning.
    pub blind_spot_threshold: usize,
    /// Maximum character length for node text values before truncation.
    pub max_value_len: usize,
}

impl Default for PrunerConfig {
    fn default() -> Self {
        Self {
            drop_zero_sized: true,
            drop_hidden: true,
            blind_spot_threshold: 2,
            max_value_len: 160,
        }
    }
}

/// Prunes noisy OS accessibility trees, redacts passwords, and assigns stable short hash IDs.
pub struct Pruner {
    config: PrunerConfig,
}

impl Pruner {
    pub fn new(config: PrunerConfig) -> Self {
        Self { config }
    }

    pub fn prune(&self, raw: RawWindowTree) -> TreeSnapshot {
        let mut used_ids = HashSet::new();
        let mut interactive_count = 0usize;

        let mut elements = Vec::new();
        self.prune_children(
            &raw.root.children,
            "root",
            &mut used_ids,
            &mut interactive_count,
            &mut elements,
        );

        let blind_spot = interactive_count < self.config.blind_spot_threshold;
        let mut warnings = Vec::new();
        if blind_spot {
            warnings.push(blind_spot_message(&raw.window.app_name, &raw.window.title));
        }
        if raw.truncated {
            warnings.push(format!(
                "Tree traversal reached node/depth budget ({} raw nodes inspected); output is truncated.",
                raw.total_raw_nodes
            ));
        }

        TreeSnapshot {
            snapshot_id: NEXT_SNAPSHOT_ID.fetch_add(1, Ordering::Relaxed),
            window: raw.window,
            elements,
            interactive_count,
            total_raw_nodes: raw.total_raw_nodes,
            truncated: raw.truncated,
            blind_spot,
            warnings,
        }
    }

    fn prune_children(
        &self,
        children: &[RawNode],
        parent_sig: &str,
        used_ids: &mut HashSet<String>,
        interactive_count: &mut usize,
        out: &mut Vec<PrunedNode>,
    ) {
        // Count how many siblings share the same (role, effective_name) so unique controls keep
        // stable IDs even when new items are inserted above them in a list.
        let mut counts: HashMap<(&str, &str), usize> = HashMap::new();
        for child in children {
            let eff = effective_node_name(child);
            *counts.entry((child.role.compact_label(), eff)).or_insert(0) += 1;
        }
        let mut seen_occ: HashMap<(&str, &str), usize> = HashMap::new();

        for child in children {
            let eff = effective_node_name(child);
            let key = (child.role.compact_label(), eff);
            let total = counts.get(&key).copied().unwrap_or(1);
            let occ = seen_occ.entry(key).or_insert(0);
            let disambiguator = if total > 1 || eff.is_empty() {
                Some(*occ)
            } else {
                None
            };
            *occ += 1;

            self.prune_node(
                child,
                parent_sig,
                disambiguator,
                used_ids,
                interactive_count,
                out,
            );
        }
    }

    fn prune_node(
        &self,
        node: &RawNode,
        parent_sig: &str,
        disambiguator: Option<usize>,
        used_ids: &mut HashSet<String>,
        interactive_count: &mut usize,
        out: &mut Vec<PrunedNode>,
    ) {
        if self.config.drop_hidden && !node.state.visible && !node.state.showing {
            return;
        }
        if self.config.drop_zero_sized
            && let Some(bounds) = node.bounds
            && bounds.is_zero_sized()
            && node.children.is_empty()
        {
            return;
        }

        let effective_name = effective_node_name(node);

        let current_sig = match disambiguator {
            Some(idx) => format!(
                "{}/{}:{}:{}",
                parent_sig,
                node.role.compact_label(),
                effective_name,
                idx
            ),
            None => format!(
                "{}/{}:{}",
                parent_sig,
                node.role.compact_label(),
                effective_name
            ),
        };

        let mut pruned_children = Vec::new();
        self.prune_children(
            &node.children,
            &current_sig,
            used_ids,
            interactive_count,
            &mut pruned_children,
        );

        let is_interactive = node.role.is_interactive() || node.state.editable;
        let has_meaningful_text = !effective_name.is_empty()
            || node.value.as_deref().is_some_and(|v| !v.trim().is_empty());

        let keep_node = is_interactive || (node.role.is_semantic() && has_meaningful_text);

        if !keep_node {
            // Flatten: lift meaningful children up, dropping empty container wrappers.
            out.extend(pruned_children);
            return;
        }

        // Drop redundant child Text nodes that merely repeat their parent's label (common in Qt/GTK/Electron).
        if !effective_name.is_empty() {
            pruned_children.retain(|child| {
                !(matches!(child.role, NodeRole::Text)
                    && child.children.is_empty()
                    && child.name.eq_ignore_ascii_case(effective_name))
            });
        }

        let id = if is_interactive {
            *interactive_count += 1;
            Some(allocate_stable_id(&current_sig, used_ids))
        } else {
            None
        };

        let value = sanitize_value(&node.role, node.value.as_deref(), self.config.max_value_len);
        let states = node
            .state
            .active_tags()
            .into_iter()
            .map(str::to_string)
            .collect();

        out.push(PrunedNode {
            id,
            backend_ref: node.backend_ref.clone(),
            role: node.role.clone(),
            name: effective_name.to_string(),
            value,
            states,
            bounds: node.bounds,
            children: pruned_children,
        });
    }
}

fn effective_node_name(node: &RawNode) -> &str {
    if !node.name.trim().is_empty() {
        node.name.trim()
    } else {
        node.description.as_deref().map(str::trim).unwrap_or("")
    }
}

fn blind_spot_message(app_name: &str, title: &str) -> String {
    let haystack = format!("{} {}", app_name.to_lowercase(), title.to_lowercase());
    if [
        "brave", "chrome", "chromium", "code", "electron", "discord", "slack", "obsidian", "opera",
        "vivaldi",
    ]
    .iter()
    .any(|k| haystack.contains(k))
    {
        return "Chromium/Electron app exposes too few accessibility nodes (accessibility tree likely disabled). Run `axdom setup` once and fully restart the app (Ctrl+Q), or use `axdom launch <app>`."
            .to_string();
    }
    if ["jetbrains", "idea", "pycharm", "webstorm", "clion", "java"]
        .iter()
        .any(|k| haystack.contains(k))
    {
        return "Java/JetBrains app exposes too few accessibility nodes (install `java-atk-wrapper` or call `capture_screenshot` for visual inspection)."
            .to_string();
    }
    "Window exposes very few interactive accessibility nodes (possible canvas/game/custom UI); call `capture_screenshot` for visual inspection."
        .to_string()
}

fn sanitize_value(role: &NodeRole, value: Option<&str>, max_len: usize) -> Option<String> {
    if matches!(role, NodeRole::PasswordInput) {
        return Some(REDACTED_VALUE.to_string());
    }
    let raw = value?.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.chars().count() > max_len {
        let truncated: String = raw.chars().take(max_len).collect();
        Some(format!("{truncated}…"))
    } else {
        Some(raw.to_string())
    }
}

/// Generates a deterministic 3-hex-character ID (e.g. `#b4f`) using FNV-1a,
/// resolving rare collisions within a snapshot cleanly.
fn allocate_stable_id(signature: &str, used: &mut HashSet<String>) -> String {
    let mut hash = fnv1a_32(signature.as_bytes());
    for _ in 0..4096 {
        let candidate = format!("#{:03x}", hash & 0x0fff);
        if used.insert(candidate.clone()) {
            return candidate;
        }
        hash = hash.wrapping_add(1);
    }
    let fallback = format!("#{:06x}", fnv1a_32(signature.as_bytes()) & 0x00ff_ffff);
    used.insert(fallback.clone());
    fallback
}

fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &b in bytes {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Bounds, NodeState, WindowInfo};

    fn visible_state() -> NodeState {
        NodeState {
            enabled: true,
            visible: true,
            showing: true,
            sensitive: true,
            ..Default::default()
        }
    }

    fn raw_node(
        role: NodeRole,
        name: &str,
        value: Option<&str>,
        children: Vec<RawNode>,
    ) -> RawNode {
        RawNode {
            backend_ref: format!("test:{name}"),
            role,
            name: name.to_string(),
            description: None,
            value: value.map(str::to_string),
            state: visible_state(),
            bounds: Some(Bounds {
                x: 0,
                y: 0,
                width: 100,
                height: 30,
            }),
            children,
        }
    }

    #[test]
    fn flattens_empty_containers_and_redacts_passwords() {
        let root = raw_node(
            NodeRole::Window,
            "Login",
            None,
            vec![raw_node(
                NodeRole::Container,
                "",
                None,
                vec![
                    raw_node(NodeRole::PasswordInput, "Password", Some("s3cr3t!"), vec![]),
                    raw_node(NodeRole::Button, "Sign In", None, vec![]),
                ],
            )],
        );

        let snapshot = Pruner::new(PrunerConfig::default()).prune(RawWindowTree {
            window: WindowInfo {
                id: "w1".into(),
                title: "Login".into(),
                app_name: "app".into(),
                pid: Some(42),
                active: true,
            },
            root,
            total_raw_nodes: 4,
            truncated: false,
        });

        assert_eq!(snapshot.elements.len(), 2);
        assert_eq!(snapshot.interactive_count, 2);
        assert!(!snapshot.blind_spot);
        assert_eq!(snapshot.elements[0].value.as_deref(), Some(REDACTED_VALUE));
        assert!(snapshot.elements[0].id.as_deref().unwrap().starts_with('#'));
        assert!(snapshot.elements[1].id.as_deref().unwrap().starts_with('#'));
        assert_ne!(snapshot.elements[0].id, snapshot.elements[1].id);
    }

    #[test]
    fn detects_blind_spot_when_no_interactive_nodes() {
        let root = raw_node(NodeRole::Window, "Blender Viewport", None, vec![]);
        let snapshot = Pruner::new(PrunerConfig::default()).prune(RawWindowTree {
            window: WindowInfo {
                id: "w2".into(),
                title: "Blender".into(),
                app_name: "blender".into(),
                pid: Some(99),
                active: true,
            },
            root,
            total_raw_nodes: 1,
            truncated: false,
        });

        assert!(snapshot.blind_spot);
        assert!(!snapshot.warnings.is_empty());
    }

    #[test]
    fn deduplicates_child_text_repeating_parent_label() {
        let root = raw_node(
            NodeRole::Window,
            "Dialog",
            None,
            vec![raw_node(
                NodeRole::Button,
                "Save",
                None,
                vec![raw_node(NodeRole::Text, "Save", None, vec![])],
            )],
        );

        let snapshot = Pruner::new(PrunerConfig::default()).prune(RawWindowTree {
            window: WindowInfo {
                id: "w3".into(),
                title: "Dialog".into(),
                app_name: "app".into(),
                pid: Some(1),
                active: true,
            },
            root,
            total_raw_nodes: 3,
            truncated: false,
        });

        assert_eq!(snapshot.elements.len(), 1);
        assert!(snapshot.elements[0].children.is_empty());
    }

    #[test]
    fn keeps_unique_node_id_stable_when_sibling_inserted_above() {
        let make_tree = |children: Vec<RawNode>| RawWindowTree {
            window: WindowInfo {
                id: "w4".into(),
                title: "App".into(),
                app_name: "app".into(),
                pid: Some(1),
                active: true,
            },
            root: raw_node(NodeRole::Window, "App", None, children),
            total_raw_nodes: 4,
            truncated: false,
        };

        let pruner = Pruner::new(PrunerConfig::default());
        let before = pruner.prune(make_tree(vec![raw_node(
            NodeRole::Button,
            "Save",
            None,
            vec![],
        )]));
        let after = pruner.prune(make_tree(vec![
            raw_node(NodeRole::Button, "New Notification", None, vec![]),
            raw_node(NodeRole::Button, "Save", None, vec![]),
        ]));

        assert_eq!(before.elements[0].id, after.elements[1].id);
    }
}
