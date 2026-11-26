#![cfg(target_os = "macos")]

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use axdom_core::provider::RawWindowTree;
use axdom_core::{
    AccessibilityProvider, ActionRequest, ActionResult, AxdomError, Bounds, CheckStatus,
    DoctorCheck, DoctorReport, KeyCombo, NodeRef, NodeRole, NodeState, RawNode, Result, Screenshot,
    TreeQuery, WindowInfo, crop_png_bytes, encode_base64,
};

use crate::mapping::{cg_flags_for_modifiers, cg_keycode_for_key, map_ax_role};

const MAX_RAW_DEPTH: usize = 48;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_AX_VALUE_CGPOINT_TYPE: u32 = 1;
const K_AX_VALUE_CGSIZE_TYPE: u32 = 2;
const K_CG_HID_EVENT_TAP: u32 = 0;

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type AXUIElementRef = *const c_void;
type CGEventRef = *mut c_void;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> i32;
    fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> i32;
    fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> i32;
    fn AXValueGetValue(value: CFTypeRef, value_type: u32, value_ptr: *mut c_void) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFStringCreateWithCString(
        alloc: CFTypeRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFStringGetCString(
        the_string: CFStringRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> bool;
    fn CFArrayGetCount(the_array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(the_array: CFArrayRef, idx: isize) -> CFTypeRef;
    fn CFDictionaryGetValue(the_dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFNumberGetValue(number: CFTypeRef, the_type: isize, value_ptr: *mut c_void) -> bool;
    fn CFBooleanGetValue(boolean: CFTypeRef) -> bool;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    static kCFBooleanTrue: CFTypeRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFArrayRef;
    fn CGEventCreateKeyboardEvent(source: CFTypeRef, keycode: u16, key_down: bool) -> CGEventRef;
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventCreateScrollWheelEvent(
        source: CFTypeRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        ...
    ) -> CGEventRef;
    fn CGEventCreateMouseEvent(
        source: CFTypeRef,
        mouse_type: u32,
        mouse_cursor_position: CGPoint,
        mouse_button: u32,
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
}

struct CfGuard(CFTypeRef);
impl Drop for CfGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

fn cf_str(s: &str) -> CfGuard {
    let c = CString::new(s).unwrap_or_default();
    let ptr =
        unsafe { CFStringCreateWithCString(ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8) };
    CfGuard(ptr)
}

fn read_cf_string(cf: CFTypeRef) -> Option<String> {
    if cf.is_null() {
        return None;
    }
    unsafe {
        if CFGetTypeID(cf) != CFStringGetTypeID() {
            return None;
        }
        let mut buf = [0i8; 2048];
        if CFStringGetCString(
            cf,
            buf.as_mut_ptr(),
            buf.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
        ) {
            return CStr::from_ptr(buf.as_ptr())
                .to_str()
                .ok()
                .map(ToOwned::to_owned);
        }
    }
    None
}

fn copy_attr(el: AXUIElementRef, attr: &str) -> Option<CfGuard> {
    let key = cf_str(attr);
    let mut val: CFTypeRef = ptr::null();
    let err = unsafe { AXUIElementCopyAttributeValue(el, key.0, &mut val) };
    if err == 0 && !val.is_null() {
        Some(CfGuard(val))
    } else {
        None
    }
}

fn copy_string_attr(el: AXUIElementRef, attr: &str) -> Option<String> {
    copy_attr(el, attr).and_then(|g| read_cf_string(g.0))
}

fn copy_bool_attr(el: AXUIElementRef, attr: &str, default: bool) -> bool {
    let Some(g) = copy_attr(el, attr) else {
        return default;
    };
    unsafe {
        if CFGetTypeID(g.0) == CFBooleanGetTypeID() {
            CFBooleanGetValue(g.0)
        } else {
            default
        }
    }
}

fn copy_bounds(el: AXUIElementRef) -> Option<Bounds> {
    let pos_val = copy_attr(el, "AXPosition")?;
    let size_val = copy_attr(el, "AXSize")?;
    let mut pt = CGPoint::default();
    let mut sz = CGSize::default();
    let ok_p = unsafe {
        AXValueGetValue(
            pos_val.0,
            K_AX_VALUE_CGPOINT_TYPE,
            (&mut pt as *mut CGPoint).cast(),
        )
    };
    let ok_s = unsafe {
        AXValueGetValue(
            size_val.0,
            K_AX_VALUE_CGSIZE_TYPE,
            (&mut sz as *mut CGSize).cast(),
        )
    };
    if !ok_p || !ok_s || sz.width <= 0.0 || sz.height <= 0.0 {
        return None;
    }
    Some(Bounds {
        x: pt.x as i32,
        y: pt.y as i32,
        width: sz.width as i32,
        height: sz.height as i32,
    })
}

fn copy_children(el: AXUIElementRef) -> Vec<CfGuard> {
    let Some(arr) = copy_attr(el, "AXChildren") else {
        return Vec::new();
    };
    unsafe {
        if CFGetTypeID(arr.0) != CFArrayGetTypeID() {
            return Vec::new();
        }
        let count = CFArrayGetCount(arr.0);
        let mut out = Vec::with_capacity(count.max(0) as usize);
        for i in 0..count {
            let item = CFArrayGetValueAtIndex(arr.0, i);
            if !item.is_null() {
                CFRetain(item);
                out.push(CfGuard(item));
            }
        }
        out
    }
}

/// Native macOS Accessibility (`AXUIElement`) provider.
pub struct MacOsProvider;

impl MacOsProvider {
    pub async fn connect() -> Result<Self> {
        if !unsafe { AXIsProcessTrusted() } {
            return Err(AxdomError::BusUnavailable(
                "macOS Accessibility permission denied; enable your terminal/IDE in System Settings -> Privacy & Security -> Accessibility".to_string(),
            ));
        }
        Ok(Self)
    }
}

fn all_desktop_windows() -> Result<Vec<(WindowInfo, CfGuard)>> {
    let sys = CfGuard(unsafe { AXUIElementCreateSystemWide() });
    let focused_pid = copy_attr(sys.0, "AXFocusedApplication").map(|app| {
        let mut p = 0i32;
        unsafe { AXUIElementGetPid(app.0, &mut p) };
        p
    });

    // Collect distinct PIDs of all on-screen windows via CGWindowListCopyWindowInfo.
    // kCGWindowListOptionOnScreenOnly = 1, kCGWindowListExcludeDesktopElements = 16
    let mut pids: Vec<i32> = Vec::new();
    if let Some(fp) = focused_pid
        && fp > 0
    {
        pids.push(fp);
    }
    let list_ptr = unsafe { CGWindowListCopyWindowInfo(1 | 16, 0) };
    if !list_ptr.is_null() {
        let list = CfGuard(list_ptr);
        let pid_key = cf_str("kCGWindowOwnerPID");
        let count = unsafe { CFArrayGetCount(list.0) };
        for i in 0..count {
            let dict = unsafe { CFArrayGetValueAtIndex(list.0, i) };
            if dict.is_null() {
                continue;
            }
            let num = unsafe { CFDictionaryGetValue(dict, pid_key.0) };
            if num.is_null() {
                continue;
            }
            let mut pid = 0i32;
            // kCFNumberSInt32Type = 3
            if unsafe { CFNumberGetValue(num, 3, (&mut pid as *mut i32).cast()) }
                && pid > 0
                && !pids.contains(&pid)
            {
                pids.push(pid);
            }
        }
    }

    let mut out = Vec::new();
    for pid in pids {
        let app = CfGuard(unsafe { AXUIElementCreateApplication(pid) });
        let app_name = copy_string_attr(app.0, "AXTitle").unwrap_or_else(|| format!("pid:{pid}"));
        let Some(arr) = copy_attr(app.0, "AXWindows") else {
            continue;
        };
        unsafe {
            if CFGetTypeID(arr.0) != CFArrayGetTypeID() {
                continue;
            }
            let count = CFArrayGetCount(arr.0);
            for idx in 0..count {
                let w = CFArrayGetValueAtIndex(arr.0, idx);
                if w.is_null() {
                    continue;
                }
                CFRetain(w);
                let guard = CfGuard(w);
                let title =
                    copy_string_attr(guard.0, "AXTitle").unwrap_or_else(|| app_name.clone());
                let is_active = Some(pid) == focused_pid && idx == 0;
                out.push((
                    WindowInfo {
                        id: format!("ax:{pid}:{idx}"),
                        title,
                        app_name: app_name.clone(),
                        pid: Some(pid as u32),
                        active: is_active,
                    },
                    guard,
                ));
            }
        }
    }
    Ok(out)
}

/// Watches foreground app/window changes on macOS to invalidate the daemon cache.
pub async fn watch_macos_events(tx: tokio::sync::mpsc::Sender<axdom_core::A11yEvent>) {
    let mut last_sig: Option<(i32, String)> = None;
    loop {
        let sig = tokio::task::spawn_blocking(|| {
            let sys = CfGuard(unsafe { AXUIElementCreateSystemWide() });
            let app = copy_attr(sys.0, "AXFocusedApplication")?;
            let mut pid = 0i32;
            unsafe { AXUIElementGetPid(app.0, &mut pid) };
            let title = copy_attr(app.0, "AXFocusedWindow")
                .and_then(|w| copy_string_attr(w.0, "AXTitle"))
                .unwrap_or_default();
            Some((pid, title))
        })
        .await
        .ok()
        .flatten();
        if let Some(cur) = sig
            && last_sig.as_ref() != Some(&cur)
        {
            last_sig = Some(cur.clone());
            let ev = axdom_core::A11yEvent {
                category: "window".to_string(),
                kind: "focus".to_string(),
                source_ref: format!("ax:{}:0", cur.0),
                detail: cur.1,
            };
            if tx.send(ev).await.is_err() {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
}

fn walk_ax(
    el: AXUIElementRef,
    path_ref: String,
    semantic_depth: usize,
    raw_depth: usize,
    max_depth: usize,
    max_nodes: usize,
    visited: &AtomicUsize,
    truncated: &AtomicBool,
) -> Option<RawNode> {
    if semantic_depth > max_depth || raw_depth > MAX_RAW_DEPTH {
        return None;
    }
    if visited.fetch_add(1, Ordering::Relaxed) >= max_nodes {
        truncated.store(true, Ordering::Relaxed);
        return None;
    }

    let role_str = copy_string_attr(el, "AXRole").unwrap_or_default();
    let subrole = copy_string_attr(el, "AXSubrole");
    let role = map_ax_role(&role_str, subrole.as_deref());
    let title = copy_string_attr(el, "AXTitle").unwrap_or_default();
    let desc = copy_string_attr(el, "AXDescription").filter(|s| !s.trim().is_empty());
    let name = if !title.trim().is_empty() {
        title
    } else {
        desc.clone().unwrap_or_default()
    };
    let value = copy_string_attr(el, "AXValue").filter(|s| !s.is_empty());
    let enabled = copy_bool_attr(el, "AXEnabled", true);
    let focused = copy_bool_attr(el, "AXFocused", false);
    let selected = copy_bool_attr(el, "AXSelected", false);

    let state = NodeState {
        enabled,
        visible: true,
        showing: true,
        sensitive: enabled,
        focusable: true,
        focused,
        checked: value.as_deref() == Some("1"),
        selected,
        editable: matches!(role, NodeRole::Input | NodeRole::PasswordInput),
        expanded: copy_bool_attr(el, "AXExpanded", false),
    };

    let next_semantic = if role == NodeRole::Container && name.trim().is_empty() {
        semantic_depth
    } else {
        semantic_depth + 1
    };

    let mut children = Vec::new();
    for (idx, child) in copy_children(el).into_iter().enumerate() {
        let child_ref = format!("{path_ref}/{idx}");
        if let Some(node) = walk_ax(
            child.0,
            child_ref,
            next_semantic,
            raw_depth + 1,
            max_depth,
            max_nodes,
            visited,
            truncated,
        ) {
            children.push(node);
        }
    }

    Some(RawNode {
        backend_ref: path_ref,
        role,
        name,
        description: desc,
        value,
        state,
        bounds: copy_bounds(el),
        children,
    })
}

fn resolve_ax_ref(backend_ref: &str) -> Result<CfGuard> {
    // Format: ax:<pid>:<win_idx>/<child_idx>/...
    let rest = backend_ref
        .strip_prefix("ax:")
        .ok_or_else(|| AxdomError::Backend(format!("invalid macOS backend_ref: {backend_ref}")))?;
    let (pid_str, path_str) = rest
        .split_once(':')
        .ok_or_else(|| AxdomError::Backend(format!("invalid macOS backend_ref: {backend_ref}")))?;
    let pid: i32 = pid_str
        .parse()
        .map_err(|_| AxdomError::Backend(format!("invalid pid in {backend_ref}")))?;

    let app = CfGuard(unsafe { AXUIElementCreateApplication(pid) });
    let mut parts = path_str.split('/');
    let win_idx: usize = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| AxdomError::NodeNotFound(backend_ref.to_string()))?;

    let wins_arr = copy_attr(app.0, "AXWindows")
        .ok_or_else(|| AxdomError::NodeNotFound(backend_ref.to_string()))?;
    let mut current = unsafe {
        if CFGetTypeID(wins_arr.0) != CFArrayGetTypeID()
            || (win_idx as isize) >= CFArrayGetCount(wins_arr.0)
        {
            return Err(AxdomError::NodeNotFound(backend_ref.to_string()));
        }
        let ptr = CFArrayGetValueAtIndex(wins_arr.0, win_idx as isize);
        CFRetain(ptr);
        CfGuard(ptr)
    };

    for seg in parts {
        let idx: usize = seg
            .parse()
            .map_err(|_| AxdomError::NodeNotFound(backend_ref.to_string()))?;
        let children = copy_children(current.0);
        current = children
            .into_iter()
            .nth(idx)
            .ok_or_else(|| AxdomError::NodeNotFound(backend_ref.to_string()))?;
    }
    Ok(current)
}

#[async_trait]
impl AccessibilityProvider for MacOsProvider {
    async fn doctor(&self) -> DoctorReport {
        let trusted = unsafe { AXIsProcessTrusted() };
        DoctorReport {
            checks: vec![DoctorCheck {
                name: "macos_accessibility".to_string(),
                status: if trusted {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Fail
                },
                detail: if trusted {
                    "AXIsProcessTrusted() == true".to_string()
                } else {
                    "AXIsProcessTrusted() == false".to_string()
                },
                hint: (!trusted).then(|| {
                    "Grant Accessibility permission in System Settings -> Privacy & Security -> Accessibility.".to_string()
                }),
            }],
        }
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        tokio::task::spawn_blocking(|| {
            Ok(all_desktop_windows()?
                .into_iter()
                .map(|(info, _)| info)
                .collect())
        })
        .await
        .map_err(|e| AxdomError::Backend(format!("join list_windows: {e}")))?
    }

    async fn get_window_tree(&self, query: &TreeQuery) -> Result<RawWindowTree> {
        let query = query.clone();
        tokio::task::spawn_blocking(move || {
            let wins = all_desktop_windows()?;
            if wins.is_empty() {
                return Err(AxdomError::NoActiveWindow);
            }
            let (win_info, win_guard) = match &query.window_filter {
                Some(filter) => {
                    let needle = filter.to_lowercase();
                    wins.into_iter()
                        .find(|(w, _)| {
                            w.id == *filter
                                || w.title.to_lowercase().contains(&needle)
                                || w.app_name.to_lowercase().contains(&needle)
                        })
                        .ok_or_else(|| AxdomError::WindowNotFound(filter.clone()))?
                }
                None => wins
                    .into_iter()
                    .find(|(w, _)| w.active)
                    .ok_or(AxdomError::NoActiveWindow)?,
            };

            let visited = AtomicUsize::new(0);
            let truncated = AtomicBool::new(false);
            let root = walk_ax(
                win_guard.0,
                win_info.id.clone(),
                0,
                0,
                query.max_depth,
                query.max_nodes,
                &visited,
                &truncated,
            )
            .ok_or_else(|| AxdomError::Backend("failed to inspect macOS window root".into()))?;

            Ok(RawWindowTree {
                window: win_info,
                root,
                total_raw_nodes: visited.load(Ordering::Relaxed),
                truncated: truncated.load(Ordering::Relaxed),
            })
        })
        .await
        .map_err(|e| AxdomError::Backend(format!("join get_window_tree: {e}")))?
    }

    async fn verify_node(&self, node: &NodeRef) -> Result<()> {
        let node = node.clone();
        tokio::task::spawn_blocking(move || {
            let el = resolve_ax_ref(&node.backend_ref)?;
            let role_str = copy_string_attr(el.0, "AXRole").unwrap_or_default();
            let subrole = copy_string_attr(el.0, "AXSubrole");
            let live_role = map_ax_role(&role_str, subrole.as_deref());
            let title = copy_string_attr(el.0, "AXTitle")
                .filter(|s| !s.trim().is_empty())
                .or_else(|| copy_string_attr(el.0, "AXDescription"))
                .unwrap_or_default();
            if live_role != node.role || (!node.name.is_empty() && title.trim() != node.name) {
                return Err(AxdomError::StaleNode {
                    id: node.id,
                    expected: format!("{} {:?}", node.role.compact_label(), node.name),
                    actual: format!("{} {:?}", live_role.compact_label(), title),
                });
            }
            Ok(())
        })
        .await
        .map_err(|e| AxdomError::Backend(format!("join verify_node: {e}")))?
    }

    async fn perform_action(
        &self,
        node: &NodeRef,
        action: &ActionRequest,
        verify: bool,
    ) -> Result<ActionResult> {
        if verify && !matches!(action, ActionRequest::ClickCoords { .. }) {
            self.verify_node(node).await?;
        }
        let node = node.clone();
        let action = action.clone();
        tokio::task::spawn_blocking(move || match action {
            ActionRequest::Click => {
                let el = resolve_ax_ref(&node.backend_ref)?;
                let act = cf_str("AXPress");
                let err = unsafe { AXUIElementPerformAction(el.0, act.0) };
                if err != 0 {
                    return Err(AxdomError::ActionUnsupported(format!(
                        "AXUIElementPerformAction(AXPress) returned {err}"
                    )));
                }
                Ok(ActionResult {
                    node_id: node.id,
                    action: "click".into(),
                    method_used: "macos:AXPress".into(),
                    success: true,
                    detail: None,
                })
            }
            ActionRequest::Focus => {
                let el = resolve_ax_ref(&node.backend_ref)?;
                let attr = cf_str("AXFocused");
                let err = unsafe { AXUIElementSetAttributeValue(el.0, attr.0, kCFBooleanTrue) };
                if err != 0 {
                    return Err(AxdomError::ActionFailed(format!("AXFocused error {err}")));
                }
                Ok(ActionResult {
                    node_id: node.id,
                    action: "focus".into(),
                    method_used: "macos:AXFocused".into(),
                    success: true,
                    detail: None,
                })
            }
            ActionRequest::SetText { text } => {
                let el = resolve_ax_ref(&node.backend_ref)?;
                let attr = cf_str("AXValue");
                let val = cf_str(&text);
                let err = unsafe { AXUIElementSetAttributeValue(el.0, attr.0, val.0) };
                if err != 0 {
                    return Err(AxdomError::ActionFailed(format!("AXValue error {err}")));
                }
                Ok(ActionResult {
                    node_id: node.id,
                    action: "set_text".into(),
                    method_used: "macos:AXValue".into(),
                    success: true,
                    detail: Some(format!("wrote {} chars", text.chars().count())),
                })
            }
            ActionRequest::SetValue { value } => {
                let el = resolve_ax_ref(&node.backend_ref)?;
                let attr = cf_str("AXValue");
                let val = cf_str(&value.to_string());
                let err = unsafe { AXUIElementSetAttributeValue(el.0, attr.0, val.0) };
                if err != 0 {
                    return Err(AxdomError::ActionFailed(format!("AXValue error {err}")));
                }
                Ok(ActionResult {
                    node_id: node.id,
                    action: "set_value".into(),
                    method_used: "macos:AXValue".into(),
                    success: true,
                    detail: Some(format!("value={value}")),
                })
            }
            ActionRequest::Scroll { delta_x, delta_y } => {
                unsafe {
                    // kCGScrollEventUnitLine = 1; negative scrolls down on macOS CGEvent.
                    let ev = CGEventCreateScrollWheelEvent(ptr::null(), 1, 2, -delta_y, -delta_x);
                    if !ev.is_null() {
                        CGEventPost(K_CG_HID_EVENT_TAP, ev);
                        CFRelease(ev.cast());
                    }
                }
                Ok(ActionResult {
                    node_id: node.id,
                    action: "scroll".into(),
                    method_used: "macos:CGEventScrollWheel".into(),
                    success: true,
                    detail: Some(format!("delta_x={delta_x}, delta_y={delta_y}")),
                })
            }
            ActionRequest::RightClick => {
                let b = node.bounds.ok_or_else(|| {
                    AxdomError::ActionUnsupported("node has no bounds for right_click".into())
                })?;
                let pt = CGPoint {
                    x: (b.x + b.width / 2) as f64,
                    y: (b.y + b.height / 2) as f64,
                };
                unsafe {
                    // kCGEventRightMouseDown = 3, kCGEventRightMouseUp = 4, kCGMouseButtonRight = 1
                    let down = CGEventCreateMouseEvent(ptr::null(), 3, pt, 1);
                    let up = CGEventCreateMouseEvent(ptr::null(), 4, pt, 1);
                    if !down.is_null() {
                        CGEventPost(K_CG_HID_EVENT_TAP, down);
                        CFRelease(down.cast());
                    }
                    if !up.is_null() {
                        CGEventPost(K_CG_HID_EVENT_TAP, up);
                        CFRelease(up.cast());
                    }
                }
                Ok(ActionResult {
                    node_id: node.id,
                    action: "right_click".into(),
                    method_used: "macos:CGEventRightClick".into(),
                    success: true,
                    detail: None,
                })
            }
            ActionRequest::DoubleClick => {
                let b = node.bounds.ok_or_else(|| {
                    AxdomError::ActionUnsupported("node has no bounds for double_click".into())
                })?;
                let pt = CGPoint {
                    x: (b.x + b.width / 2) as f64,
                    y: (b.y + b.height / 2) as f64,
                };
                unsafe {
                    for _ in 0..2 {
                        let down = CGEventCreateMouseEvent(ptr::null(), 1, pt, 0);
                        let up = CGEventCreateMouseEvent(ptr::null(), 2, pt, 0);
                        if !down.is_null() {
                            CGEventPost(K_CG_HID_EVENT_TAP, down);
                            CFRelease(down.cast());
                        }
                        if !up.is_null() {
                            CGEventPost(K_CG_HID_EVENT_TAP, up);
                            CFRelease(up.cast());
                        }
                    }
                }
                Ok(ActionResult {
                    node_id: node.id,
                    action: "double_click".into(),
                    method_used: "macos:CGEventDoubleClick".into(),
                    success: true,
                    detail: None,
                })
            }
            ActionRequest::Hover => {
                let b = node.bounds.ok_or_else(|| {
                    AxdomError::ActionUnsupported("node has no bounds for hover".into())
                })?;
                let pt = CGPoint {
                    x: (b.x + b.width / 2) as f64,
                    y: (b.y + b.height / 2) as f64,
                };
                unsafe {
                    // kCGEventMouseMoved = 5
                    let mv = CGEventCreateMouseEvent(ptr::null(), 5, pt, 0);
                    if !mv.is_null() {
                        CGEventPost(K_CG_HID_EVENT_TAP, mv);
                        CFRelease(mv.cast());
                    }
                }
                Ok(ActionResult {
                    node_id: node.id,
                    action: "hover".into(),
                    method_used: "macos:CGEventMouseMoved".into(),
                    success: true,
                    detail: None,
                })
            }
            ActionRequest::DragTo { to_x, to_y } => {
                let b = node.bounds.ok_or_else(|| {
                    AxdomError::ActionUnsupported("node has no bounds for drag_to".into())
                })?;
                let from = CGPoint {
                    x: (b.x + b.width / 2) as f64,
                    y: (b.y + b.height / 2) as f64,
                };
                let to = CGPoint {
                    x: to_x as f64,
                    y: to_y as f64,
                };
                unsafe {
                    // kCGEventLeftMouseDown = 1, kCGEventLeftMouseDragged = 6, kCGEventLeftMouseUp = 2
                    let down = CGEventCreateMouseEvent(ptr::null(), 1, from, 0);
                    let drag = CGEventCreateMouseEvent(ptr::null(), 6, to, 0);
                    let up = CGEventCreateMouseEvent(ptr::null(), 2, to, 0);
                    for ev in [down, drag, up] {
                        if !ev.is_null() {
                            CGEventPost(K_CG_HID_EVENT_TAP, ev);
                            CFRelease(ev.cast());
                        }
                    }
                }
                Ok(ActionResult {
                    node_id: node.id,
                    action: "drag_to".into(),
                    method_used: format!("macos:CGEventDrag(->{to_x},{to_y})"),
                    success: true,
                    detail: None,
                })
            }
            ActionRequest::ClickCoords { x, y } => {
                unsafe {
                    let pt = CGPoint {
                        x: x as f64,
                        y: y as f64,
                    };
                    // kCGEventLeftMouseDown = 1, kCGEventLeftMouseUp = 2
                    let down = CGEventCreateMouseEvent(ptr::null(), 1, pt, 0);
                    let up = CGEventCreateMouseEvent(ptr::null(), 2, pt, 0);
                    if !down.is_null() {
                        CGEventPost(K_CG_HID_EVENT_TAP, down);
                        CFRelease(down.cast());
                    }
                    if !up.is_null() {
                        CGEventPost(K_CG_HID_EVENT_TAP, up);
                        CFRelease(up.cast());
                    }
                }
                Ok(ActionResult {
                    node_id: node.id,
                    action: "click_coords".into(),
                    method_used: format!("macos:CGEventMouse({x},{y})"),
                    success: true,
                    detail: None,
                })
            }
        })
        .await
        .map_err(|e| AxdomError::Backend(format!("join perform_action: {e}")))?
    }

    async fn press_key(&self, combo: &KeyCombo) -> Result<ActionResult> {
        let code = cg_keycode_for_key(&combo.key).ok_or_else(|| {
            AxdomError::ActionFailed(format!("unsupported macOS keycode for {:?}", combo.key))
        })?;
        let flags = cg_flags_for_modifiers(&combo.modifiers);
        tokio::task::spawn_blocking(move || unsafe {
            let down = CGEventCreateKeyboardEvent(ptr::null(), code, true);
            let up = CGEventCreateKeyboardEvent(ptr::null(), code, false);
            if !down.is_null() {
                CGEventSetFlags(down, flags);
                CGEventPost(K_CG_HID_EVENT_TAP, down);
                CFRelease(down.cast());
            }
            if !up.is_null() {
                CGEventSetFlags(up, flags);
                CGEventPost(K_CG_HID_EVENT_TAP, up);
                CFRelease(up.cast());
            }
        })
        .await
        .map_err(|e| AxdomError::Backend(format!("join press_key: {e}")))?;

        Ok(ActionResult {
            node_id: "keyboard".into(),
            action: "press_key".into(),
            method_used: "macos:CGEventKeyboard".into(),
            success: true,
            detail: Some(format!("keycode={code}")),
        })
    }

    async fn capture_screenshot(&self, bounds: Option<Bounds>) -> Result<Screenshot> {
        let tmp = PathBuf::from(format!("/tmp/axdom-macos-{}.png", std::process::id()));
        let status = tokio::process::Command::new("screencapture")
            .args(["-x", &tmp.to_string_lossy()])
            .status()
            .await
            .map_err(|e| AxdomError::ActionFailed(format!("spawn screencapture: {e}")))?;
        if !status.success() {
            return Err(AxdomError::ActionFailed(format!(
                "screencapture exited with {status}"
            )));
        }
        let mut bytes = tokio::fs::read(&tmp)
            .await
            .map_err(|e| AxdomError::ActionFailed(format!("read screencapture: {e}")))?;
        let _ = tokio::fs::remove_file(&tmp).await;
        if let Some(b) = bounds {
            bytes = crop_png_bytes(&bytes, b)?;
        }
        let bytes_len = bytes.len();
        Ok(Screenshot {
            mime_type: "image/png".into(),
            base64_data: encode_base64(&bytes),
            method_used: "screenshot:macos-screencapture".into(),
            bytes_len,
        })
    }
}
