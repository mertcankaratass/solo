//! Windows Taskbar Notification Area (System Tray) status item.
//!
//! On Windows, menu-bar applications live in the Taskbar Notification Area
//! (System Tray) via Win32 `Shell_NotifyIconW`. Left-clicking or right-clicking
//! displays a context menu containing application-defined items followed by
//! the standard Open and Quit items. Left-clicking also emits
//! `applicationReopen`, matching macOS Dock clicks and Linux status-item clicks.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use napi::bindgen_prelude::{Error, Result};
use parking_lot::Mutex;

use super::{
    parse_update, MenuBarItem, APPLICATION_REOPEN_EVENT, MENU_BAR_ACTION_EVENT,
};
use crate::renderer::EventCallback;

type HWND = isize;
type HMENU = isize;
type HICON = isize;
type HBITMAP = isize;
type HINSTANCE = isize;
type BOOL = i32;

const TRUE: BOOL = 1;

const WS_POPUP: u32 = 0x80000000;
const GWLP_USERDATA: i32 = -21;

const WM_NULL: u32 = 0x0000;
const WM_NCCREATE: u32 = 0x0081;
const WM_NCDESTROY: u32 = 0x0082;
const WM_CONTEXTMENU: u32 = 0x007B;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_APP: u32 = 0x8000;
const WM_TRAY_CALLBACK: u32 = WM_APP + 1;

const NIM_ADD: u32 = 0x00000000;
const NIM_MODIFY: u32 = 0x00000001;
const NIM_DELETE: u32 = 0x00000002;

const NIF_MESSAGE: u32 = 0x00000001;
const NIF_ICON: u32 = 0x00000002;
const NIF_TIP: u32 = 0x00000004;

const MF_STRING: u32 = 0x00000000;
const MF_GRAYED: u32 = 0x00000001;
const MF_SEPARATOR: u32 = 0x00000800;
const MF_CHECKED: u32 = 0x00000008;

const TPM_RIGHTBUTTON: u32 = 0x0002;
const TPM_RETURNCMD: u32 = 0x0100;

const IMAGE_ICON: u32 = 1;
const LR_LOADFROMFILE: u32 = 0x0010;
const LR_DEFAULTSIZE: u32 = 0x0040;

#[repr(C)]
struct POINT {
    x: i32,
    y: i32,
}

#[repr(C)]
struct WNDCLASSEXW {
    cbSize: u32,
    style: u32,
    lpfnWndProc: Option<unsafe extern "system" fn(HWND, u32, usize, isize) -> isize>,
    cbClsExtra: i32,
    cbWndExtra: i32,
    hInstance: HINSTANCE,
    hIcon: HICON,
    hCursor: isize,
    hbrBackground: isize,
    lpszMenuName: *const u16,
    lpszClassName: *const u16,
    hIconSm: HICON,
}

#[repr(C)]
struct CREATESTRUCTW {
    lpCreateParams: *mut std::ffi::c_void,
    hInstance: HINSTANCE,
    hMenu: HMENU,
    hwndParent: HWND,
    cy: i32,
    cx: i32,
    y: i32,
    x: i32,
    style: i32,
    lpszName: *const u16,
    lpszClass: *const u16,
    dwExStyle: u32,
}

#[repr(C)]
struct ICONINFO {
    fIcon: BOOL,
    xHotspot: u32,
    yHotspot: u32,
    hbmMask: HBITMAP,
    hbmColor: HBITMAP,
}

#[repr(C)]
struct NOTIFYICONDATAW {
    cbSize: u32,
    hWnd: HWND,
    uID: u32,
    uFlags: u32,
    uCallbackMessage: u32,
    hIcon: HICON,
    szTip: [u16; 128],
    dwState: u32,
    dwStateMask: u32,
    szInfo: [u16; 256],
    uTimeoutOrVersion: u32,
    szInfoTitle: [u16; 64],
    dwInfoFlags: u32,
    guidItem: [u8; 16],
    hBalloonIcon: HICON,
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassExW(lpWndClass: *const WNDCLASSEXW) -> u16;
    fn CreateWindowExW(
        dwExStyle: u32,
        lpClassName: *const u16,
        lpWindowName: *const u16,
        dwStyle: u32,
        X: i32,
        Y: i32,
        nWidth: i32,
        nHeight: i32,
        hWndParent: HWND,
        hMenu: HMENU,
        hInstance: HINSTANCE,
        lpParam: *mut std::ffi::c_void,
    ) -> HWND;
    fn DestroyWindow(hWnd: HWND) -> BOOL;
    fn DefWindowProcW(hWnd: HWND, Msg: u32, wParam: usize, lParam: isize) -> isize;
    fn SetWindowLongPtrW(hWnd: HWND, nIndex: i32, dwNewLong: isize) -> isize;
    fn GetWindowLongPtrW(hWnd: HWND, nIndex: i32) -> isize;
    fn PostMessageW(hWnd: HWND, Msg: u32, wParam: usize, lParam: isize) -> BOOL;
    fn SetForegroundWindow(hWnd: HWND) -> BOOL;
    fn GetCursorPos(lpPoint: *mut POINT) -> BOOL;
    fn RegisterWindowMessageW(lpString: *const u16) -> u32;

    fn CreatePopupMenu() -> HMENU;
    fn AppendMenuW(hMenu: HMENU, uFlags: u32, uIDNewItem: usize, lpNewItem: *const u16) -> BOOL;
    fn TrackPopupMenuEx(
        hMenu: HMENU,
        uFlags: u32,
        x: i32,
        y: i32,
        hWnd: HWND,
        lptpm: *const std::ffi::c_void,
    ) -> BOOL;
    fn DestroyMenu(hMenu: HMENU) -> BOOL;

    fn CreateIconIndirect(piconinfo: *mut ICONINFO) -> HICON;
    fn DestroyIcon(hIcon: HICON) -> BOOL;
    fn LoadImageW(
        hInst: HINSTANCE,
        name: *const u16,
        r#type: u32,
        cx: i32,
        cy: i32,
        fuLoad: u32,
    ) -> HICON;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateBitmap(
        nWidth: i32,
        nHeight: i32,
        nPlanes: u32,
        nBitCount: u32,
        lpBits: *const std::ffi::c_void,
    ) -> HBITMAP;
    fn DeleteObject(ho: isize) -> BOOL;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(lpModuleName: *const u16) -> HINSTANCE;
}

#[link(name = "shell32")]
extern "system" {
    fn Shell_NotifyIconW(dwMessage: u32, lpData: *const NOTIFYICONDATAW) -> BOOL;
}

/// What choosing a row does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TrayClick {
    /// A menu item the app defined; its token goes to `menuBarAction`.
    Token(String),
    /// Solo's built-in item: bring the main window back.
    Open,
    /// Solo's built-in item: end the application.
    Quit,
}

/// One row of the item's menu, in painting order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TrayRow {
    Action {
        token: String,
        label: String,
        enabled: bool,
        checked: bool,
    },
    Separator,
    /// Non-actionable text (`type: "status"`).
    Status {
        label: String,
    },
    Builtin {
        click: TrayClick,
        label: String,
    },
}

/// The app's items, then Solo's built-in Open and Quit — the same order as the
/// macOS and Linux menus.
pub(super) fn rows(items: &[MenuBarItem], application_title: &str) -> Vec<TrayRow> {
    let mut rows: Vec<TrayRow> = items
        .iter()
        .map(|item| match item {
            MenuBarItem::Action {
                token,
                label,
                enabled,
                checked,
            } => TrayRow::Action {
                token: token.clone(),
                label: label.clone(),
                enabled: *enabled,
                checked: *checked,
            },
            MenuBarItem::Separator => TrayRow::Separator,
            MenuBarItem::Status { label } => TrayRow::Status {
                label: label.clone(),
            },
        })
        .collect();
    if !rows.is_empty() {
        rows.push(TrayRow::Separator);
    }
    rows.push(TrayRow::Builtin {
        click: TrayClick::Open,
        label: format!("Open {application_title}"),
    });
    rows.push(TrayRow::Separator);
    rows.push(TrayRow::Builtin {
        click: TrayClick::Quit,
        label: format!("Quit {application_title}"),
    });
    rows
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn set_tooltip_field(tip_buf: &mut [u16; 128], tooltip: Option<&str>) {
    tip_buf.fill(0);
    if let Some(t) = tooltip {
        let wide: Vec<u16> = t.encode_utf16().collect();
        let len = wide.len().min(127);
        tip_buf[..len].copy_from_slice(&wide[..len]);
    }
}

fn load_hicon(path: &str) -> Result<HICON> {
    if path.to_ascii_lowercase().ends_with(".ico") {
        let wide = to_wide_null(path);
        let hicon = unsafe {
            LoadImageW(
                0,
                wide.as_ptr(),
                IMAGE_ICON,
                0,
                0,
                LR_LOADFROMFILE | LR_DEFAULTSIZE,
            )
        };
        if hicon != 0 {
            return Ok(hicon);
        }
    }

    let bytes = std::fs::read(path).map_err(|error| {
        Error::from_reason(format!("Failed to read menu-bar icon {path:?}: {error}"))
    })?;
    let img = image::load_from_memory(&bytes).map_err(|error| {
        Error::from_reason(format!(
            "Menu-bar icon {path:?} is not a supported image: {error}"
        ))
    })?;
    let rgba = img.to_rgba8();
    let width = rgba.width() as i32;
    let height = rgba.height() as i32;

    let mut bgra = rgba.into_raw();
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    let hbm_color = unsafe {
        CreateBitmap(
            width,
            height,
            1,
            32,
            bgra.as_ptr() as *const std::ffi::c_void,
        )
    };
    if hbm_color == 0 {
        return Err(Error::from_reason(
            "Failed to create color bitmap for menu-bar icon",
        ));
    }
    let hbm_mask = unsafe { CreateBitmap(width, height, 1, 1, std::ptr::null()) };
    if hbm_mask == 0 {
        unsafe { DeleteObject(hbm_color) };
        return Err(Error::from_reason(
            "Failed to create mask bitmap for menu-bar icon",
        ));
    }

    let mut info = ICONINFO {
        fIcon: TRUE,
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: hbm_mask,
        hbmColor: hbm_color,
    };
    let icon = unsafe { CreateIconIndirect(&mut info) };
    unsafe {
        DeleteObject(hbm_color);
        DeleteObject(hbm_mask);
    }
    if icon == 0 {
        return Err(Error::from_reason(
            "Failed to create HICON from menu-bar image",
        ));
    }
    Ok(icon)
}

struct TrayState {
    hwnd: HWND,
    hicon: HICON,
    application_title: String,
    initial_icon_path: String,
    current_icon_path: String,
    initial_tooltip: Option<String>,
    current_tooltip: Option<String>,
    items: Vec<MenuBarItem>,
    callback: Option<EventCallback>,
    taskbar_created_msg: u32,
}

impl TrayState {
    fn add_tray_icon(&self) {
        let mut nid = unsafe { std::mem::zeroed::<NOTIFYICONDATAW>() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY_CALLBACK;
        nid.hIcon = self.hicon;
        set_tooltip_field(&mut nid.szTip, self.current_tooltip.as_deref());
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &nid);
        }
    }

    fn update_tray_icon(&self) {
        let mut nid = unsafe { std::mem::zeroed::<NOTIFYICONDATAW>() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_TIP;
        nid.hIcon = self.hicon;
        set_tooltip_field(&mut nid.szTip, self.current_tooltip.as_deref());
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn delete_tray_icon(&self) {
        let mut nid = unsafe { std::mem::zeroed::<NOTIFYICONDATAW>() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd;
        nid.uID = 1;
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }
}

static WINDOW_CLASS_COUNTER: AtomicU32 = AtomicU32::new(0);

fn create_tray_window(state: Arc<Mutex<TrayState>>) -> Result<HWND> {
    let id = WINDOW_CLASS_COUNTER.fetch_add(1, Ordering::Relaxed);
    let class_name_str = format!("SoloMenuBarMessageWindow_{}_{}", std::process::id(), id);
    let class_name = to_wide_null(&class_name_str);

    let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let wnd_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: 0,
        hCursor: 0,
        hbrBackground: 0,
        lpszMenuName: std::ptr::null(),
        lpszClassName: class_name.as_ptr(),
        hIconSm: 0,
    };
    unsafe {
        RegisterClassExW(&wnd_class);
    }

    let state_box = Box::into_raw(Box::new(state));
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            0,
            0,
            hinstance,
            state_box as *mut std::ffi::c_void,
        )
    };

    if hwnd == 0 {
        unsafe {
            let _ = Box::from_raw(state_box);
        }
        return Err(Error::from_reason("Failed to create tray message window"));
    }

    Ok(hwnd)
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    if msg == WM_NCCREATE {
        let cs = lparam as *const CREATESTRUCTW;
        if !cs.is_null() {
            let state_box = (*cs).lpCreateParams;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_box as isize);
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Arc<Mutex<TrayState>>;
    if !state_ptr.is_null() {
        let state_arc = &*state_ptr;
        if msg == WM_TRAY_CALLBACK {
            let mouse_msg = lparam as u32;
            if mouse_msg == WM_LBUTTONUP {
                let cb = {
                    let guard = state_arc.lock();
                    guard.callback.clone()
                };
                crate::renderer::emit_event_full(
                    &cb,
                    0,
                    APPLICATION_REOPEN_EVENT,
                    |_| {},
                );
                show_menu(state_arc);
                return 0;
            } else if mouse_msg == WM_RBUTTONUP || mouse_msg == WM_CONTEXTMENU {
                show_menu(state_arc);
                return 0;
            }
        } else if msg == WM_NCDESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let _ = Box::from_raw(state_ptr as *mut Arc<Mutex<TrayState>>);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        } else {
            let taskbar_msg = {
                let guard = state_arc.lock();
                guard.taskbar_created_msg
            };
            if taskbar_msg != 0 && msg == taskbar_msg {
                let guard = state_arc.lock();
                guard.add_tray_icon();
                return 0;
            }
        }
    }

    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn show_menu(state_arc: &Arc<Mutex<TrayState>>) {
    let (rows, callback, hwnd) = {
        let guard = state_arc.lock();
        (
            rows(&guard.items, &guard.application_title),
            guard.callback.clone(),
            guard.hwnd,
        )
    };

    let hmenu = unsafe { CreatePopupMenu() };
    if hmenu == 0 {
        return;
    }

    let mut action_tokens = Vec::new();
    const ID_OPEN: usize = 1;
    const ID_QUIT: usize = 2;
    const ID_ACTION_BASE: usize = 100;

    for row in rows {
        match row {
            TrayRow::Separator => unsafe {
                AppendMenuW(hmenu, MF_SEPARATOR, 0, std::ptr::null());
            },
            TrayRow::Status { label } => {
                let wide = to_wide_null(&label);
                unsafe {
                    AppendMenuW(hmenu, MF_STRING | MF_GRAYED, 0, wide.as_ptr());
                }
            }
            TrayRow::Action {
                token,
                label,
                enabled,
                checked,
            } => {
                let mut flags = MF_STRING;
                if !enabled {
                    flags |= MF_GRAYED;
                }
                if checked {
                    flags |= MF_CHECKED;
                }
                let id = ID_ACTION_BASE + action_tokens.len();
                action_tokens.push(token);
                let wide = to_wide_null(&label);
                unsafe {
                    AppendMenuW(hmenu, flags, id, wide.as_ptr());
                }
            }
            TrayRow::Builtin { click, label } => {
                let id = match click {
                    TrayClick::Open => ID_OPEN,
                    TrayClick::Quit => ID_QUIT,
                    TrayClick::Token(_) => unreachable!(),
                };
                let wide = to_wide_null(&label);
                unsafe {
                    AppendMenuW(hmenu, MF_STRING, id, wide.as_ptr());
                }
            }
        }
    }

    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenuEx(
            hmenu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD,
            pt.x,
            pt.y,
            hwnd,
            std::ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(hmenu);

        match cmd as usize {
            0 => {}
            ID_OPEN => {
                if let Err(error) = crate::renderer::show_main_window_from_menu() {
                    log::error!("Failed to reopen the Solo window: {error}");
                }
            }
            ID_QUIT => {
                if let Err(error) = crate::renderer::quit_from_menu() {
                    log::error!("Failed to quit the Solo application: {error}");
                }
            }
            c if c >= ID_ACTION_BASE => {
                let index = c - ID_ACTION_BASE;
                if let Some(token) = action_tokens.get(index) {
                    crate::renderer::emit_event_full(
                        &callback,
                        0,
                        MENU_BAR_ACTION_EVENT,
                        |event| event.value = Some(token.clone()),
                    );
                }
            }
            _ => {}
        }
    }
}

pub(crate) struct MenuBar {
    state: Arc<Mutex<TrayState>>,
}

impl MenuBar {
    pub(crate) fn new(
        options: &crate::renderer::MenuBarOptions,
        application_title: &str,
        callback: Option<EventCallback>,
    ) -> Result<Self> {
        let hicon = load_hicon(&options.icon_path)?;
        let taskbar_created_msg = unsafe {
            let name = to_wide_null("TaskbarCreated");
            RegisterWindowMessageW(name.as_ptr())
        };

        let state = Arc::new(Mutex::new(TrayState {
            hwnd: 0,
            hicon,
            application_title: application_title.to_string(),
            initial_icon_path: options.icon_path.clone(),
            current_icon_path: options.icon_path.clone(),
            initial_tooltip: options.tooltip.clone(),
            current_tooltip: options.tooltip.clone(),
            items: Vec::new(),
            callback,
            taskbar_created_msg,
        }));

        let hwnd = create_tray_window(Arc::clone(&state))?;
        state.lock().hwnd = hwnd;
        state.lock().add_tray_icon();

        Ok(Self { state })
    }

    pub(crate) fn update(
        &mut self,
        json: Option<&str>,
        callback: Option<EventCallback>,
    ) -> Result<()> {
        let update = json.map(parse_update).transpose()?;
        let mut guard = self.state.lock();

        let icon_path = update
            .as_ref()
            .and_then(|u| u.icon_path.as_deref())
            .unwrap_or(&guard.initial_icon_path);

        if icon_path != guard.current_icon_path {
            let new_icon = load_hicon(icon_path)?;
            let old_icon = guard.hicon;
            guard.hicon = new_icon;
            guard.current_icon_path = icon_path.to_string();
            if old_icon != 0 {
                unsafe { DestroyIcon(old_icon) };
            }
        }

        let tooltip = update
            .as_ref()
            .and_then(|u| u.tooltip.clone())
            .unwrap_or_else(|| guard.initial_tooltip.clone());

        guard.current_tooltip = tooltip;
        guard.items = update.map(|u| u.items).unwrap_or_default();
        if callback.is_some() {
            guard.callback = callback;
        }

        guard.update_tray_icon();
        Ok(())
    }
}

impl Drop for MenuBar {
    fn drop(&mut self) {
        let (hwnd, hicon) = {
            let guard = self.state.lock();
            guard.delete_tray_icon();
            (guard.hwnd, guard.hicon)
        };
        if hicon != 0 {
            unsafe { DestroyIcon(hicon) };
        }
        if hwnd != 0 {
            unsafe {
                DestroyWindow(hwnd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(token: &str, enabled: bool, checked: bool) -> MenuBarItem {
        MenuBarItem::Action {
            token: token.into(),
            label: format!("Item {token}"),
            enabled,
            checked,
        }
    }

    #[test]
    fn menu_is_app_items_then_open_and_quit() {
        let items = vec![
            action("1", true, false),
            MenuBarItem::Separator,
            MenuBarItem::Status {
                label: "Idle".into(),
            },
        ];
        assert_eq!(
            rows(&items, "Newsprint"),
            vec![
                TrayRow::Action {
                    token: "1".into(),
                    label: "Item 1".into(),
                    enabled: true,
                    checked: false,
                },
                TrayRow::Separator,
                TrayRow::Status {
                    label: "Idle".into()
                },
                TrayRow::Separator,
                TrayRow::Builtin {
                    click: TrayClick::Open,
                    label: "Open Newsprint".into(),
                },
                TrayRow::Separator,
                TrayRow::Builtin {
                    click: TrayClick::Quit,
                    label: "Quit Newsprint".into(),
                },
            ]
        );
    }

    #[test]
    fn a_menu_without_app_items_starts_at_open() {
        assert_eq!(
            rows(&[], "Solo"),
            vec![
                TrayRow::Builtin {
                    click: TrayClick::Open,
                    label: "Open Solo".into(),
                },
                TrayRow::Separator,
                TrayRow::Builtin {
                    click: TrayClick::Quit,
                    label: "Quit Solo".into(),
                },
            ]
        );
    }

    #[test]
    fn tooltip_field_sets_up_to_127_chars_and_null_terminates() {
        let mut buf = [0u16; 128];
        set_tooltip_field(&mut buf, Some("Hello World"));
        let end = buf.iter().position(|&c| c == 0).unwrap();
        assert_eq!(String::from_utf16_lossy(&buf[..end]), "Hello World");

        let long = "x".repeat(200);
        set_tooltip_field(&mut buf, Some(&long));
        assert_eq!(buf[127], 0);
        let end = buf.iter().position(|&c| c == 0).unwrap();
        assert_eq!(end, 127);
    }
}
