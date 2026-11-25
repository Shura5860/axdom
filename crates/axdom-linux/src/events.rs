use std::collections::HashMap;

use axdom_core::{A11yEvent, AxdomError, Result};
use futures::StreamExt;
use tokio::sync::mpsc;
use zbus::fdo::DBusProxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, MessageStream};

use crate::proxy::EventRegistryProxy;

const DEFAULT_EVENTS: &[&str] = &[
    "focus:",
    "window:activate",
    "object:state-changed:focused",
    "object:children-changed",
];

const MATCH_INTERFACES: &[&str] = &[
    "org.a11y.atspi.Event.Focus",
    "org.a11y.atspi.Event.Window",
    "org.a11y.atspi.Event.Object",
];

/// Subscribes to live AT-SPI2 focus, window, and object-tree events on the accessibility bus.
pub async fn watch_events(conn: Connection, tx: mpsc::Sender<A11yEvent>) -> Result<()> {
    // 1. Tell AT-SPI2 Registry which event categories we want apps to emit.
    if let Ok(registry) = EventRegistryProxy::new(&conn).await {
        for ev in DEFAULT_EVENTS {
            let _ = registry.register_event(ev).await;
        }
    }

    // 2. Add D-Bus signal match rules on the a11y bus.
    let dbus = DBusProxy::new(&conn)
        .await
        .map_err(|e| AxdomError::Backend(format!("DBusProxy: {e}")))?;

    for iface in MATCH_INTERFACES {
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(*iface)
            .map_err(|e| AxdomError::Backend(format!("match rule {iface}: {e}")))?
            .build();
        dbus.add_match_rule(rule)
            .await
            .map_err(|e| AxdomError::Backend(format!("add_match_rule {iface}: {e}")))?;
    }

    let mut stream = MessageStream::from(&conn);
    while let Some(msg_res) = stream.next().await {
        let Ok(msg) = msg_res else {
            continue;
        };
        let header = msg.header();
        let Some(iface) = header.interface() else {
            continue;
        };
        let iface_str = iface.as_str();
        if !iface_str.starts_with("org.a11y.atspi.Event.") {
            continue;
        }

        let category = iface_str
            .trim_start_matches("org.a11y.atspi.Event.")
            .to_lowercase();
        let kind = header
            .member()
            .map(|m| m.as_str().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let sender = header
            .sender()
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();
        let path = header
            .path()
            .map(|p| p.as_str().to_string())
            .unwrap_or_default();

        // Standard AT-SPI2 event signal body is `(siiva{sv})` (or legacy `(siiv(so))`).
        let body = msg.body();
        let detail = body
            .deserialize::<(String, i32, i32, OwnedValue, HashMap<String, OwnedValue>)>()
            .map(|(d, d1, d2, _, _)| format_event_detail(&d, d1, d2))
            .or_else(|_| {
                body.deserialize::<(String, i32, i32, OwnedValue, (String, OwnedObjectPath))>()
                    .map(|(d, d1, d2, _, _)| format_event_detail(&d, d1, d2))
            })
            .unwrap_or_default();

        let event = A11yEvent {
            category,
            kind,
            detail,
            source_ref: format!("{sender}:{path}"),
        };

        if tx.send(event).await.is_err() {
            break;
        }
    }

    if let Ok(registry) = EventRegistryProxy::new(&conn).await {
        for ev in DEFAULT_EVENTS {
            let _ = registry.deregister_event(ev).await;
        }
    }

    Ok(())
}

fn format_event_detail(d: &str, d1: i32, d2: i32) -> String {
    if d.is_empty() {
        format!("detail1={d1}, detail2={d2}")
    } else {
        format!("{d} ({d1}, {d2})")
    }
}
