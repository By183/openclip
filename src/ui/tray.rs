//! System tray integration for Windows: keeps OpenClip alive in the background
//! when minimized / closed, with a native popup context menu (Open, Record, Exit)
//! and global hotkey support.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, NIF_ICON, NIF_MESSAGE, NIF_TIP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, EnumWindows, GetCursorPos, GetMessageW, GetWindowLongW, GetWindowTextW,
    GetWindowThreadProcessId, LoadIconW, PostMessageW, RegisterClassExW, SetForegroundWindow,
    ShowWindow, TrackPopupMenu, TranslateMessage, GWL_STYLE, HICON, MF_SEPARATOR, MF_STRING,
    MSG, SW_RESTORE, SW_SHOW, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE,
    WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
    WNDCLASSEXW, WS_CAPTION, WS_OVERLAPPED,
};

pub const WM_TRAY_CALLBACK: u32 = WM_APP + 201;
const TRAY_ICON_ID: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Open,
    ToggleRecord,
    Exit,
}

#[derive(Debug, Clone)]
pub struct TrayLabels {
    pub open: String,
    pub start_rec: String,
    pub stop_rec: String,
    pub exit: String,
    pub tooltip: String,
}

impl Default for TrayLabels {
    fn default() -> Self {
        Self {
            open: "Open OpenClip".into(),
            start_rec: "Start Recording".into(),
            stop_rec: "Stop Recording".into(),
            exit: "Exit".into(),
            tooltip: "OpenClip - Screen Recorder".into(),
        }
    }
}

struct TrayInner {
    tx: Sender<TrayEvent>,
    is_recording: Arc<AtomicBool>,
    labels: Mutex<TrayLabels>,
    main_hwnd: Arc<AtomicIsize>,
    egui_ctx: Mutex<Option<egui::Context>>,
    force_quit: Arc<AtomicBool>,
}

static TRAY_INNER: Mutex<Option<Arc<TrayInner>>> = Mutex::new(None);

pub struct TrayManager {
    thread: Option<JoinHandle<()>>,
    hwnd: isize,
    labels: Arc<Mutex<TrayLabels>>,
    main_hwnd: Arc<AtomicIsize>,
    _force_quit: Arc<AtomicBool>,
}

unsafe extern "system" fn enum_windows_callback(
    hwnd: HWND,
    lparam: LPARAM,
) -> BOOL {
    // SAFETY: Win32 EnumWindows callback.
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
            // Exclude helper/IME/message windows by requiring standard window caption
            if (style & WS_CAPTION.0) != 0 {
                let mut title = [0u16; 256];
                let len = GetWindowTextW(hwnd, &mut title);
                let text = String::from_utf16_lossy(&title[..len as usize]);
                let mut cls = [0u16; 256];
                let cls_len = windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, &mut cls);
                let cls_name = String::from_utf16_lossy(&cls[..cls_len as usize]);
                if text.to_lowercase().contains("openclip") || cls_name == "Window Class" {
                    let out = lparam.0 as *mut HWND;
                    if !out.is_null() {
                        *out = hwnd;
                    }
                    return BOOL(0);
                }
            }
        }
    }
    BOOL(1)
}

pub fn find_main_hwnd() -> Option<HWND> {
    let mut hwnd = HWND::default();
    unsafe {
        let _ = EnumWindows(Some(enum_windows_callback), LPARAM(&mut hwnd as *mut _ as isize));
    }
    if hwnd.0.is_null() {
        None
    } else {
        Some(hwnd)
    }
}

unsafe extern "system" fn tray_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY_CALLBACK => {
            let event = lparam.0 as u32;
            let low_event = event & 0xFFFF;
            let is_lbutton = event == WM_LBUTTONUP
                || event == WM_LBUTTONDBLCLK
                || low_event == WM_LBUTTONUP
                || low_event == WM_LBUTTONDBLCLK;
            let is_rbutton = event == WM_RBUTTONUP
                || event == WM_CONTEXTMENU
                || low_event == WM_RBUTTONUP
                || low_event == WM_CONTEXTMENU;

            if is_lbutton {
                if let Some(inner) = TRAY_INNER.lock().unwrap().as_ref().cloned() {
                    // Restore main window directly from the tray thread immediately
                    let raw = inner.main_hwnd.load(Ordering::Relaxed);
                    let main_h = if raw != 0 {
                        HWND(raw as *mut _)
                    } else if let Some(h) = find_main_hwnd() {
                        inner.main_hwnd.store(h.0 as isize, Ordering::Relaxed);
                        h
                    } else {
                        HWND::default()
                    };
                    if !main_h.0.is_null() {
                        unsafe {
                            let _ = ShowWindow(main_h, SW_RESTORE);
                            let _ = ShowWindow(main_h, SW_SHOW);
                            let _ = SetForegroundWindow(main_h);
                            let _ = PostMessageW(Some(main_h), WM_NULL, WPARAM(0), LPARAM(0));
                        }
                    }
                    if let Some(ctx) = inner.egui_ctx.lock().unwrap().as_ref() {
                        ctx.request_repaint();
                    }
                    let _ = inner.tx.send(TrayEvent::Open);
                }
                return LRESULT(0);
            } else if is_rbutton {
                if let Some(inner) = TRAY_INNER.lock().unwrap().as_ref().cloned() {
                    unsafe {
                        let _ = SetForegroundWindow(hwnd);
                        let mut pt = POINT::default();
                        let _ = GetCursorPos(&mut pt);

                        if let Ok(menu) = CreatePopupMenu() {
                            let (open_str, rec_str, exit_str) = {
                                let guard = inner.labels.lock().unwrap();
                                let rec = if inner.is_recording.load(Ordering::Relaxed) {
                                    guard.stop_rec.clone()
                                } else {
                                    guard.start_rec.clone()
                                };
                                (guard.open.clone(), rec, guard.exit.clone())
                            };

                            let open_w: Vec<u16> = format!("{open_str}\0").encode_utf16().collect();
                            let rec_w: Vec<u16> = format!("{rec_str}\0").encode_utf16().collect();
                            let exit_w: Vec<u16> = format!("{exit_str}\0").encode_utf16().collect();

                            let _ = AppendMenuW(menu, MF_STRING, 1001, PCWSTR(open_w.as_ptr()));
                            let _ = AppendMenuW(menu, MF_STRING, 1002, PCWSTR(rec_w.as_ptr()));
                            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                            let _ = AppendMenuW(menu, MF_STRING, 1003, PCWSTR(exit_w.as_ptr()));

                            let cmd = TrackPopupMenu(
                                menu,
                                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                                pt.x,
                                pt.y,
                                Some(0),
                                hwnd,
                                None,
                            );
                            let _ = DestroyMenu(menu);
                            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));

                            match cmd.0 {
                                1001 => {
                                    // "Open OpenClip" - restore directly in tray thread
                                    let raw = inner.main_hwnd.load(Ordering::Relaxed);
                                    let main_h = if raw != 0 {
                                        HWND(raw as *mut _)
                                    } else if let Some(h) = find_main_hwnd() {
                                        inner.main_hwnd.store(h.0 as isize, Ordering::Relaxed);
                                        h
                                    } else {
                                        HWND::default()
                                    };
                                    if !main_h.0.is_null() {
                                        let _ = ShowWindow(main_h, SW_RESTORE);
                                        let _ = ShowWindow(main_h, SW_SHOW);
                                        let _ = SetForegroundWindow(main_h);
                                        let _ = PostMessageW(Some(main_h), WM_NULL, WPARAM(0), LPARAM(0));
                                    }
                                    if let Some(ctx) = inner.egui_ctx.lock().unwrap().as_ref() {
                                        ctx.request_repaint();
                                    }
                                    let _ = inner.tx.send(TrayEvent::Open);
                                }
                                1002 => {
                                    // "Toggle Recording"
                                    let raw = inner.main_hwnd.load(Ordering::Relaxed);
                                    if raw != 0 {
                                        let _ = PostMessageW(Some(HWND(raw as *mut _)), WM_NULL, WPARAM(0), LPARAM(0));
                                    }
                                    if let Some(ctx) = inner.egui_ctx.lock().unwrap().as_ref() {
                                        ctx.request_repaint();
                                    }
                                    let _ = inner.tx.send(TrayEvent::ToggleRecord);
                                }
                                1003 => {
                                    // "Exit" - direct handling in tray thread
                                    let nid = NOTIFYICONDATAW {
                                        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                                        hWnd: hwnd,
                                        uID: TRAY_ICON_ID,
                                        ..Default::default()
                                    };
                                    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                                    inner.force_quit.store(true, Ordering::SeqCst);
                                    let _ = inner.tx.send(TrayEvent::Exit);

                                    let raw = inner.main_hwnd.load(Ordering::Relaxed);
                                    if raw != 0 {
                                        let _ = PostMessageW(Some(HWND(raw as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
                                    }
                                    if let Some(ctx) = inner.egui_ctx.lock().unwrap().as_ref() {
                                        ctx.request_repaint();
                                    }

                                    // Guaranteed process exit after 150 ms if UI is frozen
                                    std::thread::Builder::new()
                                        .name("openclip-exit-fallback".into())
                                        .spawn(move || {
                                            std::thread::sleep(std::time::Duration::from_millis(150));
                                            std::process::exit(0);
                                        })
                                        .ok();
                                }
                                _ => {}
                            }
                        }
                    }
                }
                return LRESULT(0);
            }
        }
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

impl TrayManager {
    pub fn new(
        is_recording: Arc<AtomicBool>,
        egui_ctx: egui::Context,
        force_quit: Arc<AtomicBool>,
        initial_main_hwnd: Option<HWND>,
    ) -> (Self, Receiver<TrayEvent>) {
        let (tx, rx) = channel();
        let labels = Arc::new(Mutex::new(TrayLabels::default()));
        let main_hwnd = Arc::new(AtomicIsize::new(
            initial_main_hwnd.map(|h| h.0 as isize).unwrap_or(0),
        ));
        if main_hwnd.load(Ordering::Relaxed) == 0
            && let Some(h) = find_main_hwnd()
        {
            main_hwnd.store(h.0 as isize, Ordering::Relaxed);
        }
        let inner = Arc::new(TrayInner {
            tx,
            is_recording,
            labels: Mutex::new(TrayLabels::default()),
            main_hwnd: main_hwnd.clone(),
            egui_ctx: Mutex::new(Some(egui_ctx)),
            force_quit: force_quit.clone(),
        });
        *TRAY_INNER.lock().unwrap() = Some(inner.clone());

        let (hwnd_tx, hwnd_rx) = channel::<isize>();

        let thread = std::thread::Builder::new()
            .name("openclip-tray".into())
            .spawn(move || {
                let class_name = w!("OpenClipTrayMessageWindow");
                let wc = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(tray_window_proc),
                    lpszClassName: PCWSTR(class_name.as_ptr()),
                    ..Default::default()
                };
                unsafe {
                    let _ = RegisterClassExW(&wc);
                }

                // Overlapped hidden window (not message-only) so Shell_NotifyIconW messages are delivered
                let hwnd = unsafe {
                    CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        class_name,
                        w!("OpenClipTrayReceiver"),
                        WS_OVERLAPPED,
                        0,
                        0,
                        0,
                        0,
                        None,
                        None,
                        Some(HINSTANCE::default()),
                        None,
                    )
                }
                .unwrap_or(HWND::default());

                let _ = hwnd_tx.send(hwnd.0 as isize);
                if hwnd.0.is_null() {
                    return;
                }

                // Always delete any leftover ghost icon from a previous crashed/taskkilled instance
                let mut nid = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: TRAY_ICON_ID,
                    ..Default::default()
                };
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                }

                // Load embedded icon resource (id 1 from build.rs winresource)
                let hinstance = unsafe { GetModuleHandleW(None).unwrap_or_default() };
                let icon = unsafe {
                    LoadIconW(Some(HINSTANCE(hinstance.0)), PCWSTR(1 as _)).unwrap_or_else(|_| {
                        LoadIconW(
                            Some(HINSTANCE::default()),
                            windows::Win32::UI::WindowsAndMessaging::IDI_APPLICATION,
                        )
                        .unwrap_or(HICON::default())
                    })
                };

                nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
                nid.uCallbackMessage = WM_TRAY_CALLBACK;
                nid.hIcon = icon;
                let tip: Vec<u16> = "OpenClip\0".encode_utf16().collect();
                for (i, &c) in tip.iter().take(nid.szTip.len() - 1).enumerate() {
                    nid.szTip[i] = c;
                }

                unsafe {
                    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
                }

                let mut msg = MSG::default();
                unsafe {
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                }
            })
            .ok();

        let hwnd = hwnd_rx.recv().unwrap_or(0);

        (
            Self {
                thread,
                hwnd,
                labels,
                main_hwnd,
                _force_quit: force_quit,
            },
            rx,
        )
    }

    pub fn set_main_hwnd(&self, hwnd: HWND) {
        self.main_hwnd.store(hwnd.0 as isize, Ordering::Relaxed);
    }

    pub fn main_hwnd(&self) -> isize {
        self.main_hwnd.load(Ordering::Relaxed)
    }

    pub fn restore_main_window(&self) {
        let raw = self.main_hwnd.load(Ordering::Relaxed);
        let hwnd = if raw != 0 {
            HWND(raw as *mut _)
        } else if let Some(h) = find_main_hwnd() {
            self.main_hwnd.store(h.0 as isize, Ordering::Relaxed);
            h
        } else {
            HWND::default()
        };
        if !hwnd.0.is_null() {
            unsafe {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
            }
        }
    }

    pub fn update_labels(&self, labels: TrayLabels) {
        *self.labels.lock().unwrap() = labels.clone();
        if let Some(inner) = TRAY_INNER.lock().unwrap().as_ref() {
            *inner.labels.lock().unwrap() = labels.clone();
        }
        if self.hwnd != 0 {
            let mut nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: HWND(self.hwnd as *mut _),
                uID: TRAY_ICON_ID,
                uFlags: NIF_TIP,
                ..Default::default()
            };
            let tip: Vec<u16> = format!("{}\0", labels.tooltip).encode_utf16().collect();
            for (i, &c) in tip.iter().take(nid.szTip.len() - 1).enumerate() {
                nid.szTip[i] = c;
            }
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
            }
        }
    }
}

impl Drop for TrayManager {
    fn drop(&mut self) {
        *TRAY_INNER.lock().unwrap() = None;
        if self.hwnd != 0 {
            let hwnd = HWND(self.hwnd as *mut _);
            unsafe {
                let nid = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: TRAY_ICON_ID,
                    ..Default::default()
                };
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
