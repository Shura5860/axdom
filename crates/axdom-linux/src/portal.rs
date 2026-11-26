use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axdom_core::{ActionResult, AxdomError, KeyCombo, KeyModifier, Result};
use futures::StreamExt;
use tokio::sync::Mutex;
use zbus::fdo::DBusProxy;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, proxy};

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const DEVICE_KEYBOARD: u32 = 1;
const DEVICE_POINTER: u32 = 2;
pub const BTN_LEFT: i32 = 272;
pub const BTN_RIGHT: i32 = 273;
const PORTAL_TIMEOUT: Duration = Duration::from_secs(20);

static TOKEN_SEQ: AtomicU64 = AtomicU64::new(1);

#[proxy(
    interface = "org.freedesktop.portal.Screenshot",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait PortalScreenshot {
    fn screenshot(
        &self,
        parent_window: &str,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.freedesktop.portal.RemoteDesktop",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait RemoteDesktop {
    fn create_session(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<OwnedObjectPath>;

    fn select_devices(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;

    fn start(
        &self,
        session_handle: ObjectPath<'_>,
        parent_window: &str,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;

    fn notify_keyboard_keysym(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<&str, Value<'_>>,
        keysym: i32,
        state: u32,
    ) -> zbus::Result<()>;

    fn notify_pointer_axis_discrete(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<&str, Value<'_>>,
        axis: u32,
        steps: i32,
    ) -> zbus::Result<()>;

    fn notify_pointer_motion_absolute(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<&str, Value<'_>>,
        stream: u32,
        x: f64,
        y: f64,
    ) -> zbus::Result<()>;

    fn notify_pointer_button(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<&str, Value<'_>>,
        button: i32,
        state: u32,
    ) -> zbus::Result<()>;
}

struct PortalSession {
    conn: Connection,
    session_handle: OwnedObjectPath,
}

fn shared_session() -> &'static Mutex<Option<PortalSession>> {
    static CELL: OnceLock<Mutex<Option<PortalSession>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

fn next_token(prefix: &str) -> String {
    let seq = TOKEN_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("axdom_{prefix}_{}_{seq}", std::process::id())
}

/// Sends a keyboard combination over the native Linux XDG Desktop Portal `RemoteDesktop`
/// D-Bus interface (built into KDE KWin, GNOME Mutter, and Hyprland — zero external tools).
pub async fn send_key_via_portal(combo: &KeyCombo) -> Result<ActionResult> {
    let keysym = keysym_for_key(&combo.key).ok_or_else(|| {
        AxdomError::ActionFailed(format!("unsupported keysym for key {:?}", combo.key))
    })?;
    let mod_syms: Vec<i32> = combo
        .modifiers
        .iter()
        .map(|m| modifier_keysym(*m))
        .collect();

    let mut guard = shared_session().lock().await;
    if guard.is_none() {
        *guard = Some(open_portal_session().await?);
    }

    let send_res = {
        let sess = guard.as_ref().expect("initialized above");
        emit_combo(&sess.conn, &sess.session_handle, &mod_syms, keysym).await
    };

    if send_res.is_err() {
        // Session may have been closed by the compositor; reconnect once.
        let fresh = open_portal_session().await?;
        emit_combo(&fresh.conn, &fresh.session_handle, &mod_syms, keysym).await?;
        *guard = Some(fresh);
    }

    Ok(ActionResult {
        node_id: "keyboard".to_string(),
        action: "press_key".to_string(),
        method_used: "portal:RemoteDesktop.NotifyKeyboardKeysym".to_string(),
        success: true,
        detail: Some(format!("keysym=0x{keysym:x}")),
    })
}

/// Sends vertical and/or horizontal scroll steps via XDG `RemoteDesktop` portal.
pub async fn send_scroll_via_portal(
    node_id: &str,
    delta_x: i32,
    delta_y: i32,
) -> Result<ActionResult> {
    let mut guard = shared_session().lock().await;
    if guard.is_none() {
        *guard = Some(open_portal_session().await?);
    }

    let send_res = {
        let sess = guard.as_ref().expect("initialized above");
        emit_scroll(&sess.conn, &sess.session_handle, delta_x, delta_y).await
    };

    if send_res.is_err() {
        let fresh = open_portal_session().await?;
        emit_scroll(&fresh.conn, &fresh.session_handle, delta_x, delta_y).await?;
        *guard = Some(fresh);
    }

    Ok(ActionResult {
        node_id: node_id.to_string(),
        action: "scroll".to_string(),
        method_used: "portal:RemoteDesktop.NotifyPointerAxisDiscrete".to_string(),
        success: true,
        detail: Some(format!("delta_x={delta_x}, delta_y={delta_y}")),
    })
}

/// Moves the pointer to absolute `(x, y)` and clicks `button` (`BTN_LEFT`/`BTN_RIGHT`) `count` times
/// via native XDG `RemoteDesktop` portal (zero external tools like `ydotool` required on Wayland).
pub async fn send_pointer_click_via_portal(
    node_id: &str,
    x: i32,
    y: i32,
    button: i32,
    count: u32,
    action_name: &str,
) -> Result<ActionResult> {
    let mut guard = shared_session().lock().await;
    if guard.is_none() {
        *guard = Some(open_portal_session().await?);
    }

    let send_res = {
        let sess = guard.as_ref().expect("initialized above");
        emit_pointer_click(&sess.conn, &sess.session_handle, x, y, button, count).await
    };

    if send_res.is_err() {
        let fresh = open_portal_session().await?;
        emit_pointer_click(&fresh.conn, &fresh.session_handle, x, y, button, count).await?;
        *guard = Some(fresh);
    }

    Ok(ActionResult {
        node_id: node_id.to_string(),
        action: action_name.to_string(),
        method_used: format!("portal:RemoteDesktop.Pointer({x},{y},btn={button},n={count})"),
        success: true,
        detail: None,
    })
}

/// Moves the pointer to absolute `(x, y)` (hover) via XDG `RemoteDesktop` portal.
pub async fn send_pointer_hover_via_portal(node_id: &str, x: i32, y: i32) -> Result<ActionResult> {
    let mut guard = shared_session().lock().await;
    if guard.is_none() {
        *guard = Some(open_portal_session().await?);
    }

    let send_res = {
        let sess = guard.as_ref().expect("initialized above");
        emit_pointer_move(&sess.conn, &sess.session_handle, x, y).await
    };

    if send_res.is_err() {
        let fresh = open_portal_session().await?;
        emit_pointer_move(&fresh.conn, &fresh.session_handle, x, y).await?;
        *guard = Some(fresh);
    }

    Ok(ActionResult {
        node_id: node_id.to_string(),
        action: "hover".to_string(),
        method_used: format!("portal:RemoteDesktop.Motion({x},{y})"),
        success: true,
        detail: None,
    })
}

/// Drags with left mouse button held from `(from_x, from_y)` to `(to_x, to_y)` via XDG portal.
pub async fn send_pointer_drag_via_portal(
    node_id: &str,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
) -> Result<ActionResult> {
    let mut guard = shared_session().lock().await;
    if guard.is_none() {
        *guard = Some(open_portal_session().await?);
    }

    let send_res = {
        let sess = guard.as_ref().expect("initialized above");
        emit_pointer_drag(&sess.conn, &sess.session_handle, from_x, from_y, to_x, to_y).await
    };

    if send_res.is_err() {
        let fresh = open_portal_session().await?;
        emit_pointer_drag(
            &fresh.conn,
            &fresh.session_handle,
            from_x,
            from_y,
            to_x,
            to_y,
        )
        .await?;
        *guard = Some(fresh);
    }

    Ok(ActionResult {
        node_id: node_id.to_string(),
        action: "drag_to".to_string(),
        method_used: format!("portal:RemoteDesktop.Drag({from_x},{from_y}->{to_x},{to_y})"),
        success: true,
        detail: None,
    })
}

/// Types a short plain-text string character-by-character via XDG `RemoteDesktop` keysyms
/// (fallback for rich JS/contenteditable editors that do not expose `EditableText`).
/// When `clear_existing` is true, sends `Ctrl+A` then `BackSpace` before typing to match
/// `EditableText.SetTextContents` replacement semantics.
pub async fn type_text_via_portal(
    node_id: &str,
    text: &str,
    clear_existing: bool,
) -> Result<ActionResult> {
    let syms: Vec<i32> = text
        .chars()
        .filter_map(|c| keysym_for_key(&c.to_string()))
        .collect();
    if syms.is_empty() && !text.is_empty() {
        return Err(AxdomError::ActionFailed(
            "no mappable keysyms in text for portal typing".to_string(),
        ));
    }

    let mut guard = shared_session().lock().await;
    if guard.is_none() {
        *guard = Some(open_portal_session().await?);
    }

    let send_res = {
        let sess = guard.as_ref().expect("initialized above");
        emit_text(&sess.conn, &sess.session_handle, &syms, clear_existing).await
    };

    if send_res.is_err() {
        let fresh = open_portal_session().await?;
        emit_text(&fresh.conn, &fresh.session_handle, &syms, clear_existing).await?;
        *guard = Some(fresh);
    }

    Ok(ActionResult {
        node_id: node_id.to_string(),
        action: "set_text".to_string(),
        method_used: "portal:RemoteDesktop.TypeKeysyms".to_string(),
        success: true,
        detail: Some(format!("typed {} chars", syms.len())),
    })
}

async fn emit_text(
    conn: &Connection,
    session_handle: &OwnedObjectPath,
    syms: &[i32],
    clear_existing: bool,
) -> Result<()> {
    if clear_existing {
        let ctrl = modifier_keysym(KeyModifier::Ctrl);
        emit_combo(conn, session_handle, &[ctrl], 'a' as i32).await?;
        emit_combo(conn, session_handle, &[], 0xff08).await?; // BackSpace
    }
    for &sym in syms {
        emit_combo(conn, session_handle, &[], sym).await?;
    }
    Ok(())
}

/// Captures a PNG screenshot via the native XDG Desktop Portal `Screenshot` D-Bus interface
/// (zero external CLI tools required on KDE, GNOME, or Hyprland).
pub async fn capture_screenshot_via_portal() -> Result<(Vec<u8>, String)> {
    let conn = Connection::session()
        .await
        .map_err(|e| AxdomError::BusUnavailable(format!("session D-Bus for portal: {e}")))?;

    let proxy = PortalScreenshotProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|e| AxdomError::ActionUnsupported(format!("XDG Screenshot portal: {e}")))?;

    let dbus = DBusProxy::new(&conn)
        .await
        .map_err(|e| AxdomError::Backend(format!("DBusProxy: {e}")))?;
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(REQUEST_IFACE)
        .map_err(|e| AxdomError::Backend(e.to_string()))?
        .member("Response")
        .map_err(|e| AxdomError::Backend(e.to_string()))?
        .build();
    dbus.add_match_rule(rule)
        .await
        .map_err(|e| AxdomError::Backend(format!("add_match_rule portal screenshot: {e}")))?;

    let mut stream = MessageStream::from(&conn);
    let token = next_token("shot");
    let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
    opts.insert("handle_token", Value::from(token.as_str()));
    opts.insert("interactive", Value::from(false));

    let req_path = proxy
        .screenshot("", opts)
        .await
        .map_err(|e| AxdomError::ActionUnsupported(format!("Screenshot portal call: {e}")))?;
    let results = wait_for_portal_response(&mut stream, &req_path, "Screenshot").await?;
    let uri: String = results
        .get("uri")
        .and_then(|v| v.downcast_ref::<String>().ok())
        .ok_or_else(|| AxdomError::ActionFailed("Screenshot portal returned no uri".into()))?;

    let file_path = decode_file_uri(&uri);
    let bytes = tokio::fs::read(&file_path).await.map_err(|e| {
        AxdomError::ActionFailed(format!("read portal screenshot {file_path}: {e}"))
    })?;
    // Only clean up temporary files; never delete screenshots saved into user directories like ~/Pictures.
    let tmp_dir = std::env::temp_dir();
    if file_path.starts_with("/tmp/") || std::path::Path::new(&file_path).starts_with(&tmp_dir) {
        let _ = tokio::fs::remove_file(&file_path).await;
    }

    Ok((bytes, "screenshot:xdg-portal".to_string()))
}

async fn emit_scroll(
    conn: &Connection,
    session_handle: &OwnedObjectPath,
    delta_x: i32,
    delta_y: i32,
) -> Result<()> {
    let proxy = RemoteDesktopProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("RemoteDesktop proxy: {e}")))?;
    // axis = 0 is vertical, axis = 1 is horizontal in XDG RemoteDesktop spec.
    if delta_y != 0 {
        proxy
            .notify_pointer_axis_discrete(session_handle.as_ref(), HashMap::new(), 0, delta_y)
            .await
            .map_err(|e| AxdomError::ActionFailed(format!("portal vertical scroll: {e}")))?;
    }
    if delta_x != 0 {
        proxy
            .notify_pointer_axis_discrete(session_handle.as_ref(), HashMap::new(), 1, delta_x)
            .await
            .map_err(|e| AxdomError::ActionFailed(format!("portal horizontal scroll: {e}")))?;
    }
    Ok(())
}

async fn emit_pointer_move(
    conn: &Connection,
    session_handle: &OwnedObjectPath,
    x: i32,
    y: i32,
) -> Result<()> {
    let proxy = RemoteDesktopProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("RemoteDesktop proxy: {e}")))?;
    proxy
        .notify_pointer_motion_absolute(
            session_handle.as_ref(),
            HashMap::new(),
            0,
            x as f64,
            y as f64,
        )
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("portal pointer motion: {e}")))
}

async fn emit_pointer_click(
    conn: &Connection,
    session_handle: &OwnedObjectPath,
    x: i32,
    y: i32,
    button: i32,
    count: u32,
) -> Result<()> {
    let proxy = RemoteDesktopProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("RemoteDesktop proxy: {e}")))?;
    proxy
        .notify_pointer_motion_absolute(
            session_handle.as_ref(),
            HashMap::new(),
            0,
            x as f64,
            y as f64,
        )
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("portal pointer motion: {e}")))?;
    for _ in 0..count.max(1) {
        proxy
            .notify_pointer_button(session_handle.as_ref(), HashMap::new(), button, 1)
            .await
            .map_err(|e| AxdomError::ActionFailed(format!("portal button down: {e}")))?;
        proxy
            .notify_pointer_button(session_handle.as_ref(), HashMap::new(), button, 0)
            .await
            .map_err(|e| AxdomError::ActionFailed(format!("portal button up: {e}")))?;
    }
    Ok(())
}

async fn emit_pointer_drag(
    conn: &Connection,
    session_handle: &OwnedObjectPath,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
) -> Result<()> {
    let proxy = RemoteDesktopProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("RemoteDesktop proxy: {e}")))?;
    proxy
        .notify_pointer_motion_absolute(
            session_handle.as_ref(),
            HashMap::new(),
            0,
            from_x as f64,
            from_y as f64,
        )
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("portal drag start motion: {e}")))?;
    proxy
        .notify_pointer_button(session_handle.as_ref(), HashMap::new(), BTN_LEFT, 1)
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("portal drag button down: {e}")))?;

    let steps = 6;
    for step in 1..=steps {
        let t = step as f64 / steps as f64;
        let cx = from_x as f64 + (to_x - from_x) as f64 * t;
        let cy = from_y as f64 + (to_y - from_y) as f64 * t;
        let _ = proxy
            .notify_pointer_motion_absolute(session_handle.as_ref(), HashMap::new(), 0, cx, cy)
            .await;
        tokio::time::sleep(Duration::from_millis(12)).await;
    }

    proxy
        .notify_pointer_button(session_handle.as_ref(), HashMap::new(), BTN_LEFT, 0)
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("portal drag button up: {e}")))?;
    Ok(())
}

async fn emit_combo(
    conn: &Connection,
    session_handle: &OwnedObjectPath,
    mod_syms: &[i32],
    keysym: i32,
) -> Result<()> {
    let proxy = RemoteDesktopProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("RemoteDesktop proxy: {e}")))?;

    for &m in mod_syms {
        proxy
            .notify_keyboard_keysym(session_handle.as_ref(), HashMap::new(), m, 1)
            .await
            .map_err(|e| AxdomError::ActionFailed(format!("portal key down 0x{m:x}: {e}")))?;
    }
    proxy
        .notify_keyboard_keysym(session_handle.as_ref(), HashMap::new(), keysym, 1)
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("portal key down 0x{keysym:x}: {e}")))?;
    proxy
        .notify_keyboard_keysym(session_handle.as_ref(), HashMap::new(), keysym, 0)
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("portal key up 0x{keysym:x}: {e}")))?;
    for &m in mod_syms.iter().rev() {
        let _ = proxy
            .notify_keyboard_keysym(session_handle.as_ref(), HashMap::new(), m, 0)
            .await;
    }
    Ok(())
}

async fn open_portal_session() -> Result<PortalSession> {
    let conn = Connection::session()
        .await
        .map_err(|e| AxdomError::BusUnavailable(format!("session D-Bus for portal: {e}")))?;

    let proxy = RemoteDesktopProxy::builder(&conn)
        .destination(PORTAL_DEST)
        .map_err(|e| AxdomError::Backend(e.to_string()))?
        .path(PORTAL_PATH)
        .map_err(|e| AxdomError::Backend(e.to_string()))?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|e| AxdomError::ActionUnsupported(format!("XDG RemoteDesktop portal: {e}")))?;

    // Subscribe to org.freedesktop.portal.Request::Response signals before issuing requests.
    let dbus = DBusProxy::new(&conn)
        .await
        .map_err(|e| AxdomError::Backend(format!("DBusProxy: {e}")))?;
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(REQUEST_IFACE)
        .map_err(|e| AxdomError::Backend(e.to_string()))?
        .member("Response")
        .map_err(|e| AxdomError::Backend(e.to_string()))?
        .build();
    dbus.add_match_rule(rule)
        .await
        .map_err(|e| AxdomError::Backend(format!("add_match_rule portal: {e}")))?;

    let mut stream = MessageStream::from(&conn);

    // 1. CreateSession
    let handle_token = next_token("create");
    let session_token = next_token("sess");
    let mut create_opts: HashMap<&str, Value<'_>> = HashMap::new();
    create_opts.insert("handle_token", Value::from(handle_token.as_str()));
    create_opts.insert("session_handle_token", Value::from(session_token.as_str()));

    let req_path = proxy
        .create_session(create_opts)
        .await
        .map_err(|e| AxdomError::ActionUnsupported(format!("RemoteDesktop.CreateSession: {e}")))?;
    let results = wait_for_portal_response(&mut stream, &req_path, "RemoteDesktop").await?;
    let session_str: String = results
        .get("session_handle")
        .and_then(|v| v.downcast_ref::<String>().ok())
        .ok_or_else(|| {
            AxdomError::ActionFailed(
                "RemoteDesktop.CreateSession returned no session_handle".into(),
            )
        })?;
    let session_handle = OwnedObjectPath::try_from(session_str.as_str())
        .map_err(|e| AxdomError::Backend(format!("invalid session_handle '{session_str}': {e}")))?;

    // 2. SelectDevices (keyboard + pointer, persist_mode=2 + saved restore_token to avoid re-prompting)
    let saved_token = load_restore_token();
    let sel_token = next_token("select");
    let mut sel_opts: HashMap<&str, Value<'_>> = HashMap::new();
    sel_opts.insert("handle_token", Value::from(sel_token.as_str()));
    sel_opts.insert("types", Value::from(DEVICE_KEYBOARD | DEVICE_POINTER));
    sel_opts.insert("persist_mode", Value::from(2u32));
    if let Some(ref tok) = saved_token {
        sel_opts.insert("restore_token", Value::from(tok.as_str()));
    }

    let sel_req = proxy
        .select_devices(session_handle.as_ref(), sel_opts)
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("RemoteDesktop.SelectDevices: {e}")))?;
    wait_for_portal_response(&mut stream, &sel_req, "RemoteDesktop").await?;

    // 3. Start (prompts once when no valid restore_token exists; saves the returned token for future runs)
    let start_token = next_token("start");
    let mut start_opts: HashMap<&str, Value<'_>> = HashMap::new();
    start_opts.insert("handle_token", Value::from(start_token.as_str()));

    let start_req = proxy
        .start(session_handle.as_ref(), "", start_opts)
        .await
        .map_err(|e| AxdomError::ActionFailed(format!("RemoteDesktop.Start: {e}")))?;
    let start_results = wait_for_portal_response(&mut stream, &start_req, "RemoteDesktop").await?;
    if let Some(new_token) = start_results
        .get("restore_token")
        .and_then(|v| v.downcast_ref::<String>().ok())
        .filter(|s| !s.trim().is_empty())
    {
        save_restore_token(&new_token);
    }

    Ok(PortalSession {
        conn,
        session_handle,
    })
}

fn portal_token_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))?;
    Some(base.join("axdom").join("portal-token"))
}

fn load_restore_token() -> Option<String> {
    let path = portal_token_path()?;
    let tok = std::fs::read_to_string(path).ok()?;
    let trimmed = tok.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Revokes the cached XDG RemoteDesktop `restore_token` so future sessions will prompt again.
pub fn revoke_restore_token() -> std::io::Result<bool> {
    let Some(path) = portal_token_path() else {
        return Ok(false);
    };
    if path.is_file() {
        std::fs::remove_file(&path)?;
        return Ok(true);
    }
    Ok(false)
}

fn save_restore_token(token: &str) {
    use std::io::Write;
    let Some(path) = portal_token_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut file) = opts.open(&path) {
        let _ = writeln!(file, "{}", token.trim());
    }
}

async fn wait_for_portal_response(
    stream: &mut MessageStream,
    expected_path: &OwnedObjectPath,
    portal_name: &str,
) -> Result<HashMap<String, OwnedValue>> {
    let fut = async {
        while let Some(msg_res) = stream.next().await {
            let Ok(msg) = msg_res else {
                continue;
            };
            let header = msg.header();
            if header.path().map(|p| p.as_str()) != Some(expected_path.as_str()) {
                continue;
            }
            if header.interface().map(|i| i.as_str()) != Some(REQUEST_IFACE) {
                continue;
            }
            let (code, results): (u32, HashMap<String, OwnedValue>) = msg
                .body()
                .deserialize()
                .map_err(|e| AxdomError::Backend(format!("portal response decode: {e}")))?;
            if code != 0 {
                return Err(AxdomError::ActionFailed(format!(
                    "XDG {portal_name} portal request rejected or cancelled (code={code})"
                )));
            }
            return Ok(results);
        }
        Err(AxdomError::Backend(
            "D-Bus stream closed while waiting for XDG portal response".to_string(),
        ))
    };

    tokio::time::timeout(PORTAL_TIMEOUT, fut)
        .await
        .map_err(|_| AxdomError::Timeout(PORTAL_TIMEOUT.as_millis() as u64))?
}

fn decode_file_uri(uri: &str) -> String {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            decoded.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(decoded).unwrap_or_else(|_| raw.to_string())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn modifier_keysym(m: KeyModifier) -> i32 {
    match m {
        KeyModifier::Ctrl => 0xffe3,  // XK_Control_L
        KeyModifier::Alt => 0xffe9,   // XK_Alt_L
        KeyModifier::Shift => 0xffe1, // XK_Shift_L
        KeyModifier::Super => 0xffeb, // XK_Super_L
    }
}

/// Maps key names (`Enter`, `Escape`, `Tab`, `F5`, `a`, `S`, `" "`) to X11 keysym integers.
pub fn keysym_for_key(key: &str) -> Option<i32> {
    if key == " " {
        return Some(0x0020);
    }
    let lower = key.trim().to_lowercase();
    let sym = match lower.as_str() {
        "enter" | "return" => 0xff0d,
        "esc" | "escape" => 0xff1b,
        "tab" => 0xff09,
        "space" => 0x0020,
        "backspace" => 0xff08,
        "delete" | "del" => 0xffff,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" | "page_up" => 0xff55,
        "pagedown" | "page_down" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "f1" => 0xffbe,
        "f2" => 0xffbf,
        "f3" => 0xffc0,
        "f4" => 0xffc1,
        "f5" => 0xffc2,
        "f6" => 0xffc3,
        "f7" => 0xffc4,
        "f8" => 0xffc5,
        "f9" => 0xffc6,
        "f10" => 0xffc7,
        "f11" => 0xffc8,
        "f12" => 0xffc9,
        _ => {
            let mut chars = key.trim().chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            let code = ch as u32;
            if (0x20..=0x7e).contains(&code) {
                code as i32
            } else {
                (0x0100_0000 | code) as i32
            }
        }
    };
    Some(sym)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_keysyms() {
        assert_eq!(keysym_for_key("Enter"), Some(0xff0d));
        assert_eq!(keysym_for_key("Escape"), Some(0xff1b));
        assert_eq!(keysym_for_key("F4"), Some(0xffc1));
        assert_eq!(keysym_for_key("s"), Some('s' as i32));
        assert_eq!(keysym_for_key(" "), Some(0x0020));
        assert_eq!(keysym_for_key("space"), Some(0x0020));
        assert_eq!(keysym_for_key("UnknownKeyName"), None);
    }

    #[test]
    fn decodes_percent_encoded_file_uris() {
        assert_eq!(
            decode_file_uri("file:///tmp/my%20screenshot%201.png"),
            "/tmp/my screenshot 1.png"
        );
        assert_eq!(
            decode_file_uri("file:///home/user/Pictures/Ekran%20G%C3%B6r%C3%BCnt%C3%BCs%C3%BC.png"),
            "/home/user/Pictures/Ekran Görüntüsü.png"
        );
    }
}
