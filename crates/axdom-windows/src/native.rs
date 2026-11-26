#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use axdom_core::provider::RawWindowTree;
use axdom_core::{
    AccessibilityProvider, ActionRequest, ActionResult, AxdomError, Bounds, CheckStatus,
    DoctorCheck, DoctorReport, KeyCombo, NodeRef, NodeRole, NodeState, RawNode, Result, Screenshot,
    TreeQuery, WindowInfo, crop_png_bytes, encode_base64,
};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    IUIAutomationRangeValuePattern, IUIAutomationScrollPattern, IUIAutomationSelectionItemPattern,
    IUIAutomationTogglePattern, IUIAutomationTreeWalker, IUIAutomationValuePattern,
    ScrollAmount_LargeDecrement, ScrollAmount_LargeIncrement, ScrollAmount_NoAmount,
    TreeScope_Children, UIA_InvokePatternId, UIA_RangeValuePatternId, UIA_ScrollPatternId,
    UIA_SelectionItemPatternId, UIA_TogglePatternId, UIA_ValuePatternId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
};
use windows::core::BSTR;

use crate::mapping::{format_runtime_id, map_uia_control_type, vk_for_key, vk_for_modifier};

const MAX_RAW_DEPTH: usize = 48;

/// Native Windows UI Automation (UIA) accessibility provider.
pub struct WindowsProvider;

impl WindowsProvider {
    pub async fn connect() -> Result<Self> {
        tokio::task::spawn_blocking(|| init_uia().map(|_| ()))
            .await
            .map_err(|e| AxdomError::Backend(format!("join UIA init: {e}")))??;
        Ok(Self)
    }
}

fn init_uia() -> Result<IUIAutomation> {
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| AxdomError::BusUnavailable(format!("UIAutomation CoCreateInstance: {e}")))
    }
}

fn runtime_id_of(el: &IUIAutomationElement) -> String {
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    unsafe {
        let Ok(sa) = el.GetRuntimeId() else {
            return "uia:unknown".to_string();
        };
        if sa.is_null() {
            return "uia:unknown".to_string();
        }
        let lbound = windows::Win32::System::Ole::SafeArrayGetLBound(sa, 1).unwrap_or(0);
        let ubound = windows::Win32::System::Ole::SafeArrayGetUBound(sa, 1).unwrap_or(-1);
        let mut ids = Vec::new();
        for idx in lbound..=ubound {
            let mut val = 0i32;
            if windows::Win32::System::Ole::SafeArrayGetElement(
                sa,
                &idx,
                (&mut val as *mut i32).cast(),
            )
            .is_ok()
            {
                ids.push(val);
            }
        }
        let _ = windows::Win32::System::Ole::SafeArrayDestroy(sa);
        if ids.is_empty() {
            "uia:unknown".to_string()
        } else {
            format_runtime_id(&ids)
        }
    }
}

fn bounds_of(el: &IUIAutomationElement) -> Option<Bounds> {
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let rect: RECT = unsafe { el.CurrentBoundingRectangle().ok()? };
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(Bounds {
        x: rect.left,
        y: rect.top,
        width,
        height,
    })
}

fn list_windows_sync() -> Result<Vec<(WindowInfo, IUIAutomationElement)>> {
    let uia = init_uia()?;
    // SAFETY: Read-only Win32 user32 query with no pointer arguments.
    let fg_hwnd: HWND = unsafe { GetForegroundWindow() };
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let root = unsafe {
        uia.GetRootElement()
            .map_err(|e| AxdomError::Backend(format!("GetRootElement: {e}")))?
    };
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let cond = unsafe {
        uia.CreateTrueCondition()
            .map_err(|e| AxdomError::Backend(format!("CreateTrueCondition: {e}")))?
    };
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let array = unsafe {
        root.FindAll(TreeScope_Children, &cond)
            .map_err(|e| AxdomError::Backend(format!("FindAll windows: {e}")))?
    };
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let len = unsafe { array.Length().unwrap_or(0) };
    let mut windows = Vec::new();
    for i in 0..len {
        // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
        let Ok(el) = (unsafe { array.GetElement(i) }) else {
            continue;
        };
        // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
        let title = unsafe { el.CurrentName().map(|b| b.to_string()).unwrap_or_default() };
        if title.trim().is_empty() {
            continue;
        }
        // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
        let pid = unsafe { el.CurrentProcessId().ok().map(|p| p as u32) };
        // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
        let hwnd = unsafe {
            el.CurrentNativeWindowHandle()
                .ok()
                .unwrap_or(HWND::default())
        };
        let active = !fg_hwnd.is_invalid() && hwnd == fg_hwnd;
        // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
        let class_name = unsafe {
            el.CurrentClassName()
                .map(|b| b.to_string())
                .unwrap_or_else(|_| "window".to_string())
        };
        let info = WindowInfo {
            id: runtime_id_of(&el),
            title,
            app_name: class_name,
            pid,
            active,
        };
        windows.push((info, el));
    }
    Ok(windows)
}

fn walk_uia(
    walker: &IUIAutomationTreeWalker,
    el: &IUIAutomationElement,
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

    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let control_type = unsafe { el.CurrentControlType().map(|c| c.0).unwrap_or(0) };
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let is_password = unsafe { el.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(false) };
    let role = map_uia_control_type(control_type, is_password);
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let name = unsafe { el.CurrentName().map(|b| b.to_string()).unwrap_or_default() };
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let help = unsafe { el.CurrentHelpText().map(|b| b.to_string()).ok() }
        .filter(|s| !s.trim().is_empty());

    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let enabled = unsafe { el.CurrentIsEnabled().map(|b| b.as_bool()).unwrap_or(true) };
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let offscreen = unsafe {
        el.CurrentIsOffscreen()
            .map(|b| b.as_bool())
            .unwrap_or(false)
    };
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let focused = unsafe {
        el.CurrentHasKeyboardFocus()
            .map(|b| b.as_bool())
            .unwrap_or(false)
    };
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let focusable = unsafe {
        el.CurrentIsKeyboardFocusable()
            .map(|b| b.as_bool())
            .unwrap_or(false)
    };

    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let value = unsafe {
        el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            .ok()
            .and_then(|p| p.CurrentValue().ok())
            .map(|b| b.to_string())
            .filter(|s| !s.is_empty())
    };
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let checked = unsafe {
        el.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
            .ok()
            .and_then(|p| p.CurrentToggleState().ok())
            .map(|s| s.0 == 1)
            .unwrap_or(false)
    };
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let selected = unsafe {
        el.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
            .ok()
            .and_then(|p| p.CurrentIsSelected().ok())
            .map(|b| b.as_bool())
            .unwrap_or(false)
    };

    let state = NodeState {
        enabled,
        visible: !offscreen,
        showing: !offscreen,
        sensitive: enabled,
        focusable,
        focused,
        checked,
        selected,
        editable: matches!(role, NodeRole::Input | NodeRole::PasswordInput),
        expanded: false,
    };

    let next_semantic = if role == NodeRole::Container && name.trim().is_empty() {
        semantic_depth
    } else {
        semantic_depth + 1
    };

    let mut children = Vec::new();
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    if let Ok(mut child) = unsafe { walker.GetFirstChildElement(el) } {
        loop {
            if let Some(node) = walk_uia(
                walker,
                &child,
                next_semantic,
                raw_depth + 1,
                max_depth,
                max_nodes,
                visited,
                truncated,
            ) {
                children.push(node);
            }
            // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
            match unsafe { walker.GetNextSiblingElement(&child) } {
                Ok(next) => child = next,
                Err(_) => break,
            }
        }
    }

    Some(RawNode {
        backend_ref: runtime_id_of(el),
        role,
        name,
        description: help,
        value,
        state,
        bounds: bounds_of(el),
        children,
    })
}

fn find_element_by_ref(
    walker: &IUIAutomationTreeWalker,
    el: &IUIAutomationElement,
    target_ref: &str,
    depth: usize,
) -> Option<IUIAutomationElement> {
    if depth > MAX_RAW_DEPTH {
        return None;
    }
    if runtime_id_of(el) == target_ref {
        return Some(el.clone());
    }
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    if let Ok(mut child) = unsafe { walker.GetFirstChildElement(el) } {
        loop {
            if let Some(found) = find_element_by_ref(walker, &child, target_ref, depth + 1) {
                return Some(found);
            }
            // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
            match unsafe { walker.GetNextSiblingElement(&child) } {
                Ok(next) => child = next,
                Err(_) => break,
            }
        }
    }
    None
}

fn locate_live_element(target_ref: &str) -> Result<IUIAutomationElement> {
    let uia = init_uia()?;
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    let walker = unsafe {
        uia.ControlViewWalker()
            .map_err(|e| AxdomError::Backend(format!("ControlViewWalker: {e}")))?
    };
    for (_, win_el) in list_windows_sync()? {
        if let Some(found) = find_element_by_ref(&walker, &win_el, target_ref, 0) {
            return Ok(found);
        }
    }
    Err(AxdomError::NodeNotFound(target_ref.to_string()))
}

fn send_vk_combo(combo: &KeyCombo) -> Result<ActionResult> {
    let key_vk = vk_for_key(&combo.key).ok_or_else(|| {
        AxdomError::ActionFailed(format!("unsupported Win32 virtual key {:?}", combo.key))
    })?;
    let mod_vks: Vec<u16> = combo
        .modifiers
        .iter()
        .map(|m| vk_for_modifier(*m))
        .collect();

    let mut inputs = Vec::with_capacity(mod_vks.len() * 2 + 2);
    let make_key = |vk: u16, key_up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if key_up {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    for &m in &mod_vks {
        inputs.push(make_key(m, false));
    }
    inputs.push(make_key(key_vk, false));
    inputs.push(make_key(key_vk, true));
    for &m in mod_vks.iter().rev() {
        inputs.push(make_key(m, true));
    }

    // SAFETY: Win32 SendInput receives a valid contiguous slice of initialized INPUT structs with exact struct size.
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent == 0 {
        return Err(AxdomError::ActionFailed("Win32 SendInput failed".into()));
    }
    Ok(ActionResult {
        node_id: "keyboard".to_string(),
        action: "press_key".to_string(),
        method_used: "win32:SendInput".to_string(),
        success: true,
        detail: Some(format!("vk=0x{key_vk:x}")),
    })
}

fn send_mouse_click(node_id: &str, x: i32, y: i32) -> Result<ActionResult> {
    // SAFETY: Read-only Win32 user32 query with no pointer arguments.
    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1);
    // SAFETY: Read-only Win32 user32 query with no pointer arguments.
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1);
    let abs_x = (x * 65535) / screen_w;
    let abs_y = (y * 65535) / screen_h;
    let inputs = [
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: abs_x,
                    dy: abs_y,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: abs_x,
                    dy: abs_y,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_LEFTDOWN | MOUSEEVENTF_LEFTUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];
    // SAFETY: Win32 SendInput receives a valid contiguous slice of initialized INPUT structs with exact struct size.
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent == 0 {
        return Err(AxdomError::ActionFailed(
            "Win32 mouse SendInput failed".into(),
        ));
    }
    Ok(ActionResult {
        node_id: node_id.to_string(),
        action: "click_coords".to_string(),
        method_used: format!("win32:SendInput({x},{y})"),
        success: true,
        detail: None,
    })
}

fn send_mouse_wheel(node_id: &str, delta_x: i32, delta_y: i32) -> Result<ActionResult> {
    let mut inputs = Vec::new();
    if delta_y != 0 {
        let wheel = (-delta_y).saturating_mul(120) as u32;
        inputs.push(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: wheel,
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }
    if delta_x != 0 {
        let wheel = delta_x.saturating_mul(120) as u32;
        inputs.push(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: wheel,
                    dwFlags: MOUSEEVENTF_HWHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }
    if !inputs.is_empty() {
        // SAFETY: Win32 SendInput receives a valid contiguous slice of initialized INPUT structs with exact struct size.
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent == 0 {
            return Err(AxdomError::ActionFailed(
                "Win32 wheel SendInput failed".into(),
            ));
        }
    }
    Ok(ActionResult {
        node_id: node_id.to_string(),
        action: "scroll".to_string(),
        method_used: "win32:SendInput(WHEEL)".to_string(),
        success: true,
        detail: Some(format!("delta_x={delta_x}, delta_y={delta_y}")),
    })
}

fn send_mouse_button_action(
    node_id: &str,
    x: i32,
    y: i32,
    right: bool,
    count: u32,
    action: &str,
) -> Result<ActionResult> {
    // SAFETY: Read-only Win32 user32 query with no pointer arguments.
    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1);
    // SAFETY: Read-only Win32 user32 query with no pointer arguments.
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1);
    let abs_x = (x * 65535) / screen_w;
    let abs_y = (y * 65535) / screen_h;
    let flags = if right {
        MOUSEEVENTF_RIGHTDOWN | MOUSEEVENTF_RIGHTUP
    } else {
        MOUSEEVENTF_LEFTDOWN | MOUSEEVENTF_LEFTUP
    };
    let mut inputs = vec![INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: abs_x,
                dy: abs_y,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }];
    for _ in 0..count {
        inputs.push(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: abs_x,
                    dy: abs_y,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }
    // SAFETY: Win32 SendInput receives a valid contiguous slice of initialized INPUT structs with exact struct size.
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent == 0 {
        return Err(AxdomError::ActionFailed(format!(
            "Win32 {action} SendInput failed"
        )));
    }
    Ok(ActionResult {
        node_id: node_id.to_string(),
        action: action.to_string(),
        method_used: format!("win32:SendInput({x},{y})"),
        success: true,
        detail: None,
    })
}

fn send_mouse_drag(
    node_id: &str,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
) -> Result<ActionResult> {
    // SAFETY: Read-only Win32 user32 query with no pointer arguments.
    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1);
    // SAFETY: Read-only Win32 user32 query with no pointer arguments.
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1);
    let to_abs = |x: i32, y: i32| ((x * 65535) / screen_w, (y * 65535) / screen_h);
    let (fx, fy) = to_abs(from_x, from_y);
    let (tx, ty) = to_abs(to_x, to_y);
    let inputs = [
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: fx,
                    dy: fy,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: fx,
                    dy: fy,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_LEFTDOWN,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: tx,
                    dy: ty,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: tx,
                    dy: ty,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_LEFTUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];
    // SAFETY: Win32 SendInput receives a valid contiguous slice of initialized INPUT structs with exact struct size.
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent == 0 {
        return Err(AxdomError::ActionFailed(
            "Win32 drag SendInput failed".into(),
        ));
    }
    Ok(ActionResult {
        node_id: node_id.to_string(),
        action: "drag_to".to_string(),
        method_used: format!("win32:SendInput({from_x},{from_y}->{to_x},{to_y})"),
        success: true,
        detail: None,
    })
}

/// Watches foreground window & UIA focus changes on Windows to invalidate the daemon cache.
pub async fn watch_windows_events(tx: tokio::sync::mpsc::Sender<axdom_core::A11yEvent>) {
    let mut last_sig: Option<(isize, String)> = None;
    loop {
        let sig = tokio::task::spawn_blocking(|| {
            // SAFETY: Read-only Win32 user32 query with no pointer arguments.
            let hwnd = unsafe { GetForegroundWindow() }.0 as isize;
            let focus_id = init_uia()
                .ok()
                // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
                .and_then(|u| unsafe { u.GetFocusedElement().ok() })
                .map(|el| runtime_id_of(&el))
                .unwrap_or_default();
            (hwnd, focus_id)
        })
        .await
        .ok();
        if let Some(cur) = sig
            && last_sig.as_ref() != Some(&cur)
        {
            last_sig = Some(cur.clone());
            let ev = axdom_core::A11yEvent {
                category: "focus".to_string(),
                kind: "changed".to_string(),
                source_ref: cur.1,
                detail: format!("hwnd={}", cur.0),
            };
            if tx.send(ev).await.is_err() {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
}

fn capture_gdi_png(bounds: Option<Bounds>) -> Result<Screenshot> {
    // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
    unsafe {
        let width = GetSystemMetrics(SM_CXSCREEN);
        let height = GetSystemMetrics(SM_CYSCREEN);
        if width <= 0 || height <= 0 {
            return Err(AxdomError::ActionFailed(
                "invalid Windows screen dimensions".into(),
            ));
        }
        let hdc_screen = GetDC(HWND::default());
        let hdc_mem = CreateCompatibleDC(hdc_screen);
        let hbm = CreateCompatibleBitmap(hdc_screen, width, height);
        let old = SelectObject(hdc_mem, hbm);
        let _ = BitBlt(hdc_mem, 0, 0, width, height, hdc_screen, 0, 0, SRCCOPY);

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down DIB
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bgra = vec![0u8; (width * height * 4) as usize];
        GetDIBits(
            hdc_mem,
            hbm,
            0,
            height as u32,
            Some(bgra.as_mut_ptr().cast()),
            &mut bmi,
            DIB_RGB_COLORS,
        );

        SelectObject(hdc_mem, old);
        let _ = DeleteObject(hbm);
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(HWND::default(), hdc_screen);

        // Convert BGRA -> RGBA.
        for chunk in bgra.as_chunks_mut::<4>().0 {
            chunk.swap(0, 2);
            chunk[3] = 255;
        }

        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, width as u32, height as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc
                .write_header()
                .map_err(|e| AxdomError::ActionFailed(format!("encode PNG header: {e}")))?;
            writer
                .write_image_data(&bgra)
                .map_err(|e| AxdomError::ActionFailed(format!("encode PNG data: {e}")))?;
        }
        if let Some(b) = bounds {
            png_bytes = crop_png_bytes(&png_bytes, b)?;
        }
        let bytes_len = png_bytes.len();
        Ok(Screenshot {
            mime_type: "image/png".to_string(),
            base64_data: encode_base64(&png_bytes),
            method_used: "screenshot:win32-gdi".to_string(),
            bytes_len,
        })
    }
}

#[async_trait]
impl AccessibilityProvider for WindowsProvider {
    async fn doctor(&self) -> DoctorReport {
        let check = match tokio::task::spawn_blocking(|| list_windows_sync().map(|w| w.len())).await
        {
            Ok(Ok(count)) => DoctorCheck {
                name: "windows_uia".to_string(),
                status: CheckStatus::Ok,
                detail: format!("UI Automation active ({count} top-level windows)"),
                hint: None,
            },
            Ok(Err(e)) => DoctorCheck {
                name: "windows_uia".to_string(),
                status: CheckStatus::Fail,
                detail: e.to_string(),
                hint: Some("Ensure Windows UI Automation service is available.".to_string()),
            },
            Err(e) => DoctorCheck {
                name: "windows_uia".to_string(),
                status: CheckStatus::Fail,
                detail: e.to_string(),
                hint: None,
            },
        };
        DoctorReport {
            checks: vec![check],
        }
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        tokio::task::spawn_blocking(|| {
            Ok(list_windows_sync()?
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
            let uia = init_uia()?;
            // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
            let walker = unsafe {
                uia.ControlViewWalker()
                    .map_err(|e| AxdomError::Backend(format!("ControlViewWalker: {e}")))?
            };
            let windows = list_windows_sync()?;
            let (win_info, win_el) = match &query.window_filter {
                Some(filter) => {
                    let needle = filter.to_lowercase();
                    windows
                        .into_iter()
                        .find(|(w, _)| {
                            w.id == *filter
                                || w.title.to_lowercase().contains(&needle)
                                || w.app_name.to_lowercase().contains(&needle)
                        })
                        .ok_or_else(|| AxdomError::WindowNotFound(filter.clone()))?
                }
                None => windows
                    .into_iter()
                    .find(|(w, _)| w.active)
                    .ok_or(AxdomError::NoActiveWindow)?,
            };

            let visited = AtomicUsize::new(0);
            let truncated = AtomicBool::new(false);
            let root = walk_uia(
                &walker,
                &win_el,
                0,
                0,
                query.max_depth,
                query.max_nodes,
                &visited,
                &truncated,
            )
            .ok_or_else(|| AxdomError::Backend("failed to walk UIA window root".to_string()))?;

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
            let el = locate_live_element(&node.backend_ref)?;
            // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
            let ct = unsafe { el.CurrentControlType().map(|c| c.0).unwrap_or(0) };
            // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
            let is_pw = unsafe { el.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(false) };
            let live_role = map_uia_control_type(ct, is_pw);
            // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
            let live_name = unsafe { el.CurrentName().map(|b| b.to_string()).unwrap_or_default() };
            if live_role != node.role || (!node.name.is_empty() && live_name.trim() != node.name) {
                return Err(AxdomError::StaleNode {
                    id: node.id,
                    expected: format!("{} {:?}", node.role.compact_label(), node.name),
                    actual: format!("{} {:?}", live_role.compact_label(), live_name),
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
            ActionRequest::ClickCoords { x, y } => send_mouse_click(&node.id, x, y),
            ActionRequest::RightClick => {
                let b = node.bounds.ok_or_else(|| {
                    AxdomError::ActionUnsupported("node has no bounds for right_click".into())
                })?;
                send_mouse_button_action(
                    &node.id,
                    b.x + b.width / 2,
                    b.y + b.height / 2,
                    true,
                    1,
                    "right_click",
                )
            }
            ActionRequest::DoubleClick => {
                let b = node.bounds.ok_or_else(|| {
                    AxdomError::ActionUnsupported("node has no bounds for double_click".into())
                })?;
                send_mouse_button_action(
                    &node.id,
                    b.x + b.width / 2,
                    b.y + b.height / 2,
                    false,
                    2,
                    "double_click",
                )
            }
            ActionRequest::Hover => {
                let b = node.bounds.ok_or_else(|| {
                    AxdomError::ActionUnsupported("node has no bounds for hover".into())
                })?;
                send_mouse_button_action(
                    &node.id,
                    b.x + b.width / 2,
                    b.y + b.height / 2,
                    false,
                    0,
                    "hover",
                )
            }
            ActionRequest::DragTo { to_x, to_y } => {
                let b = node.bounds.ok_or_else(|| {
                    AxdomError::ActionUnsupported("node has no bounds for drag_to".into())
                })?;
                send_mouse_drag(&node.id, b.x + b.width / 2, b.y + b.height / 2, to_x, to_y)
            }
            ActionRequest::Focus => {
                let el = locate_live_element(&node.backend_ref)?;
                // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
                unsafe {
                    el.SetFocus()
                        .map_err(|e| AxdomError::ActionFailed(format!("UIA.SetFocus: {e}")))?;
                }
                Ok(ActionResult {
                    node_id: node.id,
                    action: "focus".into(),
                    method_used: "uia:SetFocus".into(),
                    success: true,
                    detail: None,
                })
            }
            ActionRequest::Click => {
                let el = locate_live_element(&node.backend_ref)?;
                // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
                unsafe {
                    if let Ok(inv) =
                        el.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                    {
                        inv.Invoke()
                            .map_err(|e| AxdomError::ActionFailed(format!("UIA.Invoke: {e}")))?;
                        return Ok(ActionResult {
                            node_id: node.id,
                            action: "click".into(),
                            method_used: "uia:InvokePattern.Invoke".into(),
                            success: true,
                            detail: None,
                        });
                    }
                    if let Ok(tog) =
                        el.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                    {
                        tog.Toggle()
                            .map_err(|e| AxdomError::ActionFailed(format!("UIA.Toggle: {e}")))?;
                        return Ok(ActionResult {
                            node_id: node.id,
                            action: "click".into(),
                            method_used: "uia:TogglePattern.Toggle".into(),
                            success: true,
                            detail: None,
                        });
                    }
                    if let Ok(sel) = el.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                        UIA_SelectionItemPatternId,
                    ) {
                        sel.Select()
                            .map_err(|e| AxdomError::ActionFailed(format!("UIA.Select: {e}")))?;
                        return Ok(ActionResult {
                            node_id: node.id,
                            action: "click".into(),
                            method_used: "uia:SelectionItemPattern.Select".into(),
                            success: true,
                            detail: None,
                        });
                    }
                }
                Err(AxdomError::ActionUnsupported(format!(
                    "node {} exposes no UIA Invoke/Toggle/SelectionItem pattern",
                    node.id
                )))
            }
            ActionRequest::SetText { text } => {
                let el = locate_live_element(&node.backend_ref)?;
                // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
                unsafe {
                    let _ = el.SetFocus();
                    let val = el
                        .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                        .map_err(|e| {
                            AxdomError::ActionUnsupported(format!("UIA.ValuePattern: {e}"))
                        })?;
                    val.SetValue(&BSTR::from(&text)).map_err(|e| {
                        AxdomError::ActionFailed(format!("UIA.ValuePattern.SetValue: {e}"))
                    })?;
                }
                Ok(ActionResult {
                    node_id: node.id,
                    action: "set_text".into(),
                    method_used: "uia:ValuePattern.SetValue".into(),
                    success: true,
                    detail: Some(format!("wrote {} chars", text.chars().count())),
                })
            }
            ActionRequest::SetValue { value } => {
                let el = locate_live_element(&node.backend_ref)?;
                // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
                unsafe {
                    let rv = el
                        .GetCurrentPatternAs::<IUIAutomationRangeValuePattern>(
                            UIA_RangeValuePatternId,
                        )
                        .map_err(|e| {
                            AxdomError::ActionUnsupported(format!("UIA.RangeValuePattern: {e}"))
                        })?;
                    rv.SetValue(value).map_err(|e| {
                        AxdomError::ActionFailed(format!("UIA.RangeValuePattern.SetValue: {e}"))
                    })?;
                }
                Ok(ActionResult {
                    node_id: node.id,
                    action: "set_value".into(),
                    method_used: "uia:RangeValuePattern.SetValue".into(),
                    success: true,
                    detail: Some(format!("value={value}")),
                })
            }
            ActionRequest::Scroll { delta_x, delta_y } => {
                let el = locate_live_element(&node.backend_ref)?;
                // SAFETY: COM/Win32 UIA call operates on valid COM interfaces initialized on this thread via CoInitializeEx.
                unsafe {
                    let _ = el.SetFocus();
                    if delta_x == 0
                        && let Ok(sc) = el
                            .GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
                    {
                        let amount = if delta_y >= 0 {
                            ScrollAmount_LargeIncrement
                        } else {
                            ScrollAmount_LargeDecrement
                        };
                        if sc.Scroll(ScrollAmount_NoAmount, amount).is_ok() {
                            return Ok(ActionResult {
                                node_id: node.id,
                                action: "scroll".into(),
                                method_used: "uia:ScrollPattern.Scroll".into(),
                                success: true,
                                detail: Some(format!("delta_x={delta_x}, delta_y={delta_y}")),
                            });
                        }
                    }
                }
                send_mouse_wheel(&node.id, delta_x, delta_y)
            }
        })
        .await
        .map_err(|e| AxdomError::Backend(format!("join perform_action: {e}")))?
    }

    async fn press_key(&self, combo: &KeyCombo) -> Result<ActionResult> {
        let combo = combo.clone();
        tokio::task::spawn_blocking(move || send_vk_combo(&combo))
            .await
            .map_err(|e| AxdomError::Backend(format!("join press_key: {e}")))?
    }

    async fn capture_screenshot(&self, bounds: Option<Bounds>) -> Result<Screenshot> {
        tokio::task::spawn_blocking(move || capture_gdi_png(bounds))
            .await
            .map_err(|e| AxdomError::Backend(format!("join capture_screenshot: {e}")))?
    }
}
