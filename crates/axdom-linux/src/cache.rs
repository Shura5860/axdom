use std::collections::{HashMap, HashSet};

use axdom_core::provider::RawWindowTree;
use axdom_core::{RawNode, TreeQuery, WindowInfo};
use zbus::Connection;
use zbus::zvariant::OwnedObjectPath;

use crate::action::parse_backend_ref;
use crate::provider::{decode_state, fetch_value, map_atspi_role};
use crate::proxy::{AtspiCacheItem, cache_proxy};

/// Fast-path tree builder using `org.a11y.atspi.Cache.GetItems` (single D-Bus round-trip),
/// followed by bounded value enrichment for Input/Slider controls.
/// Returns `None` if the application does not expose the AT-SPI2 Cache interface or the window
/// root is not present in the cache, allowing callers to fall back to parallel `walk_node`.
pub async fn fetch_window_tree_via_cache(
    conn: &Connection,
    window: &WindowInfo,
    dest: &str,
    root_path: &OwnedObjectPath,
    query: &TreeQuery,
) -> Option<RawWindowTree> {
    let proxy = cache_proxy(conn, dest.to_string()).await.ok()?;
    let items = proxy.get_items().await.ok()?;
    if items.is_empty() {
        return None;
    }

    let mut tree = build_tree_from_cache_items(window, dest, root_path.as_str(), &items, query)?;

    // Enrich values for Input/Slider controls (bounded to 64 nodes to preserve sub-10ms latency).
    let iface_map: HashMap<String, &[String]> = items
        .iter()
        .map(|it| {
            let d = if it.0.0.is_empty() {
                dest
            } else {
                it.0.0.as_str()
            };
            (format!("{d}:{}", it.0.1.as_str()), it.4.as_slice())
        })
        .collect();
    let mut remaining = 64usize;
    enrich_values(conn, &mut tree.root, &iface_map, &mut remaining).await;

    Some(tree)
}

fn enrich_values<'a>(
    conn: &'a Connection,
    node: &'a mut RawNode,
    iface_map: &'a HashMap<String, &'a [String]>,
    remaining: &'a mut usize,
) -> futures::future::BoxFuture<'a, ()> {
    Box::pin(async move {
        if *remaining == 0 {
            return;
        }
        let needs_val = matches!(
            node.role,
            axdom_core::NodeRole::Input
                | axdom_core::NodeRole::ComboBox
                | axdom_core::NodeRole::Slider
        );
        if needs_val
            && node.value.is_none()
            && let Some(ifaces) = iface_map.get(&node.backend_ref)
            && let Ok((d, p)) = parse_backend_ref(&node.backend_ref)
        {
            *remaining -= 1;
            node.value = fetch_value(conn, &d, &p, ifaces, &node.role).await;
        }
        for child in &mut node.children {
            enrich_values(conn, child, iface_map, remaining).await;
        }
    })
}

/// Pure function reconstructing a `RawWindowTree` from a flat `Cache.GetItems` list.
pub fn build_tree_from_cache_items(
    window: &WindowInfo,
    dest: &str,
    root_path: &str,
    items: &[AtspiCacheItem],
    query: &TreeQuery,
) -> Option<RawWindowTree> {
    let mut index: HashMap<(&str, &str), &AtspiCacheItem> = HashMap::with_capacity(items.len());
    for item in items {
        let (ref obj_dest, ref obj_path) = item.0;
        let key_dest = if obj_dest.is_empty() {
            dest
        } else {
            obj_dest.as_str()
        };
        index.insert((key_dest, obj_path.as_str()), item);
    }

    if !index.contains_key(&(dest, root_path)) {
        return None;
    }

    let mut visited_count = 0usize;
    let mut truncated = false;
    let mut active_stack: HashSet<(&str, &str)> = HashSet::new();

    let root = build_node(
        &index,
        dest,
        root_path,
        0,
        query,
        &mut visited_count,
        &mut truncated,
        &mut active_stack,
    )?;

    Some(RawWindowTree {
        window: window.clone(),
        root,
        total_raw_nodes: visited_count,
        truncated,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_node<'a>(
    index: &HashMap<(&'a str, &'a str), &'a AtspiCacheItem>,
    dest: &'a str,
    path: &'a str,
    depth: usize,
    query: &TreeQuery,
    visited_count: &mut usize,
    truncated: &mut bool,
    active_stack: &mut HashSet<(&'a str, &'a str)>,
) -> Option<RawNode> {
    if *visited_count >= query.max_nodes {
        *truncated = true;
        return None;
    }
    // Guard against malformed parent/child cycles in buggy toolkits.
    if !active_stack.insert((dest, path)) {
        return None;
    }

    let item = *index.get(&(dest, path))?;
    *visited_count += 1;

    let children_refs = &item.3;
    let mut children = Vec::new();
    if depth < query.max_depth {
        for (c_dest, c_path) in children_refs {
            let child_dest = if c_dest.is_empty() {
                dest
            } else {
                c_dest.as_str()
            };
            if let Some(child_node) = build_node(
                index,
                child_dest,
                c_path.as_str(),
                depth + 1,
                query,
                visited_count,
                truncated,
                active_stack,
            ) {
                children.push(child_node);
            }
        }
    } else if !children_refs.is_empty() {
        *truncated = true;
    }

    active_stack.remove(&(dest, path));

    Some(RawNode {
        backend_ref: format!("{dest}:{path}"),
        role: map_atspi_role(item.6),
        name: item.5.clone(),
        description: (!item.7.trim().is_empty()).then(|| item.7.clone()),
        value: None,
        state: decode_state(&item.8),
        bounds: None,
        children,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axdom_core::NodeRole;

    fn path(s: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(s).unwrap()
    }

    fn item(
        dest: &str,
        p: &str,
        children: Vec<(&str, &str)>,
        name: &str,
        role: u32,
    ) -> AtspiCacheItem {
        (
            (dest.to_string(), path(p)),
            (dest.to_string(), path("/app")),
            (dest.to_string(), path("/parent")),
            children
                .into_iter()
                .map(|(cd, cp)| (cd.to_string(), path(cp)))
                .collect(),
            vec!["Accessible".to_string()],
            name.to_string(),
            role,
            String::new(),
            vec![(1 << 8) | (1 << 24) | (1 << 25) | (1 << 30)], // enabled | sensitive | showing | visible
        )
    }

    #[test]
    fn reconstructs_tree_from_flat_cache_items() {
        let win = WindowInfo {
            id: ":1.9:/win".into(),
            title: "Editor".into(),
            app_name: "gedit".into(),
            pid: Some(77),
            active: true,
        };
        let items = vec![
            item(
                ":1.9",
                "/win",
                vec![(":1.9", "/btn1"), (":1.9", "/btn2")],
                "Editor",
                23,
            ),
            item(":1.9", "/btn1", vec![], "Save", 43),
            item(":1.9", "/btn2", vec![], "Close", 43),
        ];

        let tree = build_tree_from_cache_items(&win, ":1.9", "/win", &items, &TreeQuery::default())
            .expect("tree reconstructed");

        assert_eq!(tree.total_raw_nodes, 3);
        assert!(!tree.truncated);
        assert_eq!(tree.root.children.len(), 2);
        assert_eq!(tree.root.children[0].role, NodeRole::Button);
        assert_eq!(tree.root.children[0].name, "Save");
    }

    #[test]
    fn respects_max_nodes_budget() {
        let win = WindowInfo {
            id: ":1.9:/win".into(),
            title: "Editor".into(),
            app_name: "gedit".into(),
            pid: Some(77),
            active: true,
        };
        let items = vec![
            item(
                ":1.9",
                "/win",
                vec![(":1.9", "/btn1"), (":1.9", "/btn2")],
                "Editor",
                23,
            ),
            item(":1.9", "/btn1", vec![], "Save", 43),
            item(":1.9", "/btn2", vec![], "Close", 43),
        ];
        let query = TreeQuery {
            max_nodes: 2,
            ..Default::default()
        };

        let tree = build_tree_from_cache_items(&win, ":1.9", "/win", &items, &query)
            .expect("tree reconstructed");

        assert_eq!(tree.total_raw_nodes, 2);
        assert!(tree.truncated);
    }
}
