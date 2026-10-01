//! Win32 plumbing: window styles, placement, click-through region,
//! foreground-window tracking, autostart and single-instance guard.

use std::cell::RefCell;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateRectRgn, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, SetWindowRgn, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetDesktopWindow, GetForegroundWindow, GetShellWindow, GetWindowLongPtrW, GetWindowRect, IsIconic,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND, GWL_EXSTYLE,
    HWND_TOPMOST, OBJID_WINDOW, SWP_NOACTIVATE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

pub fn hwnd_of(window: &slint::Window) -> Option<HWND> {
    let handle = window.window_handle();
    let raw = handle.window_handle().ok()?.as_raw();
    match raw {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// No taskbar button, never steals focus, always on top.
pub fn make_overlay(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let ex = (ex | WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0 | WS_EX_TOPMOST.0) & !WS_EX_APPWINDOW.0;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex as isize);
    }
}

pub fn show(hwnd: HWND, visible: bool) {
    unsafe {
        let _ = ShowWindow(hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
    }
}

fn monitor_rect(mon: HMONITOR) -> RECT {
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe {
        let _ = GetMonitorInfoW(mon, &mut info);
    }
    info.rcMonitor
}

/// Centers the window horizontally at the very top of the primary monitor.
pub fn place_top_center(hwnd: HWND) {
    unsafe {
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let m = monitor_rect(mon);
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        let w = r.right - r.left;
        let x = m.left + (m.right - m.left - w) / 2;
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, m.top, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

/// Restricts hit-testing (and drawing) to the pill's bounding box so the rest
/// of the transparent canvas doesn't block clicks on windows below.
/// Sizes are in logical pixels.
pub fn set_hit_region(hwnd: HWND, scale: f32, pill_w: f32, pill_h: f32, top: f32) {
    unsafe {
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        let win_w = (r.right - r.left) as f32;
        // Room for the drop shadow; almost none for the thin tucked handle so
        // it blocks as little of the app below as possible.
        let margin = if pill_h < 12.0 { 3.0 } else { 14.0 } * scale;
        let pw = pill_w * scale;
        let left = ((win_w - pw) / 2.0 - margin).floor() as i32;
        let right = ((win_w + pw) / 2.0 + margin).ceil() as i32;
        let bottom = ((top + pill_h) * scale + margin).ceil() as i32;
        let rgn = CreateRectRgn(left.max(0), 0, right, bottom);
        // The system owns the region after this call.
        let _ = SetWindowRgn(hwnd, Some(rgn), true);
    }
}

// ---------- foreground window tracking ----------

/// What the foreground window is doing relative to the island.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Foreground {
    /// Covers the whole monitor (game, video).
    pub fullscreen: bool,
    /// Overlaps the island's spot at the top of the screen (e.g. a maximised
    /// browser whose tabs would otherwise be hidden).
    pub under_island: bool,
}

thread_local! {
    static FOREGROUND_CB: RefCell<Option<Box<dyn Fn(Foreground)>>> = const { RefCell::new(None) };
    static ISLAND: RefCell<Option<(HWND, Foreground)>> = const { RefCell::new(None) };
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn foreground_state(island: HWND) -> Foreground {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() || fg == island || fg == GetDesktopWindow() || fg == GetShellWindow() || IsIconic(fg).as_bool()
        {
            return Foreground::default();
        }
        if matches!(class_name(fg).as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
            return Foreground::default();
        }
        let mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        if mon != MonitorFromWindow(island, MONITOR_DEFAULTTONEAREST) {
            return Foreground::default();
        }
        let m = monitor_rect(mon);
        let mut r = RECT::default();
        let _ = GetWindowRect(fg, &mut r);
        let fullscreen = r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom;

        // The compact island's area: middle of the canvas, top ~56 px.
        let mut w = RECT::default();
        let _ = GetWindowRect(island, &mut w);
        let (ww, wh) = (w.right - w.left, w.bottom - w.top);
        let zone = RECT {
            left: w.left + ww * 60 / 520,
            right: w.right - ww * 60 / 520,
            top: w.top,
            bottom: w.top + wh * 56 / 420,
        };
        let under_island = r.left < zone.right && r.right > zone.left && r.top < zone.bottom && r.bottom > zone.top;
        Foreground { fullscreen, under_island }
    }
}

unsafe extern "system" fn win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // Location changes fire for carets, scrollbars etc. — only care about the
    // foreground top-level window (moved, maximised, entered F11...).
    if event == EVENT_OBJECT_LOCATIONCHANGE && (id_object != OBJID_WINDOW.0 || hwnd != GetForegroundWindow()) {
        return;
    }
    let changed = ISLAND.with(|i| {
        let mut i = i.borrow_mut();
        let (island, last) = i.as_mut()?;
        let now = foreground_state(*island);
        (now != *last).then(|| {
            *last = now;
            now
        })
    });
    if let Some(state) = changed {
        FOREGROUND_CB.with(|cb| {
            if let Some(cb) = cb.borrow().as_ref() {
                cb(state);
            }
        });
    }
}

/// Calls `cb` now and whenever the foreground window starts or stops being
/// fullscreen / under the island. Must be called on the UI thread.
pub fn watch_foreground(island: HWND, cb: impl Fn(Foreground) + 'static) {
    let initial = foreground_state(island);
    cb(initial);
    ISLAND.with(|i| *i.borrow_mut() = Some((island, initial)));
    FOREGROUND_CB.with(|c| *c.borrow_mut() = Some(Box::new(cb)));
    unsafe {
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(win_event), 0, 0, flags);
        SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE, None, Some(win_event), 0, 0, flags);
    }
}

// ---------- autostart ----------

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_NAME: PCWSTR = w!("DynamicIsland");

fn open_run_key(access: REG_SAM_FLAGS) -> Option<HKEY> {
    let mut key = HKEY::default();
    unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, access, &mut key).is_ok().then_some(key) }
}

pub fn autostart_enabled() -> bool {
    let Some(key) = open_run_key(KEY_QUERY_VALUE) else { return false };
    let ok = unsafe { RegQueryValueExW(key, RUN_NAME, None, None, None, None).is_ok() };
    unsafe {
        let _ = RegCloseKey(key);
    }
    ok
}

pub fn set_autostart(enable: bool) {
    let Some(key) = open_run_key(KEY_SET_VALUE) else { return };
    unsafe {
        if enable {
            if let Ok(exe) = std::env::current_exe() {
                let value: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain([0]).collect();
                let bytes = std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2);
                let _ = RegSetValueExW(key, RUN_NAME, None, REG_SZ, Some(bytes));
            }
        } else {
            let _ = RegDeleteValueW(key, RUN_NAME);
        }
        let _ = RegCloseKey(key);
    }
}

/// Returns false if another instance is already running.
pub fn single_instance() -> bool {
    unsafe {
        let handle = CreateMutexW(None, true, w!("Local\\DynamicIsland.SingleInstance"));
        let already = GetLastError() == ERROR_ALREADY_EXISTS;
        // Leak the handle: it must live as long as the process.
        std::mem::forget(handle);
        !already
    }
}
