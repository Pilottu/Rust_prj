use slint::Weak;
use std::{thread, time::Duration};

use crate::AppWindow;

// Всё, что трогает голый WinAPI: заголовок окна, буфер обмена и опрос
// глобальных горячих клавиш (Ctrl+Z/C/V) фоновым потоком, пока приложение
// свёрнуто/не в фокусе ввода Slint. Не имеет отношения к бизнес-логике
// дерева — чисто платформенный слой.

#[link(name = "user32")]
extern "system" {
    fn FindWindowW(lp_c: *const u16, lp_w: *const u16) -> *mut std::ffi::c_void;
    fn ShowWindow(hwnd: *mut std::ffi::c_void, n_cmd_show: i32) -> i32;
    fn SetWindowPos(
        hwnd: *mut std::ffi::c_void,
        h_after: *mut std::ffi::c_void,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> i32;
    fn GetWindowLongW(hwnd: *mut std::ffi::c_void, n_index: i32) -> i32;
    fn SetWindowLongW(hwnd: *mut std::ffi::c_void, n_idx: i32, dw: i32) -> i32;
    fn GetAsyncKeyState(v_key: i32) -> i16;
    fn OpenClipboard(hwnd: *mut std::ffi::c_void) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(u_format: u32, h_mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn GetClipboardData(u_format: u32) -> *mut std::ffi::c_void;
    fn GetForegroundWindow() -> *mut std::ffi::c_void;
}
#[link(name = "kernel32")]
extern "system" {
    fn GlobalAlloc(u_flags: u32, dw_bytes: usize) -> *mut std::ffi::c_void;
    fn GlobalLock(h_mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn GlobalUnlock(h_mem: *mut std::ffi::c_void) -> i32;
    fn GlobalFree(h_mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}
const GWL_STYLE: i32 = -16;
const WS_CAPTION: i32 = 0x00C00000;
const WS_MAXIMIZEBOX: i32 = 0x00010000;
const WS_MINIMIZEBOX: i32 = 0x00020000;
const WS_THICKFRAME: i32 = 0x00040000;
const SWP_FRAMECHANGED: u32 = 0x0020;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOZORDER: u32 = 0x0004;
const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

pub fn set_clipboard_text(text: &str) {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) != 0 {
            EmptyClipboard();
            let v: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let h_mem = GlobalAlloc(GMEM_MOVEABLE, v.len() * 2);
            if !h_mem.is_null() {
                let ptr = GlobalLock(h_mem) as *mut u16;
                std::ptr::copy_nonoverlapping(v.as_ptr(), ptr, v.len());
                GlobalUnlock(h_mem);
                if SetClipboardData(CF_UNICODETEXT, h_mem).is_null() {
                    GlobalFree(h_mem);
                }
            }
            CloseClipboard();
        }
    }
}
pub fn get_clipboard_text() -> Option<String> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) != 0 {
            let h_mem = GetClipboardData(CF_UNICODETEXT);
            if !h_mem.is_null() {
                let ptr = GlobalLock(h_mem) as *const u16;
                if !ptr.is_null() {
                    let mut len = 0;
                    while *ptr.add(len) != 0 {
                        len += 1;
                    }
                    let slice = std::slice::from_raw_parts(ptr, len);
                    let res = String::from_utf16(slice).ok();
                    GlobalUnlock(h_mem);
                    CloseClipboard();
                    return res;
                }
                GlobalUnlock(h_mem);
            }
            CloseClipboard();
        }
    }
    None
}

pub fn fix_window_chrome(app_weak: Weak<AppWindow>) {
    slint::invoke_from_event_loop(move || {
        if let Some(_ui) = app_weak.upgrade() {
            let title: Vec<u16> = "Менеджер команд голосового управления (CAD Среда)\0"
                .encode_utf16()
                .collect();
            unsafe {
                let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
                if !hwnd.is_null() {
                    let cur = GetWindowLongW(hwnd, GWL_STYLE);
                    SetWindowLongW(
                        hwnd,
                        GWL_STYLE,
                        cur | WS_CAPTION | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_THICKFRAME,
                    );
                    SetWindowPos(
                        hwnd,
                        std::ptr::null_mut(),
                        0,
                        0,
                        0,
                        0,
                        SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
                    );
                    ShowWindow(hwnd, 3);
                }
            }
        }
    })
    .unwrap();
}

pub fn spawn_hotkey_thread(app_weak: Weak<AppWindow>) {
    thread::spawn(move || {
        let (mut c_w, mut z_w, mut cc_w, mut v_w) = (false, false, false, false);
        let title: Vec<u16> = "Менеджер команд голосового управления (CAD Среда)\0"
            .encode_utf16()
            .collect();
        let mut hw: *mut std::ffi::c_void = std::ptr::null_mut();
        loop {
            thread::sleep(Duration::from_millis(50));
            unsafe {
                if hw.is_null() {
                    hw = FindWindowW(std::ptr::null(), title.as_ptr());
                }
                let active = GetForegroundWindow();
                if !hw.is_null() && (active == hw) {
                    let ctrl = (GetAsyncKeyState(0x11) & -32768_i16) != 0;
                    let z = (GetAsyncKeyState(0x5A) & -32768_i16) != 0;
                    let c = (GetAsyncKeyState(0x43) & -32768_i16) != 0;
                    let v = (GetAsyncKeyState(0x56) & -32768_i16) != 0;
                    if ctrl && z && (!c_w || !z_w) {
                        let aw = app_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(a) = aw.upgrade() {
                                a.invoke_undo_action();
                            }
                        });
                    }
                    if ctrl && c && (!c_w || !cc_w) {
                        let aw = app_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(a) = aw.upgrade() {
                                a.invoke_copy_action();
                            }
                        });
                    }
                    if ctrl && v && (!c_w || !v_w) {
                        let aw = app_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(a) = aw.upgrade() {
                                a.invoke_paste_action();
                            }
                        });
                    }
                    c_w = ctrl;
                    z_w = z;
                    cc_w = c;
                    v_w = v;
                }
            }
        }
    });
}
