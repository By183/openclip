//! Interactive screen snipping tool and annotation editor (ShareX / Snipping Tool style).
//!
//! Freezes the desktop on activation, presents a borderless multi-monitor
//! overlay, allows region selection with 8-handle resizing and moving,
//! provides drawing tools (rectangle, arrow, line, pen, highlighter, text, blur/pixelate),
//! and supports saving to library and copying to clipboard simultaneously.

use anyhow::{Context, Result};
use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, CursorIcon, FontId, Key, Margin, Modifiers,
    PointerButton, Pos2, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, UiBuilder,
    Vec2, ViewportBuilder, ViewportId,
};

use crate::capture::monitors::{screenshot_monitor_image, MonitorInfo};
use crate::capture::Rect as PhysRect;
use crate::ui::icons;
use crate::ui::theme::{BLUE, CARD, GREEN, RED};
use openclip_overlay::sprite::Sprite;
use openclip_overlay::text::TextRenderer;

#[cfg(windows)]
mod win32_overlay {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE,
        DWMWCP_DONOTROUND,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, CallWindowProcW, FindWindowW, GetWindowLongPtrW, GetWindowRect, IsWindow,
        SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, GWL_STYLE,
        GWLP_WNDPROC, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_SHOWWINDOW,
        WM_ACTIVATE, WM_ERASEBKGND, WM_NCCALCSIZE, WM_NCHITTEST, WM_NCPAINT, WM_NCDESTROY, WS_CAPTION,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP,
        WS_SYSMENU, WS_THICKFRAME,
    };

    static WNDPROCS: Mutex<Option<HashMap<isize, isize>>> = Mutex::new(None);
    static DEACTIVATED_SIGNAL: AtomicBool = AtomicBool::new(false);
    static HAD_FOCUS_SIGNAL: AtomicBool = AtomicBool::new(false);

    const HTCLIENT: isize = 1;
    const DWMWA_COLOR_NONE: u32 = 0xFFFFFFFE;
    const WA_INACTIVE: usize = 0;
    const WA_ACTIVE: usize = 1;
    const WA_CLICKACTIVE: usize = 2;

    unsafe extern "system" fn overlay_subclass_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // Intercept non-client hit testing and always report client area.
        if msg == WM_NCHITTEST {
            return LRESULT(HTCLIENT);
        }
        // Suppress non-client painting to prevent any titlebar / border lines
        if msg == WM_NCPAINT {
            return LRESULT(0);
        }
        // Client area occupies 100% of the window
        if msg == WM_NCCALCSIZE {
            return LRESULT(0);
        }
        // Prevent background erase flickers
        if msg == WM_ERASEBKGND {
            return LRESULT(1);
        }

        // Track focus & deactivation
        if msg == WM_ACTIVATE {
            let state = wparam.0 & 0xFFFF;
            if state == WA_ACTIVE || state == WA_CLICKACTIVE {
                HAD_FOCUS_SIGNAL.store(true, Ordering::Relaxed);
            } else if state == WA_INACTIVE {
                let other_hwnd = lparam.0;
                let is_our_window = if let Ok(guard) = WNDPROCS.lock() {
                    guard.as_ref().is_some_and(|m| m.contains_key(&other_hwnd))
                } else {
                    false
                };
                if !is_our_window {
                    DEACTIVATED_SIGNAL.store(true, Ordering::Relaxed);
                }
            }
        }

        // On window destruction, restore the original wndproc and clean up WNDPROCS
        if msg == WM_NCDESTROY {
            let prev = {
                if let Ok(mut guard) = WNDPROCS.lock() {
                    guard.as_mut().and_then(|m| m.remove(&(hwnd.0 as isize))).unwrap_or(0)
                } else {
                    0
                }
            };
            if prev != 0 {
                use windows::Win32::UI::WindowsAndMessaging::WNDPROC;
                let prev_fn: WNDPROC = unsafe { std::mem::transmute(prev) };
                unsafe {
                    let _ = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, prev);
                    return CallWindowProcW(prev_fn, hwnd, msg, wparam, lparam);
                }
            }
        }

        let prev = {
            if let Ok(guard) = WNDPROCS.lock() {
                guard.as_ref().and_then(|m| m.get(&(hwnd.0 as isize)).copied()).unwrap_or(0)
            } else {
                0
            }
        };

        if prev != 0 {
            use windows::Win32::UI::WindowsAndMessaging::WNDPROC;
            let prev_fn: WNDPROC = unsafe { std::mem::transmute(prev) };
            unsafe { CallWindowProcW(prev_fn, hwnd, msg, wparam, lparam) }
        } else {
            unsafe { windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }

    pub fn setup_overlay_window(title: &str, info: &crate::capture::monitors::MonitorInfo) -> bool {
        let name = HSTRING::from(title);
        unsafe {
            let Ok(hwnd) = FindWindowW(PCWSTR::null(), PCWSTR(name.as_ptr())) else { return false };
            if hwnd.is_invalid() {
                return false;
            }

            // 1. Strip resize borders, caption, and system menu to avoid non-client handling
            let mut style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            style &= !(WS_THICKFRAME.0 | WS_CAPTION.0 | WS_MAXIMIZEBOX.0 | WS_MINIMIZEBOX.0 | WS_SYSMENU.0);
            style |= WS_POPUP.0;
            SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);

            let mut ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            ex_style |= WS_EX_TOPMOST.0 | WS_EX_TOOLWINDOW.0;
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_style as isize);

            // 2. Disable Windows 11 DWM accent border and rounded corners
            let border_color: u32 = DWMWA_COLOR_NONE;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_BORDER_COLOR,
                &border_color as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            );
            let corner_pref: u32 = DWMWCP_DONOTROUND.0 as u32;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner_pref as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            );

            // 3. Subclass to force WM_NCHITTEST -> HTCLIENT and prevent non-client painting
            let new_proc = overlay_subclass_proc as *const () as usize as isize;
            let already_registered = {
                if let Ok(guard) = WNDPROCS.lock() {
                    guard.as_ref().is_some_and(|m| m.contains_key(&(hwnd.0 as isize)))
                } else {
                    false
                }
            };

            if !already_registered {
                let current_proc = GetWindowLongPtrW(hwnd, GWLP_WNDPROC);
                if current_proc != new_proc {
                    let old_proc = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, new_proc);
                    if old_proc != 0 && old_proc != new_proc
                        && let Ok(mut guard) = WNDPROCS.lock()
                    {
                        guard.get_or_insert_with(HashMap::new).insert(hwnd.0 as isize, old_proc);
                    }
                }
            }

            // 4. Set exact physical position and size to match monitor bounds
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                info.x,
                info.y,
                info.width as i32,
                info.height as i32,
                SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );

            // 5. Focus window once
            let _ = SetForegroundWindow(hwnd);
            let _ = BringWindowToTop(hwnd);
            let _ = SetFocus(Some(hwnd));

            // 6. Verification log
            let mut rect = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rect);
            log::info!(
                "[Snipping Overlay] Setup HWND {:?}: title='{}', target=({}, {}, {}x{}), actual=({}, {}, {}x{}), monitor='{}', scale={}",
                hwnd, title,
                info.x, info.y, info.width, info.height,
                rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top,
                info.name, info.scale_factor
            );

            true
        }
    }

    pub fn restore_all_subclasses() {
        if let Ok(mut guard) = WNDPROCS.lock()
            && let Some(map) = guard.as_mut()
        {
            for (&hwnd_val, &prev) in map.iter() {
                let hwnd = HWND(hwnd_val as _);
                unsafe {
                    if !hwnd.is_invalid() && IsWindow(Some(hwnd)).as_bool() {
                        let _ = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, prev);
                    }
                }
            }
            map.clear();
        }
        DEACTIVATED_SIGNAL.store(false, Ordering::Relaxed);
        HAD_FOCUS_SIGNAL.store(false, Ordering::Relaxed);
    }

    pub fn had_focus() -> bool {
        HAD_FOCUS_SIGNAL.load(Ordering::Relaxed)
    }

    pub fn check_and_reset_deactivated() -> bool {
        DEACTIVATED_SIGNAL.swap(false, Ordering::Relaxed)
    }

    pub fn reset_signals() {
        DEACTIVATED_SIGNAL.store(false, Ordering::Relaxed);
        HAD_FOCUS_SIGNAL.store(false, Ordering::Relaxed);
    }
}

#[cfg(not(windows))]
mod win32_overlay {
    pub fn setup_overlay_window(_title: &str, _info: &crate::capture::monitors::MonitorInfo) -> bool {
        true
    }
    pub fn restore_all_subclasses() {}
    pub fn had_focus() -> bool { false }
    pub fn check_and_reset_deactivated() -> bool { false }
    pub fn reset_signals() {}
}

#[cfg(windows)]
fn is_key_just_pressed(vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY, was_down: &mut bool) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    let is_down = unsafe { (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0 };
    let just_pressed = is_down && !*was_down;
    *was_down = is_down;
    just_pressed
}

#[cfg(not(windows))]
fn is_key_just_pressed(_vk: u32, _was_down: &mut bool) -> bool {
    false
}

/// Outcome of the snipping tool execution.
pub enum SnippingOutcome {
    Pending,
    Saved(xcap::image::RgbaImage),
    CopiedOnly(xcap::image::RgbaImage),
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Select,
    Rect,
    Arrow,
    Line,
    Pen,
    Marker,
    Text,
    Pixelate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeHandle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Annotation {
    Rect { p1: Pos2, p2: Pos2, color: Color32, width: f32 },
    Arrow { p1: Pos2, p2: Pos2, color: Color32, width: f32 },
    Line { p1: Pos2, p2: Pos2, color: Color32, width: f32 },
    Pen { points: Vec<Pos2>, color: Color32, width: f32 },
    Marker { p1: Pos2, p2: Pos2, color: Color32, width: f32 },
    Text { pos: Pos2, text: String, color: Color32, font_size: f32, custom_width: Option<f32> },
    Pixelate { rect: Rect, blocks: Vec<(Rect, Color32)> },
}

fn dist_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len_sq = ab.x * ab.x + ab.y * ab.y;
    if len_sq <= 1e-4 {
        return p.distance(a);
    }
    let ap = p - a;
    let t = ((ap.x * ab.x + ap.y * ab.y) / len_sq).clamp(0.0, 1.0);
    let proj = a + ab * t;
    p.distance(proj)
}

pub fn wrap_text(text: &str, max_width: Option<f32>, font_size: f32, renderer: &TextRenderer) -> Vec<String> {
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        if let Some(limit_w) = max_width {
            let limit_w = limit_w.max(40.0);
            let mut cur_line = String::new();
            let words: Vec<&str> = raw_line.split_whitespace().collect();
            if words.is_empty() {
                lines.push(String::new());
                continue;
            }
            for word in words {
                let test = if cur_line.is_empty() {
                    word.to_string()
                } else {
                    format!("{cur_line} {word}")
                };
                let w = renderer.width(&test, font_size);
                if w <= limit_w || cur_line.is_empty() {
                    cur_line = test;
                } else {
                    lines.push(cur_line);
                    cur_line = word.to_string();
                }
            }
            if !cur_line.is_empty() {
                lines.push(cur_line);
            }
        } else {
            lines.push(raw_line.to_string());
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

pub fn measure_text_size(text: &str, font_size: f32, max_width: Option<f32>) -> (f32, f32) {
    if let Some(renderer) = TextRenderer::new() {
        let line_h = font_size * 1.3;
        let lines = wrap_text(text, max_width, font_size, &renderer);
        let mut max_w = 0.0f32;
        for l in &lines {
            max_w = max_w.max(renderer.width(l, font_size));
        }
        if let Some(mw) = max_width {
            max_w = max_w.max(mw.max(40.0));
        }
        (max_w.max(20.0), (lines.len() as f32 * line_h).max(line_h))
    } else {
        let char_w = font_size * 0.55;
        let line_h = font_size * 1.3;
        let lines: Vec<&str> = text.split('\n').collect();
        let max_w = lines.iter().map(|l| l.chars().count() as f32 * char_w).fold(0.0f32, f32::max);
        let eff_w = if let Some(mw) = max_width { mw.max(max_w).max(40.0) } else { max_w };
        (eff_w.max(20.0), (lines.len() as f32 * line_h).max(line_h))
    }
}

pub fn hit_test_text_handle(sel: Rect, pos: Pos2) -> Option<ResizeHandle> {
    let tol = 12.0;
    let check = |center: Pos2| pos.distance(center) <= tol;

    // Corner handles (scale font size with anchor in opposite corner)
    if check(sel.left_top()) { return Some(ResizeHandle::TopLeft); }
    if check(sel.right_top()) { return Some(ResizeHandle::TopRight); }
    if check(sel.right_bottom()) { return Some(ResizeHandle::BottomRight); }
    if check(sel.left_bottom()) { return Some(ResizeHandle::BottomLeft); }

    // Side handles (adjust width with word-wrap)
    if check(Pos2::new(sel.right(), sel.center().y)) { return Some(ResizeHandle::Right); }
    if check(Pos2::new(sel.left(), sel.center().y)) { return Some(ResizeHandle::Left); }

    // Top and Bottom handles are hidden and disabled
    None
}

pub fn resize_text_handle_pure(
    handle: ResizeHandle,
    start_rect: Rect,
    cur: Pos2,
    start_font_size: f32,
    text: &str,
    start_custom_width: Option<f32>,
) -> (Pos2, f32, Option<f32>) {
    let pad_x = 8.0;
    let pad_y = 5.0;
    let mut pos = start_rect.min + egui::vec2(pad_x, pad_y);
    let mut font_size = start_font_size;
    let mut custom_width = start_custom_width;

    match handle {
        ResizeHandle::Right => {
            let delta_x = cur.x - start_rect.right();
            let new_w = (start_rect.width() + delta_x - pad_x * 2.0).max(40.0);
            custom_width = Some(new_w);
        }
        ResizeHandle::Left => {
            let delta_x = cur.x - start_rect.left();
            let new_w = (start_rect.width() - delta_x - pad_x * 2.0).max(40.0);
            let new_left = start_rect.right() - (new_w + pad_x * 2.0);
            pos.x = new_left + pad_x;
            custom_width = Some(new_w);
        }
        ResizeHandle::BottomRight => {
            let anchor = start_rect.left_top();
            let diag = start_rect.right_bottom() - anchor;
            let proj = (cur - anchor).dot(diag) / diag.length_sq().max(1.0);
            font_size = (start_font_size * proj).clamp(12.0, 96.0);
        }
        ResizeHandle::TopLeft => {
            let anchor = start_rect.right_bottom();
            let diag = start_rect.left_top() - anchor;
            let proj = (cur - anchor).dot(diag) / diag.length_sq().max(1.0);
            let new_fs = (start_font_size * proj).clamp(12.0, 96.0);
            font_size = new_fs;
            let (w, h) = measure_text_size(text, new_fs, custom_width);
            pos = anchor - egui::vec2(w + pad_x, h + pad_y);
        }
        ResizeHandle::TopRight => {
            let anchor = start_rect.left_bottom();
            let diag = start_rect.right_top() - anchor;
            let proj = (cur - anchor).dot(diag) / diag.length_sq().max(1.0);
            let new_fs = (start_font_size * proj).clamp(12.0, 96.0);
            font_size = new_fs;
            let (_w, h) = measure_text_size(text, new_fs, custom_width);
            pos = Pos2::new(anchor.x + pad_x, anchor.y - (h + pad_y));
        }
        ResizeHandle::BottomLeft => {
            let anchor = start_rect.right_top();
            let diag = start_rect.left_bottom() - anchor;
            let proj = (cur - anchor).dot(diag) / diag.length_sq().max(1.0);
            let new_fs = (start_font_size * proj).clamp(12.0, 96.0);
            font_size = new_fs;
            let (w, _h) = measure_text_size(text, new_fs, custom_width);
            pos = Pos2::new(anchor.x - (w + pad_x), anchor.y + pad_y);
        }
        _ => {}
    }

    (pos, font_size, custom_width)
}

impl Annotation {
    pub fn bounding_box_with_ctx(&self, ctx: Option<&egui::Context>) -> Rect {
        match self {
            Annotation::Rect { p1, p2, width, .. } => {
                Rect::from_two_pos(*p1, *p2).expand(*width / 2.0 + 4.0)
            }
            Annotation::Arrow { p1, p2, width, .. } => {
                Rect::from_two_pos(*p1, *p2).expand(*width * 3.0 + 8.0)
            }
            Annotation::Line { p1, p2, width, .. } => {
                Rect::from_two_pos(*p1, *p2).expand(*width / 2.0 + 4.0)
            }
            Annotation::Pen { points, width, .. } => {
                if points.is_empty() {
                    Rect::from_two_pos(Pos2::ZERO, Pos2::ZERO)
                } else {
                    let mut min_x = points[0].x;
                    let mut max_x = points[0].x;
                    let mut min_y = points[0].y;
                    let mut max_y = points[0].y;
                    for &pt in &points[1..] {
                        min_x = min_x.min(pt.x);
                        max_x = max_x.max(pt.x);
                        min_y = min_y.min(pt.y);
                        max_y = max_y.max(pt.y);
                    }
                    Rect::from_min_max(Pos2::new(min_x, min_y), Pos2::new(max_x, max_y)).expand(*width / 2.0 + 4.0)
                }
            }
            Annotation::Marker { p1, p2, width, .. } => {
                Rect::from_two_pos(*p1, *p2).expand(*width * 2.0 + 4.0)
            }
            Annotation::Text { pos, text, font_size, custom_width, .. } => {
                let pad_x = 8.0;
                let pad_y = 5.0;
                let (w, h) = if let Some(ctx) = ctx {
                    let wrap_w = custom_width.map(|w| w.max(40.0)).unwrap_or(f32::INFINITY);
                    let sz = ctx.fonts_mut(|fonts| {
                        fonts.layout(text.clone(), FontId::proportional(*font_size), Color32::WHITE, wrap_w).size()
                    });
                    (sz.x.max(20.0), sz.y.max(*font_size))
                } else {
                    measure_text_size(text, *font_size, *custom_width)
                };
                Rect::from_min_size(*pos - egui::vec2(pad_x, pad_y), egui::vec2(w + pad_x * 2.0, h + pad_y * 2.0))
            }
            Annotation::Pixelate { rect, .. } => *rect,
        }
    }

    pub fn bounding_box(&self) -> Rect {
        self.bounding_box_with_ctx(None)
    }

    pub fn hit_test_with_ctx(&self, pos: Pos2, ctx: Option<&egui::Context>) -> bool {
        match self {
            Annotation::Rect { p1, p2, width, .. } => {
                let r = Rect::from_two_pos(*p1, *p2);
                let tol = (*width / 2.0 + 8.0).max(10.0);
                r.expand(tol).contains(pos)
            }
            Annotation::Arrow { p1, p2, width, .. } => {
                let tol = (*width / 2.0 + 10.0).max(12.0);
                dist_to_segment(pos, *p1, *p2) <= tol
            }
            Annotation::Line { p1, p2, width, .. } => {
                let tol = (*width / 2.0 + 8.0).max(10.0);
                dist_to_segment(pos, *p1, *p2) <= tol
            }
            Annotation::Pen { points, width, .. } => {
                let tol = (*width / 2.0 + 8.0).max(10.0);
                points.windows(2).any(|w| dist_to_segment(pos, w[0], w[1]) <= tol)
            }
            Annotation::Marker { p1, p2, width, .. } => {
                let tol = (*width * 2.0 + 8.0).max(12.0);
                dist_to_segment(pos, *p1, *p2) <= tol
            }
            Annotation::Text { .. } => {
                self.bounding_box_with_ctx(ctx).expand(8.0).contains(pos)
            }
            Annotation::Pixelate { rect, .. } => {
                rect.expand(6.0).contains(pos)
            }
        }
    }

    pub fn hit_test(&self, pos: Pos2) -> bool {
        self.hit_test_with_ctx(pos, None)
    }

    pub fn translate(&mut self, delta: Vec2) {
        match self {
            Annotation::Rect { p1, p2, .. } => {
                *p1 += delta;
                *p2 += delta;
            }
            Annotation::Arrow { p1, p2, .. } => {
                *p1 += delta;
                *p2 += delta;
            }
            Annotation::Line { p1, p2, .. } => {
                *p1 += delta;
                *p2 += delta;
            }
            Annotation::Pen { points, .. } => {
                for pt in points {
                    *pt += delta;
                }
            }
            Annotation::Marker { p1, p2, .. } => {
                *p1 += delta;
                *p2 += delta;
            }
            Annotation::Text { pos, .. } => {
                *pos += delta;
            }
            Annotation::Pixelate { rect, blocks } => {
                *rect = rect.translate(delta);
                for (cell, _) in blocks {
                    *cell = cell.translate(delta);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum DragState {
    None,
    Selecting { start: Pos2, current: Pos2 },
    Moving { start_rect: Rect, start_pos: Pos2, start_annotations: Vec<Annotation> },
    Resizing { handle: ResizeHandle, start_rect: Rect, start_pos: Pos2 },
    Drawing { start: Pos2, current: Pos2, pen_points: Vec<Pos2> },
    MovingAnnotation { index: usize, last_pos: Pos2, start_annotations: Vec<Annotation> },
    ResizingAnnotation { index: usize, handle: ResizeHandle, start_rect: Rect, start_pos: Pos2, start_font_size: f32, start_annotations: Vec<Annotation> },
    ResizingLine { index: usize, is_p2: bool, start_annotations: Vec<Annotation> },
}

struct MonitorView {
    info: MonitorInfo,
    raw: xcap::image::RgbaImage,
    tex: Option<TextureHandle>,
}

#[derive(Debug, Clone)]
struct TextInputState {
    pos: Pos2,
    buffer: String,
    font_size: f32,
    color: Color32,
    custom_width: Option<f32>,
}

pub struct SnippingTool {
    views: Vec<MonitorView>,
    /// Selected monitor index and rectangle in viewport points
    selection: Option<(usize, Rect)>,
    drag: DragState,
    tool: Tool,
    color: Color32,
    stroke_width: f32,
    font_size: f32,
    annotations: Vec<Annotation>,
    undo_stack: Vec<Vec<Annotation>>,
    redo_stack: Vec<Vec<Annotation>>,
    selected_index: Option<usize>,
    text_input: Option<TextInputState>,
    annotate_enabled: bool,
    outcome: Option<SnippingOutcome>,
    toolbar_rect: Option<Rect>,
    configured_views: std::collections::HashSet<u32>,
    was_esc_down: bool,
    was_enter_down: bool,
    created_at: std::time::Instant,
    had_focus: bool,
}

const PALETTE: [Color32; 7] = [
    Color32::from_rgb(255, 59, 48),   // Red
    Color32::from_rgb(255, 149, 0),  // Orange
    Color32::from_rgb(255, 204, 0),  // Yellow
    Color32::from_rgb(52, 199, 89),   // Green
    Color32::from_rgb(0, 122, 255),   // Blue
    Color32::from_rgb(255, 255, 255), // White
    Color32::from_rgb(0, 0, 0),       // Black
];

impl Drop for SnippingTool {
    fn drop(&mut self) {
        win32_overlay::restore_all_subclasses();
    }
}

impl SnippingTool {
    fn push_undo_snapshot(&mut self) {
        self.undo_stack.push(self.annotations.clone());
        self.redo_stack.clear();
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo_stack.pop() {
            self.redo_stack.push(std::mem::replace(&mut self.annotations, prev));
            self.selected_index = None;
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push(std::mem::replace(&mut self.annotations, next));
            self.selected_index = if self.annotations.is_empty() {
                None
            } else {
                Some(self.annotations.len() - 1)
            };
        }
    }

    fn ann_bounding_box(&self, ctx: &egui::Context, ann: &Annotation) -> Rect {
        ann.bounding_box_with_ctx(Some(ctx))
    }

    fn ann_hit_test(&self, ctx: &egui::Context, ann: &Annotation, pos: Pos2) -> bool {
        ann.hit_test_with_ctx(pos, Some(ctx))
    }

    fn find_hit_annotation(&self, ctx: &egui::Context, pos: Pos2) -> Option<usize> {
        self.annotations.iter().enumerate().rev()
            .find(|(_, ann)| self.ann_hit_test(ctx, ann, pos))
            .map(|(idx, _)| idx)
    }

    /// Captures full-resolution screenshots of all monitors up front (freeze-frame).
    pub fn new(monitors: &[MonitorInfo], annotate_enabled: bool) -> Result<Self> {
        Self::with_precaptured(monitors, annotate_enabled, None)
    }

    /// Creates a snipping session, optionally using pre-captured monitor images
    /// taken at the exact instant the global hotkey was pressed.
    pub fn with_precaptured(
        monitors: &[MonitorInfo],
        annotate_enabled: bool,
        precaptured: Option<&[(u32, xcap::image::RgbaImage)]>,
    ) -> Result<Self> {
        if monitors.is_empty() {
            anyhow::bail!("no monitors available for snapshot");
        }
        win32_overlay::reset_signals();
        let mut views = Vec::new();
        for m in monitors {
            let raw = if let Some(items) = precaptured
                && let Some((_, img)) = items.iter().find(|(id, _)| *id == m.id)
            {
                img.clone()
            } else {
                screenshot_monitor_image(m.id)?
            };
            views.push(MonitorView { info: m.clone(), raw, tex: None });
        }
        Ok(Self {
            views,
            selection: None,
            drag: DragState::None,
            tool: Tool::Select,
            color: Color32::from_rgb(255, 59, 48), // Default bright Red
            stroke_width: 4.0,
            font_size: 22.0,
            annotations: Vec::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            selected_index: None,
            text_input: None,
            annotate_enabled,
            outcome: None,
            toolbar_rect: None,
            configured_views: std::collections::HashSet::new(),
            was_esc_down: false,
            was_enter_down: false,
            created_at: std::time::Instant::now(),
            had_focus: false,
        })
    }

    /// Explicitly cancel snipping session (used by repeated hotkey or UI cancel).
    pub fn cancel(&mut self) {
        self.outcome = Some(SnippingOutcome::Cancelled);
    }

    /// Renders overlays on all monitors and processes events.
    pub fn show(&mut self, ctx: &egui::Context) -> SnippingOutcome {
        if let Some(outcome) = self.outcome.take() {
            return outcome;
        }

        // Track focus
        if win32_overlay::had_focus() || ctx.input(|i| i.focused) {
            self.had_focus = true;
        }

        // Deactivation check: cancel when focus is lost (WM_ACTIVATE with WA_INACTIVE),
        // but ONLY after window had focus, not earlier than 300 ms after creation,
        // and NOT while editing text, pointer is down, dragging, or hovering toolbar.
        let lost_focus = win32_overlay::check_and_reset_deactivated();
        if lost_focus {
            let elapsed = self.created_at.elapsed();
            let is_editing_text = self.text_input.is_some();
            let is_pointer_down = ctx.input(|i| i.pointer.any_down());
            let is_dragging = !matches!(self.drag, DragState::None);

            let pointer_pos = ctx.input(|inp| inp.pointer.hover_pos().or_else(|| inp.pointer.interact_pos()));
            let is_in_toolbar = self.toolbar_rect.is_some_and(|r| pointer_pos.is_some_and(|p| r.contains(p)));

            if elapsed >= std::time::Duration::from_millis(300)
                && self.had_focus
                && !is_editing_text
                && !is_pointer_down
                && !is_dragging
                && !is_in_toolbar
            {
                log::info!("[Snipping] Overlay deactivated after loss of focus, cancelling");
                return SnippingOutcome::Cancelled;
            }
        }
        #[cfg(windows)]
        let (raw_esc, raw_enter) = {
            use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_RETURN};
            let esc = is_key_just_pressed(VK_ESCAPE, &mut self.was_esc_down);
            let enter = is_key_just_pressed(VK_RETURN, &mut self.was_enter_down);
            (esc, enter)
        };
        #[cfg(not(windows))]
        let (raw_esc, raw_enter) = (false, false);

        // 1. Guaranteed Esc handling (Win32 GetAsyncKeyState hardware key + egui)
        let esc_pressed = raw_esc
            || ctx.input(|inp| inp.key_pressed(Key::Escape))
            || ctx.input_mut(|inp| inp.consume_key(Modifiers::NONE, Key::Escape));

        if esc_pressed {
            if self.text_input.is_some() {
                self.text_input = None;
            } else {
                return SnippingOutcome::Cancelled;
            }
        }

        // 2. Delete / Backspace key handling to delete selected annotation
        if self.text_input.is_none()
            && ctx.input_mut(|inp| {
                inp.consume_key(Modifiers::NONE, Key::Delete) || inp.consume_key(Modifiers::NONE, Key::Backspace)
            })
            && let Some(idx) = self.selected_index.take()
            && idx < self.annotations.len()
        {
            self.push_undo_snapshot();
            self.annotations.remove(idx);
        }

        // 3. Other hotkeys (Enter via Win32 hardware key + egui, Ctrl+C, Ctrl+Z, Ctrl+Y / Ctrl+Shift+Z)
        let (egui_enter, copy_hotkey, undo_hotkey, redo_hotkey) = ctx.input(|inp| {
            let enter = inp.key_pressed(Key::Enter);
            let copy = inp.modifiers.matches_exact(Modifiers::CTRL) && inp.key_pressed(Key::C);
            let undo = inp.modifiers.matches_exact(Modifiers::CTRL) && inp.key_pressed(Key::Z);
            let redo = (inp.modifiers.matches_exact(Modifiers::CTRL) && inp.key_pressed(Key::Y))
                || ((inp.modifiers.ctrl && inp.modifiers.shift) && inp.key_pressed(Key::Z));
            (enter, copy, undo, redo)
        });
        let enter = raw_enter || egui_enter;

        if undo_hotkey {
            self.undo();
        }
        if redo_hotkey {
            self.redo();
        }

        // Ensure selected_index is within bounds
        if let Some(idx) = self.selected_index
            && idx >= self.annotations.len()
        {
            self.selected_index = None;
        }

        if let Some((m_idx, sel)) = self.selection {
            if enter && self.text_input.is_none() {
                return self.finalize(m_idx, sel, true);
            }
            if copy_hotkey && self.text_input.is_none() {
                return self.finalize(m_idx, sel, false);
            }
        }

        for i in 0..self.views.len() {
            let info = self.views[i].info.clone();
            let scale = info.scale_factor.max(0.1);
            let id = ViewportId::from_hash_of(("openclip-snip", info.id));
            let expected_pos = Pos2::new(info.x as f32 / scale, info.y as f32 / scale);
            let expected_size = egui::vec2(info.width as f32 / scale, info.height as f32 / scale);
            let title = format!("OpenClip Snipping Tool #{}", info.id);
            let builder = ViewportBuilder::default()
                .with_title(&title)
                .with_decorations(false)
                .with_resizable(false)
                .with_taskbar(false)
                .with_always_on_top()
                .with_active(true)
                .with_position(expected_pos)
                .with_inner_size(expected_size);

            ctx.show_viewport_immediate(id, builder, |ctx, _class| {
                if !self.configured_views.contains(&info.id)
                    && win32_overlay::setup_overlay_window(&title, &info)
                {
                    self.configured_views.insert(info.id);
                }

                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .inner_margin(0)
                            .outer_margin(0)
                            .stroke(Stroke::NONE)
                            .fill(Color32::from_gray(20)),
                    )
                    .show(ctx, |ui| {
                        self.draw_monitor(ui, i, expected_size);
                    });
            });
        }

        self.outcome.take().unwrap_or(SnippingOutcome::Pending)
    }

    fn draw_monitor(&mut self, ui: &mut egui::Ui, i: usize, expected_size: Vec2) {
        let ctx = ui.ctx().clone();
        let full = ui.max_rect().union(Rect::from_min_size(Pos2::ZERO, expected_size));
        let ppp = ctx.pixels_per_point();

        // Ensure texture is loaded
        if self.views[i].tex.is_none() {
            let raw = &self.views[i].raw;
            let color = ColorImage::from_rgba_unmultiplied(
                [raw.width() as usize, raw.height() as usize],
                raw.as_raw(),
            );
            self.views[i].tex = Some(ctx.load_texture(format!("snip-mon-{i}"), color, TextureOptions::LINEAR));
        }

        let painter = ui.painter().clone();
        let uv_full = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));

        // 1. Draw base freeze-frame
        if let Some(tex) = &self.views[i].tex {
            painter.image(tex.id(), full, uv_full, Color32::WHITE);
        } else {
            painter.rect_filled(full, 0.0, Color32::from_gray(30));
        }

        // 2. Dimmed veil over the whole monitor (rgba 0, 0, 0, 120)
        painter.rect_filled(full, 0.0, Color32::from_rgba_unmultiplied(0, 0, 0, 120));

        let current_sel = match self.selection {
            Some((m, s)) if m == i => Some(s),
            _ => None,
        };

        let pointer_pos = ctx.input(|inp| inp.pointer.hover_pos().or_else(|| inp.pointer.interact_pos()));

        // In Move/Select mode, check handles and hover
        let mut hovered_ann_handle: Option<ResizeHandle> = None;
        let mut hovered_line_handle: Option<bool> = None;
        let mut hovered_ann_idx: Option<usize> = None;

        if self.tool == Tool::Select {
            if let (Some(idx), Some(pos)) = (self.selected_index, pointer_pos)
                && idx < self.annotations.len()
            {
                match &self.annotations[idx] {
                    Annotation::Pixelate { rect, .. } => {
                        hovered_ann_handle = self.hit_test_rect_handle(*rect, pos);
                    }
                    Annotation::Rect { p1, p2, .. } => {
                        hovered_ann_handle = self.hit_test_rect_handle(Rect::from_two_pos(*p1, *p2), pos);
                    }
                    Annotation::Text { .. } => {
                        hovered_ann_handle = hit_test_text_handle(self.ann_bounding_box(&ctx, &self.annotations[idx]), pos);
                    }
                    Annotation::Arrow { p1, p2, .. } | Annotation::Line { p1, p2, .. } => {
                        hovered_line_handle = self.hit_test_endpoint_handle(*p1, *p2, pos);
                    }
                    _ => {}
                }
            }

            if hovered_ann_handle.is_none() && hovered_line_handle.is_none()
                && let Some(pos) = pointer_pos.filter(|p| current_sel.is_some_and(|sel| sel.contains(*p)))
            {
                hovered_ann_idx = self.find_hit_annotation(&ctx, pos);
            }
        }

        // Check handle hover for main crop selection
        let mut hovered_crop_handle: Option<ResizeHandle> = None;
        if hovered_ann_handle.is_none() && hovered_line_handle.is_none()
            && let (Some(sel), Some(pos)) = (current_sel, pointer_pos)
        {
            hovered_crop_handle = self.hit_test_handle(sel, pos);
        }

        // Determine cursor icon
        let mut cursor = match self.tool {
            Tool::Select => {
                if let Some(h) = hovered_ann_handle.or(hovered_crop_handle) {
                    match h {
                        ResizeHandle::TopLeft | ResizeHandle::BottomRight => CursorIcon::ResizeNorthWest,
                        ResizeHandle::TopRight | ResizeHandle::BottomLeft => CursorIcon::ResizeNorthEast,
                        ResizeHandle::Top | ResizeHandle::Bottom => CursorIcon::ResizeNorth,
                        ResizeHandle::Left | ResizeHandle::Right => CursorIcon::ResizeEast,
                    }
                } else if hovered_line_handle.is_some() {
                    CursorIcon::Crosshair
                } else if hovered_ann_idx.is_some() {
                    CursorIcon::Grab
                } else if current_sel.is_some_and(|s| pointer_pos.is_some_and(|p| s.contains(p))) {
                    CursorIcon::Move
                } else {
                    CursorIcon::Crosshair
                }
            }
            Tool::Text => CursorIcon::Text,
            _ => {
                if let Some(h) = hovered_crop_handle {
                    match h {
                        ResizeHandle::TopLeft | ResizeHandle::BottomRight => CursorIcon::ResizeNorthWest,
                        ResizeHandle::TopRight | ResizeHandle::BottomLeft => CursorIcon::ResizeNorthEast,
                        ResizeHandle::Top | ResizeHandle::Bottom => CursorIcon::ResizeNorth,
                        ResizeHandle::Left | ResizeHandle::Right => CursorIcon::ResizeEast,
                    }
                } else {
                    CursorIcon::Crosshair
                }
            }
        };

        if matches!(self.drag, DragState::MovingAnnotation { .. }) {
            cursor = CursorIcon::Grabbing;
        } else if matches!(self.drag, DragState::ResizingAnnotation { .. } | DragState::ResizingLine { .. }) {
            cursor = CursorIcon::Crosshair;
        }

        ctx.set_cursor_icon(cursor);

        // 3. Highlight selected region if present
        if let Some(sel) = current_sel {
            // Cutout un-dimmed screenshot
            if let Some(tex) = &self.views[i].tex {
                let uv = Rect::from_min_max(
                    Pos2::new(sel.min.x / full.width(), sel.min.y / full.height()),
                    Pos2::new(sel.max.x / full.width(), sel.max.y / full.height()),
                );
                painter.image(tex.id(), sel, uv, Color32::WHITE);
            }

            // Draw pixelate annotations first
            for ann in &self.annotations {
                if let Annotation::Pixelate { rect, blocks } = ann {
                    let inter = rect.intersect(sel);
                    if inter.is_positive() {
                        if blocks.is_empty() {
                            self.render_pixelate_preview(&painter, inter, i, full);
                        } else {
                            for (cell, color) in blocks {
                                let c_inter = cell.intersect(sel);
                                if c_inter.is_positive() {
                                    painter.rect_filled(c_inter, 0.0, *color);
                                }
                            }
                        }
                    }
                }
            }

            // Draw vector annotations
            for ann in &self.annotations {
                if !matches!(ann, Annotation::Pixelate { .. }) {
                    self.render_annotation(&painter, ann);
                }
            }

            // Draw highlight and handles for selected_index in Move/Select mode
            if self.tool == Tool::Select {
                if let Some(idx) = self.selected_index
                    && idx < self.annotations.len()
                {
                    let ann = &self.annotations[idx];
                    match ann {
                        Annotation::Pixelate { rect, .. } => {
                            painter.rect_stroke(rect.expand(2.0), 0.0, Stroke::new(1.5, BLUE), egui::StrokeKind::Outside);
                            self.draw_rect_handles(&painter, *rect);
                        }
                        Annotation::Rect { p1, p2, .. } => {
                            let r = Rect::from_two_pos(*p1, *p2);
                            painter.rect_stroke(r.expand(2.0), 0.0, Stroke::new(1.5, BLUE), egui::StrokeKind::Outside);
                            self.draw_rect_handles(&painter, r);
                        }
                        Annotation::Text { .. } => {
                            let bbox = self.ann_bounding_box(&ctx, ann);
                            painter.rect_stroke(bbox.expand(1.5), 0.0, Stroke::new(1.5, BLUE), egui::StrokeKind::Outside);
                            self.draw_text_handles(&painter, bbox);
                        }
                        Annotation::Arrow { p1, p2, .. } | Annotation::Line { p1, p2, .. } => {
                            self.draw_endpoint_handles(&painter, *p1, *p2);
                        }
                        _ => {
                            let bbox = self.ann_bounding_box(&ctx, ann);
                            painter.rect_stroke(
                                bbox.expand(4.0),
                                4.0,
                                Stroke::new(1.5, BLUE),
                                egui::StrokeKind::Outside,
                            );
                        }
                    }
                }

                // Highlight hovered annotation with light bounding box if not already selected
                if let Some(ann) = hovered_ann_idx.filter(|idx| Some(*idx) != self.selected_index).and_then(|idx| self.annotations.get(idx)) {
                    let bbox = self.ann_bounding_box(&ctx, ann);
                    painter.rect_stroke(
                        bbox.expand(3.0),
                        3.0,
                        Stroke::new(1.0, Color32::from_white_alpha(160)),
                        egui::StrokeKind::Outside,
                    );
                }
            }

            // Selection border & dimension tag
            painter.rect_stroke(sel, 0.0, Stroke::new(2.0, BLUE), egui::StrokeKind::Outside);

            let phys = to_physical(sel, full, ppp, &self.views[i].info);
            let tag = format!("{} × {}", phys.width, phys.height);
            let tag_pos = Pos2::new(sel.min.x, (sel.min.y - 24.0).max(full.min.y + 4.0));
            let tag_rect = Rect::from_min_size(tag_pos, egui::vec2(tag.len() as f32 * 8.5 + 16.0, 22.0));
            painter.rect_filled(tag_rect, 6.0, CARD);
            painter.text(
                tag_pos + egui::vec2(8.0, 3.0),
                Align2::LEFT_TOP,
                tag,
                FontId::monospace(13.0),
                Color32::WHITE,
            );

            // Draw 8 resize handles for selection
            self.draw_handles(&painter, sel);
        }

        // Draw active drag draft
        match &self.drag {
            DragState::Selecting { start, current } => {
                let draft_sel = Rect::from_two_pos(*start, *current).intersect(full);
                if draft_sel.is_positive() {
                    if let Some(tex) = &self.views[i].tex {
                        let uv = Rect::from_min_max(
                            Pos2::new(draft_sel.min.x / full.width(), draft_sel.min.y / full.height()),
                            Pos2::new(draft_sel.max.x / full.width(), draft_sel.max.y / full.height()),
                        );
                        painter.image(tex.id(), draft_sel, uv, Color32::WHITE);
                    }
                    painter.rect_stroke(draft_sel, 0.0, Stroke::new(2.0, BLUE), egui::StrokeKind::Outside);
                    let phys = to_physical(draft_sel, full, ppp, &self.views[i].info);
                    let tag = format!("{} × {}", phys.width, phys.height);
                    let tag_pos = Pos2::new(draft_sel.min.x, (draft_sel.min.y - 24.0).max(full.min.y + 4.0));
                    let tag_rect = Rect::from_min_size(tag_pos, egui::vec2(tag.len() as f32 * 8.5 + 16.0, 22.0));
                    painter.rect_filled(tag_rect, 6.0, CARD);
                    painter.text(
                        tag_pos + egui::vec2(8.0, 3.0),
                        Align2::LEFT_TOP,
                        tag,
                        FontId::monospace(13.0),
                        Color32::WHITE,
                    );
                }
            }
            DragState::Drawing { start, current, pen_points } => {
                if let Some(draft_ann) = self.build_draft_annotation(*start, *current, pen_points) {
                    match &draft_ann {
                        Annotation::Pixelate { rect, .. } => {
                            let inter = if let Some(sel) = current_sel {
                                rect.intersect(sel)
                            } else {
                                *rect
                            };
                            if inter.is_positive() {
                                self.render_pixelate_preview(&painter, inter, i, full);
                                painter.rect_stroke(
                                    inter,
                                    0.0,
                                    Stroke::new(2.0, BLUE),
                                    egui::StrokeKind::Outside,
                                );
                                painter.rect_stroke(
                                    inter,
                                    0.0,
                                    Stroke::new(1.0, Color32::WHITE),
                                    egui::StrokeKind::Inside,
                                );
                            }
                        }
                        _ => {
                            self.render_annotation(&painter, &draft_ann);
                        }
                    }
                }
            }
            _ => {}
        }

        // Interactive mouse handling
        let resp = ui.interact(full, egui::Id::new("snip_canvas"), Sense::click_and_drag());

        if resp.dragged() || self.drag != DragState::None {
            ctx.request_repaint();
        }

        // RMB or double click in Move/Select mode for text editing
        let rmb_clicked = resp.secondary_clicked() || ctx.input(|inp| inp.pointer.button_clicked(PointerButton::Secondary));
        if (rmb_clicked || resp.double_clicked()) && self.tool == Tool::Select
            && let Some(pos) = pointer_pos
            && let Some(idx) = self.find_hit_annotation(&ctx, pos)
        {
            if let Annotation::Text { pos: tpos, text: ttext, color: tcolor, font_size: tfont_size, custom_width: tcustom_width } = &self.annotations[idx] {
                let state = TextInputState {
                    pos: *tpos,
                    buffer: ttext.clone(),
                    font_size: *tfont_size,
                    color: *tcolor,
                    custom_width: *tcustom_width,
                };
                self.push_undo_snapshot();
                self.annotations.remove(idx);
                self.text_input = Some(state);
                self.selected_index = None;
            } else {
                self.selected_index = Some(idx);
            }
        }

        if resp.drag_started() {
            let press_origin = ctx.input(|inp| inp.pointer.press_origin())
                .or_else(|| resp.interact_pointer_pos())
                .unwrap_or(Pos2::ZERO);

            let in_toolbar = self.toolbar_rect.is_some_and(|r| r.contains(press_origin));

            if !in_toolbar {
                if let Some(sel) = current_sel {
                    if self.tool == Tool::Select {
                        // Strict priority:
                        // 1. Handles of selected annotation
                        let hit_handle = if let Some(idx) = self.selected_index.filter(|&i| i < self.annotations.len()) {
                            match &self.annotations[idx] {
                                Annotation::Pixelate { rect, .. } => self.hit_test_rect_handle(*rect, press_origin).map(|h| (idx, h)),
                                Annotation::Rect { p1, p2, .. } => self.hit_test_rect_handle(Rect::from_two_pos(*p1, *p2), press_origin).map(|h| (idx, h)),
                                Annotation::Text { .. } => hit_test_text_handle(self.ann_bounding_box(&ctx, &self.annotations[idx]), press_origin).map(|h| (idx, h)),
                                _ => None,
                            }
                        } else {
                            None
                        };

                        let hit_line_handle = if let Some(idx) = self.selected_index.filter(|&i| i < self.annotations.len()) {
                            match &self.annotations[idx] {
                                Annotation::Arrow { p1, p2, .. } | Annotation::Line { p1, p2, .. } => {
                                    self.hit_test_endpoint_handle(*p1, *p2, press_origin).map(|is_p2| (idx, is_p2))
                                }
                                _ => None,
                            }
                        } else {
                            None
                        };

                        if let Some((idx, handle)) = hit_handle {
                            let (r, sfs) = match &self.annotations[idx] {
                                Annotation::Pixelate { rect, .. } => (*rect, 0.0),
                                Annotation::Rect { p1, p2, .. } => (Rect::from_two_pos(*p1, *p2), 0.0),
                                Annotation::Text { font_size, .. } => (self.ann_bounding_box(&ctx, &self.annotations[idx]), *font_size),
                                _ => (Rect::ZERO, 0.0),
                            };
                            self.drag = DragState::ResizingAnnotation {
                                index: idx,
                                handle,
                                start_rect: r,
                                start_pos: press_origin,
                                start_font_size: sfs,
                                start_annotations: self.annotations.clone(),
                            };
                        } else if let Some((idx, is_p2)) = hit_line_handle {
                            self.drag = DragState::ResizingLine {
                                index: idx,
                                is_p2,
                                start_annotations: self.annotations.clone(),
                            };
                        // 2. Selected annotation body
                        } else if let Some(idx) = self.selected_index.filter(|&i| i < self.annotations.len()).filter(|&i| self.ann_hit_test(&ctx, &self.annotations[i], press_origin)) {
                            self.drag = DragState::MovingAnnotation {
                                index: idx,
                                last_pos: press_origin,
                                start_annotations: self.annotations.clone(),
                            };
                        // 3. Other annotations (unselected)
                        } else if let Some(idx) = self.find_hit_annotation(&ctx, press_origin) {
                            self.selected_index = Some(idx);
                            self.drag = DragState::MovingAnnotation {
                                index: idx,
                                last_pos: press_origin,
                                start_annotations: self.annotations.clone(),
                            };
                        // 4. Crop handles
                        } else if let Some(handle) = self.hit_test_handle(sel, press_origin) {
                            self.drag = DragState::Resizing { handle, start_rect: sel, start_pos: press_origin };
                        // 5. Inside main crop
                        } else if sel.contains(press_origin) {
                            self.selected_index = None;
                            self.drag = DragState::Moving {
                                start_rect: sel,
                                start_pos: press_origin,
                                start_annotations: self.annotations.clone(),
                            };
                        // 6. Outside main crop
                        } else {
                            self.selected_index = None;
                            self.selection = None;
                            self.annotations.clear();
                            self.undo_stack.clear();
                            self.redo_stack.clear();
                            self.drag = DragState::Selecting { start: press_origin, current: press_origin };
                        }
                    } else if self.tool != Tool::Text {
                        // Drawing tools
                        if let Some(handle) = self.hit_test_handle(sel, press_origin) {
                            self.drag = DragState::Resizing { handle, start_rect: sel, start_pos: press_origin };
                        } else if sel.contains(press_origin) {
                            self.drag = DragState::Drawing {
                                start: press_origin,
                                current: press_origin,
                                pen_points: vec![press_origin],
                            };
                        } else {
                            self.selection = None;
                            self.annotations.clear();
                            self.undo_stack.clear();
                            self.redo_stack.clear();
                            self.drag = DragState::Selecting { start: press_origin, current: press_origin };
                        }
                    } else {
                        // Text tool: dragging outside selection starts new selection
                        if !sel.contains(press_origin) {
                            self.selection = None;
                            self.annotations.clear();
                            self.undo_stack.clear();
                            self.redo_stack.clear();
                            self.drag = DragState::Selecting { start: press_origin, current: press_origin };
                        }
                    }
                } else {
                    self.selected_index = None;
                    self.drag = DragState::Selecting { start: press_origin, current: press_origin };
                }
            }
        }

        if let Some(cur) = resp.interact_pointer_pos().filter(|_| resp.dragged()) {
            match &mut self.drag {
                DragState::None => {
                    if current_sel.is_none() {
                        let origin = ctx.input(|inp| inp.pointer.press_origin()).unwrap_or(cur);
                        self.drag = DragState::Selecting { start: origin, current: cur };
                    }
                }
                DragState::MovingAnnotation { index, last_pos, .. } => {
                    let delta = cur - *last_pos;
                    if let Some(ann) = self.annotations.get_mut(*index) {
                        ann.translate(delta);
                    }
                    *last_pos = cur;
                }
                DragState::ResizingAnnotation { index, handle, start_rect, start_pos, start_font_size, .. } => {
                    let delta = cur - *start_pos;
                    let mut r = *start_rect;
                    match handle {
                        ResizeHandle::TopLeft => { r.min.x += delta.x; r.min.y += delta.y; }
                        ResizeHandle::Top => { r.min.y += delta.y; }
                        ResizeHandle::TopRight => { r.max.x += delta.x; r.min.y += delta.y; }
                        ResizeHandle::Right => { r.max.x += delta.x; }
                        ResizeHandle::BottomRight => { r.max.x += delta.x; r.max.y += delta.y; }
                        ResizeHandle::Bottom => { r.max.y += delta.y; }
                        ResizeHandle::BottomLeft => { r.min.x += delta.x; r.max.y += delta.y; }
                        ResizeHandle::Left => { r.min.x += delta.x; }
                    }
                    let norm = Rect::from_two_pos(
                        Pos2::new(r.min.x.min(r.max.x), r.min.y.min(r.max.y)),
                        Pos2::new(r.min.x.max(r.max.x), r.min.y.max(r.max.y)),
                    );
                    if let Some(ann) = self.annotations.get_mut(*index) {
                        match ann {
                            Annotation::Pixelate { rect, blocks } => {
                                *rect = norm;
                                blocks.clear();
                            }
                            Annotation::Rect { p1, p2, .. } => {
                                *p1 = norm.min;
                                *p2 = norm.max;
                            }
                            Annotation::Text { pos, font_size, custom_width, text, .. } => {
                                let (new_pos, new_fs, new_cw) = resize_text_handle_pure(
                                    *handle,
                                    *start_rect,
                                    cur,
                                    *start_font_size,
                                    text,
                                    *custom_width,
                                );
                                *pos = new_pos;
                                *font_size = new_fs;
                                *custom_width = new_cw;
                            }
                            _ => {}
                        }
                    }
                    if let Some(Annotation::Text { font_size, .. }) = self.annotations.get(*index)
                        && matches!(handle, ResizeHandle::TopLeft | ResizeHandle::TopRight | ResizeHandle::BottomLeft | ResizeHandle::BottomRight)
                    {
                        self.font_size = *font_size;
                    }
                }
                DragState::ResizingLine { index, is_p2, .. } => {
                    if let Some(Annotation::Arrow { p1, p2, .. } | Annotation::Line { p1, p2, .. }) = self.annotations.get_mut(*index) {
                        if *is_p2 {
                            *p2 = cur;
                        } else {
                            *p1 = cur;
                        }
                    }
                }
                DragState::Selecting { current, .. } => {
                    *current = cur;
                }
                DragState::Moving { start_rect, start_pos, start_annotations } => {
                    let delta = cur - *start_pos;
                    let moved = start_rect.translate(delta);
                    self.selection = Some((i, moved.intersect(full)));
                    self.annotations = start_annotations.clone();
                    for ann in &mut self.annotations {
                        ann.translate(delta);
                    }
                }
                DragState::Resizing { handle, start_rect, start_pos } => {
                    let delta = cur - *start_pos;
                    let mut r = *start_rect;
                    match handle {
                        ResizeHandle::TopLeft => { r.min.x += delta.x; r.min.y += delta.y; }
                        ResizeHandle::Top => { r.min.y += delta.y; }
                        ResizeHandle::TopRight => { r.max.x += delta.x; r.min.y += delta.y; }
                        ResizeHandle::Right => { r.max.x += delta.x; }
                        ResizeHandle::BottomRight => { r.max.x += delta.x; r.max.y += delta.y; }
                        ResizeHandle::Bottom => { r.max.y += delta.y; }
                        ResizeHandle::BottomLeft => { r.min.x += delta.x; r.max.y += delta.y; }
                        ResizeHandle::Left => { r.min.x += delta.x; }
                    }
                    let norm = Rect::from_two_pos(
                        Pos2::new(r.min.x.min(r.max.x), r.min.y.min(r.max.y)),
                        Pos2::new(r.min.x.max(r.max.x), r.min.y.max(r.max.y)),
                    );
                    self.selection = Some((i, norm.intersect(full)));
                }
                DragState::Drawing { current, pen_points, .. } => {
                    *current = cur;
                    if self.tool == Tool::Pen && pen_points.last().is_none_or(|p| p.distance(cur) > 2.0) {
                        pen_points.push(cur);
                    }
                }
            }
        }

        if resp.drag_stopped() {
            match std::mem::replace(&mut self.drag, DragState::None) {
                DragState::MovingAnnotation { index, start_annotations, .. } => {
                    let target_rect = match self.annotations.get(index) {
                        Some(Annotation::Pixelate { rect, .. }) => Some(*rect),
                        _ => None,
                    };
                    if let Some(r) = target_rect {
                        let blocks = self.compute_pixelate_blocks(r, i, full);
                        if let Some(Annotation::Pixelate { blocks: b, .. }) = self.annotations.get_mut(index) {
                            *b = blocks;
                        }
                    }
                    if self.annotations != start_annotations {
                        self.undo_stack.push(start_annotations);
                        self.redo_stack.clear();
                    }
                }
                DragState::ResizingAnnotation { index, start_annotations, .. } => {
                    let target_rect = match self.annotations.get(index) {
                        Some(Annotation::Pixelate { rect, .. }) => Some(*rect),
                        _ => None,
                    };
                    if let Some(r) = target_rect {
                        let blocks = self.compute_pixelate_blocks(r, i, full);
                        if let Some(Annotation::Pixelate { blocks: b, .. }) = self.annotations.get_mut(index) {
                            *b = blocks;
                        }
                    }
                    if self.annotations != start_annotations {
                        self.undo_stack.push(start_annotations);
                        self.redo_stack.clear();
                    }
                }
                DragState::ResizingLine { start_annotations, .. } => {
                    if self.annotations != start_annotations {
                        self.undo_stack.push(start_annotations);
                        self.redo_stack.clear();
                    }
                }
                DragState::Moving { start_annotations, .. } => {
                    if self.annotations != start_annotations {
                        self.undo_stack.push(start_annotations);
                        self.redo_stack.clear();
                    }
                }
                DragState::Selecting { start, current } => {
                    let sel = Rect::from_two_pos(start, current).intersect(full);
                    if sel.width() >= 16.0 && sel.height() >= 16.0 {
                        self.selection = Some((i, sel));
                        if !self.annotate_enabled {
                            // If annotation disabled, user can immediately confirm
                        }
                    }
                }
                DragState::Drawing { start, current, pen_points } => {
                    if let Some(mut ann) = self.build_draft_annotation(start, current, &pen_points) {
                        let valid = match &ann {
                            Annotation::Rect { p1, p2, .. } | Annotation::Marker { p1, p2, .. } | Annotation::Line { p1, p2, .. } => {
                                p1.distance(*p2) > 4.0
                            }
                            Annotation::Arrow { p1, p2, .. } => p1.distance(*p2) > 6.0,
                            Annotation::Pen { points, .. } => points.len() >= 2 && points.windows(2).map(|w| w[0].distance(w[1])).sum::<f32>() > 4.0,
                            Annotation::Pixelate { rect, .. } => rect.width() > 4.0 && rect.height() > 4.0,
                            Annotation::Text { .. } => true,
                        };
                        if valid {
                            if let Annotation::Pixelate { rect, ref mut blocks } = ann {
                                *blocks = self.compute_pixelate_blocks(rect, i, full);
                            }
                            self.push_undo_snapshot();
                            self.annotations.push(ann);
                            self.selected_index = Some(self.annotations.len() - 1);
                        }
                    }
                }
                _ => {}
            }
        }

        // Single click (when not dragging)
        if resp.clicked()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            if self.tool == Tool::Select {
                let hit = self.find_hit_annotation(&ctx, pos);
                if let Some(idx) = hit {
                    self.selected_index = Some(idx);
                } else if current_sel.is_some_and(|sel| sel.contains(pos)) {
                    self.selected_index = None;
                }
            } else if self.tool == Tool::Text && current_sel.is_some_and(|sel| sel.contains(pos)) {
                self.text_input = Some(TextInputState {
                    pos,
                    buffer: String::new(),
                    font_size: self.font_size,
                    color: self.color,
                    custom_width: None,
                });
            }
        }

        // Handle active text editing popup
        if let Some(mut state) = self.text_input.take() {
            let mut close_text = false;
            let pad_x = 8.0;
            let pad_y = 5.0;
            let wrap_w = state.custom_width.map(|w| w.max(40.0)).unwrap_or(f32::INFINITY);
            let (w, h) = ctx.fonts_mut(|fonts| {
                let sz = fonts.layout(state.buffer.clone(), FontId::proportional(state.font_size), Color32::WHITE, wrap_w).size();
                (sz.x.max(40.0), sz.y.max(state.font_size))
            });
            let text_rect = Rect::from_min_size(
                state.pos - egui::vec2(pad_x, pad_y),
                egui::vec2(w + pad_x * 2.0, h + pad_y * 2.0),
            );

            ui.scope_builder(UiBuilder::new().max_rect(text_rect), |ui| {
                ui.painter().rect_filled(text_rect, 4.0, Color32::from_black_alpha(180));
                ui.painter().rect_stroke(text_rect, 4.0, Stroke::new(1.5, BLUE), egui::StrokeKind::Outside);

                let enter_finish = ui.input_mut(|inp| {
                    if !inp.modifiers.shift && inp.key_pressed(Key::Enter) {
                        inp.consume_key(Modifiers::NONE, Key::Enter);
                        true
                    } else {
                        false
                    }
                });

                let text_edit = egui::TextEdit::multiline(&mut state.buffer)
                    .font(FontId::proportional(state.font_size))
                    .text_color(state.color)
                    .frame(egui::Frame::NONE)
                    .margin(Margin::symmetric(pad_x as i8, pad_y as i8));

                let text_resp = ui.add(text_edit);
                text_resp.request_focus();

                if enter_finish || text_resp.lost_focus() {
                    if state.buffer.ends_with('\n') {
                        state.buffer.pop();
                    }
                    if !state.buffer.trim().is_empty() {
                        self.push_undo_snapshot();
                        self.annotations.push(Annotation::Text {
                            pos: state.pos,
                            text: state.buffer.clone(),
                            color: state.color,
                            font_size: state.font_size,
                            custom_width: state.custom_width,
                        });
                        self.selected_index = Some(self.annotations.len() - 1);
                    }
                    close_text = true;
                }
            });
            if !close_text {
                self.text_input = Some(state);
            }
        }

        // Draw hint if no selection
        if self.selection.is_none() && !matches!(self.drag, DragState::Selecting { .. }) {
            painter.text(
                full.center(),
                Align2::CENTER_CENTER,
                crate::t!(SNIP_HINT),
                FontId::proportional(22.0),
                Color32::from_white_alpha(230),
            );
        }

        // 4. Render floating annotation toolbar
        if let Some(sel) = current_sel {
            self.draw_toolbar(ui, i, sel, full);
        } else {
            self.toolbar_rect = None;
        }

        ctx.request_repaint();
    }

    fn draw_handles(&self, painter: &egui::Painter, sel: Rect) {
        let size = 8.0;
        let stroke = Stroke::new(1.5, BLUE);
        let fill = Color32::WHITE;

        let points = [
            sel.left_top(),
            Pos2::new(sel.center().x, sel.top()),
            sel.right_top(),
            Pos2::new(sel.right(), sel.center().y),
            sel.right_bottom(),
            Pos2::new(sel.center().x, sel.bottom()),
            sel.left_bottom(),
            Pos2::new(sel.left(), sel.center().y),
        ];

        for p in points {
            let r = Rect::from_center_size(p, Vec2::splat(size));
            painter.rect(r, 2.0, fill, stroke, egui::StrokeKind::Outside);
        }
    }

    fn hit_test_handle(&self, sel: Rect, pos: Pos2) -> Option<ResizeHandle> {
        let tol = 12.0;
        let check = |center: Pos2| pos.distance(center) <= tol;

        if check(sel.left_top()) { return Some(ResizeHandle::TopLeft); }
        if check(sel.right_top()) { return Some(ResizeHandle::TopRight); }
        if check(sel.right_bottom()) { return Some(ResizeHandle::BottomRight); }
        if check(sel.left_bottom()) { return Some(ResizeHandle::BottomLeft); }
        if check(Pos2::new(sel.center().x, sel.top())) { return Some(ResizeHandle::Top); }
        if check(Pos2::new(sel.right(), sel.center().y)) { return Some(ResizeHandle::Right); }
        if check(Pos2::new(sel.center().x, sel.bottom())) { return Some(ResizeHandle::Bottom); }
        if check(Pos2::new(sel.left(), sel.center().y)) { return Some(ResizeHandle::Left); }

        None
    }

    fn draw_rect_handles(&self, painter: &egui::Painter, sel: Rect) {
        let size = 7.0;
        let stroke = Stroke::new(1.0, BLUE);
        let fill = Color32::WHITE;

        let points = [
            sel.left_top(),
            Pos2::new(sel.center().x, sel.top()),
            sel.right_top(),
            Pos2::new(sel.right(), sel.center().y),
            sel.right_bottom(),
            Pos2::new(sel.center().x, sel.bottom()),
            sel.left_bottom(),
            Pos2::new(sel.left(), sel.center().y),
        ];

        for p in points {
            let r = Rect::from_center_size(p, Vec2::splat(size));
            painter.rect(r, 1.5, fill, stroke, egui::StrokeKind::Outside);
        }
    }

    fn draw_text_handles(&self, painter: &egui::Painter, sel: Rect) {
        let size = 7.0;
        let stroke = Stroke::new(1.0, BLUE);
        let fill = Color32::WHITE;

        let points = [
            sel.left_top(),
            sel.right_top(),
            sel.right_bottom(),
            sel.left_bottom(),
            Pos2::new(sel.right(), sel.center().y),
            Pos2::new(sel.left(), sel.center().y),
        ];

        for p in points {
            let r = Rect::from_center_size(p, Vec2::splat(size));
            painter.rect(r, 1.5, fill, stroke, egui::StrokeKind::Outside);
        }
    }

    fn hit_test_rect_handle(&self, sel: Rect, pos: Pos2) -> Option<ResizeHandle> {
        hit_test_rect_handle_pure(sel, pos)
    }

    fn draw_endpoint_handles(&self, painter: &egui::Painter, p1: Pos2, p2: Pos2) {
        let stroke = Stroke::new(1.5, BLUE);
        let fill = Color32::WHITE;
        for p in [p1, p2] {
            let r = Rect::from_center_size(p, Vec2::splat(8.0));
            painter.rect(r, 2.0, fill, stroke, egui::StrokeKind::Outside);
        }
    }

    fn hit_test_endpoint_handle(&self, p1: Pos2, p2: Pos2, pos: Pos2) -> Option<bool> {
        if pos.distance(p2) <= 12.0 {
            Some(true)
        } else if pos.distance(p1) <= 12.0 {
            Some(false)
        } else {
            None
        }
    }

    fn build_draft_annotation(&self, start: Pos2, current: Pos2, pen_points: &[Pos2]) -> Option<Annotation> {
        match self.tool {
            Tool::Select => None,
            Tool::Rect => Some(Annotation::Rect { p1: start, p2: current, color: self.color, width: self.stroke_width }),
            Tool::Arrow => Some(Annotation::Arrow { p1: start, p2: current, color: self.color, width: self.stroke_width }),
            Tool::Line => Some(Annotation::Line { p1: start, p2: current, color: self.color, width: self.stroke_width }),
            Tool::Pen => Some(Annotation::Pen { points: pen_points.to_vec(), color: self.color, width: self.stroke_width }),
            Tool::Marker => Some(Annotation::Marker { p1: start, p2: current, color: self.color, width: self.stroke_width }),
            Tool::Pixelate => Some(Annotation::Pixelate { rect: Rect::from_two_pos(start, current), blocks: Vec::new() }),
            Tool::Text => None,
        }
    }

    fn render_annotation(&self, painter: &egui::Painter, ann: &Annotation) {
        match ann {
            Annotation::Rect { p1, p2, color, width } => {
                painter.rect_stroke(Rect::from_two_pos(*p1, *p2), 0.0, Stroke::new(*width, *color), egui::StrokeKind::Outside);
            }
            Annotation::Line { p1, p2, color, width } => {
                painter.line_segment([*p1, *p2], Stroke::new(*width, *color));
            }
            Annotation::Arrow { p1, p2, color, width } => {
                painter.line_segment([*p1, *p2], Stroke::new(*width, *color));
                let dir = *p2 - *p1;
                let len = dir.length();
                if len > 2.0 {
                    let norm_dir = dir / len;
                    let normal = Vec2::new(-norm_dir.y, norm_dir.x);
                    let h_len = (*width * 3.5).clamp(14.0, 36.0);
                    let back = *p2 - norm_dir * h_len;
                    let left = back + normal * (h_len * 0.45);
                    let right = back - normal * (h_len * 0.45);
                    painter.add(egui::Shape::convex_polygon(vec![*p2, left, right], *color, Stroke::NONE));
                }
            }
            Annotation::Pen { points, color, width } => {
                for pair in points.windows(2) {
                    painter.line_segment([pair[0], pair[1]], Stroke::new(*width, *color));
                }
            }
            Annotation::Marker { p1, p2, color, width } => {
                let mut c = *color;
                c = Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 90);
                painter.line_segment([*p1, *p2], Stroke::new(*width * 3.5, c));
            }
            Annotation::Text { pos, text, color, font_size, custom_width } => {
                let wrap_w = custom_width.map(|w| w.max(40.0)).unwrap_or(f32::INFINITY);
                let galley = painter.layout(text.clone(), FontId::proportional(*font_size), *color, wrap_w);
                let text_size = galley.size();
                let pad_x = 8.0;
                let pad_y = 5.0;
                let pill_rect = Rect::from_min_size(*pos - egui::vec2(pad_x, pad_y), text_size + egui::vec2(pad_x * 2.0, pad_y * 2.0));
                painter.rect_filled(pill_rect, 4.0, Color32::from_black_alpha(180));
                painter.galley(*pos, galley, *color);
            }
            Annotation::Pixelate { blocks, .. } => {
                for (cell, color) in blocks {
                    painter.rect_filled(*cell, 0.0, *color);
                }
            }
        }
    }

    fn compute_pixelate_blocks(&self, rect: Rect, monitor_idx: usize, full: Rect) -> Vec<(Rect, Color32)> {
        let mut blocks = Vec::new();
        let block_pt = 10.0;
        let raw = &self.views[monitor_idx].raw;
        let full_w = full.width().max(1.0);
        let full_h = full.height().max(1.0);
        let raw_w = raw.width();
        let raw_h = raw.height();

        let pt_to_px_x = |x: f32| -> u32 {
            (((x - full.min.x) / full_w) * raw_w as f32).round().clamp(0.0, (raw_w.saturating_sub(1)) as f32) as u32
        };
        let pt_to_px_y = |y: f32| -> u32 {
            (((y - full.min.y) / full_h) * raw_h as f32).round().clamp(0.0, (raw_h.saturating_sub(1)) as f32) as u32
        };

        let start_y = (rect.min.y as i32).max(full.min.y as i32);
        let end_y = (rect.max.y as i32).min(full.max.y as i32);
        let start_x = (rect.min.x as i32).max(full.min.x as i32);
        let end_x = (rect.max.x as i32).min(full.max.x as i32);

        for by in (start_y..end_y).step_by(block_pt as usize) {
            for bx in (start_x..end_x).step_by(block_pt as usize) {
                let cell = Rect::from_min_max(
                    Pos2::new(bx as f32, by as f32),
                    Pos2::new((bx as f32 + block_pt).min(rect.max.x), (by as f32 + block_pt).min(rect.max.y)),
                );
                if cell.is_positive() {
                    let px0 = pt_to_px_x(cell.min.x);
                    let px1 = (pt_to_px_x(cell.max.x) + 1).min(raw_w);
                    let py0 = pt_to_px_y(cell.min.y);
                    let py1 = (pt_to_px_y(cell.max.y) + 1).min(raw_h);

                    let mut r_sum = 0u64;
                    let mut g_sum = 0u64;
                    let mut b_sum = 0u64;
                    let mut count = 0u64;

                    let step_x = ((px1.saturating_sub(px0)) / 8).max(1);
                    let step_y = ((py1.saturating_sub(py0)) / 8).max(1);

                    for py in (py0..py1).step_by(step_y as usize) {
                        for px in (px0..px1).step_by(step_x as usize) {
                            let p = raw.get_pixel(px, py);
                            r_sum += p[0] as u64;
                            g_sum += p[1] as u64;
                            b_sum += p[2] as u64;
                            count += 1;
                        }
                    }

                    if let (Some(r), Some(g), Some(b)) = (
                        r_sum.checked_div(count),
                        g_sum.checked_div(count),
                        b_sum.checked_div(count),
                    ) {
                        let avg = Color32::from_rgb(r as u8, g as u8, b as u8);
                        blocks.push((cell, avg));
                    }
                }
            }
        }
        blocks
    }

    fn render_pixelate_preview(&self, painter: &egui::Painter, rect: Rect, monitor_idx: usize, full: Rect) {
        let blocks = self.compute_pixelate_blocks(rect, monitor_idx, full);
        for (cell, color) in blocks {
            painter.rect_filled(cell, 0.0, color);
        }
    }

    fn draw_toolbar(&mut self, ui: &mut egui::Ui, monitor_idx: usize, sel: Rect, full: Rect) {
        let tb_h = 44.0;

        // Vertical positioning:
        // Default below selection; if reaching bottom or near fullscreen, place inside at the bottom
        let mut y = sel.max.y + 12.0;
        if y + tb_h + 24.0 > full.max.y {
            y = full.max.y - tb_h - 24.0;
        }
        let max_y = (full.max.y - tb_h - 12.0).max(full.min.y + 12.0);
        y = y.clamp(full.min.y + 12.0, max_y);

        let mut do_save = false;
        let mut do_copy = false;
        let mut do_cancel = false;

        let area_res = egui::Area::new(egui::Id::new("snip_floating_toolbar"))
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, y))
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::new()
                    .fill(CARD)
                    .corner_radius(CornerRadius::same(10))
                    .stroke(Stroke::new(1.0, Color32::from_gray(65)))
                    .inner_margin(Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        ui.spacing_mut().button_padding = egui::vec2(5.0, 4.0);

                        ui.horizontal(|ui| {
                            if self.annotate_enabled {
                                // Tool buttons
                                let tools = [
                                    (Tool::Select, icons::ARROWS_ALT, crate::t!(SNIP_TOOL_MOVE)),
                                    (Tool::Rect, icons::SQUARE, crate::t!(SNIP_TOOL_RECT)),
                                    (Tool::Arrow, icons::ARROW_RIGHT, crate::t!(SNIP_TOOL_ARROW)),
                                    (Tool::Line, icons::MINUS, crate::t!(SNIP_TOOL_LINE)),
                                    (Tool::Pen, icons::PEN, crate::t!(SNIP_TOOL_PEN)),
                                    (Tool::Marker, icons::HIGHLIGHTER, crate::t!(SNIP_TOOL_MARKER)),
                                    (Tool::Text, icons::FONT, crate::t!(SNIP_TOOL_TEXT)),
                                    (Tool::Pixelate, icons::EYE_SLASH, crate::t!(SNIP_TOOL_PIXELATE)),
                                ];

                                for (tool, icon, tip) in tools {
                                    let active = self.tool == tool;
                                    let text = RichText::new(icon).size(15.0).color(if active { BLUE } else { Color32::WHITE });
                                    if ui.button(text).on_hover_text(tip).clicked() {
                                        self.tool = tool;
                                        if tool != Tool::Select {
                                            self.selected_index = None;
                                        }
                                    }
                                }

                                ui.separator();

                                // Palette
                                for &c in &PALETTE {
                                    let is_sel = self.color == c;
                                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::click());
                                    ui.painter().circle_filled(rect.center(), 7.0, c);
                                    if is_sel {
                                        ui.painter().circle_stroke(rect.center(), 8.5, Stroke::new(2.0, Color32::WHITE));
                                    }
                                    if resp.clicked() {
                                        self.color = c;
                                        if let Some(idx) = self.selected_index
                                            && let Some(ann) = self.annotations.get(idx)
                                        {
                                            let current_c = match ann {
                                                Annotation::Rect { color, .. }
                                                | Annotation::Arrow { color, .. }
                                                | Annotation::Line { color, .. }
                                                | Annotation::Pen { color, .. }
                                                | Annotation::Marker { color, .. }
                                                | Annotation::Text { color, .. } => Some(*color),
                                                Annotation::Pixelate { .. } => None,
                                            };
                                            if let Some(cur) = current_c && cur != c {
                                                self.push_undo_snapshot();
                                                match &mut self.annotations[idx] {
                                                    Annotation::Rect { color, .. }
                                                    | Annotation::Arrow { color, .. }
                                                    | Annotation::Line { color, .. }
                                                    | Annotation::Pen { color, .. }
                                                    | Annotation::Marker { color, .. }
                                                    | Annotation::Text { color, .. } => {
                                                        *color = c;
                                                    }
                                                    Annotation::Pixelate { .. } => {}
                                                }
                                            }
                                        }
                                    }
                                }

                                ui.separator();

                                let is_text_mode = self.tool == Tool::Text
                                    || self.selected_index.is_some_and(|idx| matches!(self.annotations.get(idx), Some(Annotation::Text { .. })));

                                if is_text_mode {
                                    let smaller_text = RichText::new("A−").size(12.0).color(Color32::WHITE);
                                    if ui.button(smaller_text).on_hover_text(crate::t!(SNIP_TOOL_TEXT_SMALLER)).clicked() {
                                        let new_fs = (self.font_size - 4.0).clamp(12.0, 96.0);
                                        if let Some(idx) = self.selected_index
                                            && let Some(Annotation::Text { font_size, .. }) = self.annotations.get_mut(idx)
                                            && *font_size != new_fs
                                        {
                                            self.push_undo_snapshot();
                                            if let Some(Annotation::Text { font_size, .. }) = self.annotations.get_mut(idx) {
                                                *font_size = new_fs;
                                            }
                                        }
                                        self.font_size = new_fs;
                                    }

                                    let cur_fs = if let Some(idx) = self.selected_index {
                                        if let Some(Annotation::Text { font_size, .. }) = self.annotations.get(idx) {
                                            *font_size
                                        } else {
                                            self.font_size
                                        }
                                    } else {
                                        self.font_size
                                    };
                                    ui.label(RichText::new(format!("{}pt", cur_fs as u32)).size(11.0).color(BLUE));

                                    let larger_text = RichText::new("A+").size(12.0).color(Color32::WHITE);
                                    if ui.button(larger_text).on_hover_text(crate::t!(SNIP_TOOL_TEXT_LARGER)).clicked() {
                                        let new_fs = (self.font_size + 4.0).clamp(12.0, 96.0);
                                        if let Some(idx) = self.selected_index
                                            && let Some(Annotation::Text { font_size, .. }) = self.annotations.get_mut(idx)
                                            && *font_size != new_fs
                                        {
                                            self.push_undo_snapshot();
                                            if let Some(Annotation::Text { font_size, .. }) = self.annotations.get_mut(idx) {
                                                *font_size = new_fs;
                                            }
                                        }
                                        self.font_size = new_fs;
                                    }
                                } else {
                                    // Stroke width
                                    let widths = [2.0f32, 4.0, 8.0];
                                    for &w in &widths {
                                        let is_w = (self.stroke_width - w).abs() < 0.5;
                                        let label = format!("{}px", w as u32);
                                        let text = RichText::new(label).size(11.0).color(if is_w { BLUE } else { Color32::from_gray(160) });
                                        if ui.button(text).clicked() {
                                            self.stroke_width = w;
                                            if let Some(idx) = self.selected_index
                                                && let Some(
                                                    Annotation::Rect { width, .. }
                                                    | Annotation::Arrow { width, .. }
                                                    | Annotation::Line { width, .. }
                                                    | Annotation::Pen { width, .. }
                                                    | Annotation::Marker { width, .. }
                                                ) = self.annotations.get_mut(idx)
                                                && *width != w
                                            {
                                                self.push_undo_snapshot();
                                                if let Some(
                                                    Annotation::Rect { width, .. }
                                                    | Annotation::Arrow { width, .. }
                                                    | Annotation::Line { width, .. }
                                                    | Annotation::Pen { width, .. }
                                                    | Annotation::Marker { width, .. }
                                                ) = self.annotations.get_mut(idx) {
                                                    *width = w;
                                                }
                                            }
                                        }
                                    }
                                }

                                ui.separator();

                                // Undo
                                let can_undo = !self.undo_stack.is_empty();
                                let undo_text = RichText::new(icons::UNDO).size(14.0).color(if can_undo { Color32::WHITE } else { Color32::from_gray(80) });
                                if ui.add_enabled(can_undo, egui::Button::new(undo_text)).on_hover_text(crate::t!(SNIP_TOOL_UNDO)).clicked() {
                                    self.undo();
                                }

                                // Redo
                                let can_redo = !self.redo_stack.is_empty();
                                let redo_text = RichText::new(icons::REDO).size(14.0).color(if can_redo { Color32::WHITE } else { Color32::from_gray(80) });
                                if ui.add_enabled(can_redo, egui::Button::new(redo_text)).on_hover_text(crate::t!(SNIP_TOOL_REDO)).clicked() {
                                    self.redo();
                                }

                                // Trash (Delete selected annotation)
                                let can_delete = self.selected_index.is_some_and(|idx| idx < self.annotations.len());
                                let trash_text = RichText::new(icons::TRASH).size(14.0).color(if can_delete { RED } else { Color32::from_gray(80) });
                                if ui.add_enabled(can_delete, egui::Button::new(trash_text)).on_hover_text(crate::t!(SNIP_TOOL_DELETE)).clicked()
                                    && let Some(idx) = self.selected_index.take()
                                    && idx < self.annotations.len()
                                {
                                    self.push_undo_snapshot();
                                    self.annotations.remove(idx);
                                }

                                ui.separator();
                            }

                            // Actions: Cancel / Copy / Save
                            let cancel_btn = RichText::new(icons::XMARK).size(15.0).color(RED);
                            if ui.button(cancel_btn).on_hover_text(crate::t!(SNIP_TOOL_CANCEL)).clicked() {
                                do_cancel = true;
                            }

                            let copy_btn = RichText::new(icons::COPY).size(14.0).color(Color32::WHITE);
                            if ui.button(copy_btn).on_hover_text(crate::t!(SNIP_TOOL_COPY)).clicked() {
                                do_copy = true;
                            }

                            let save_btn = RichText::new(icons::CHECK).size(15.0).color(Color32::WHITE);
                            let save_response = ui.add(egui::Button::new(save_btn).fill(GREEN)).on_hover_text(crate::t!(SNIP_TOOL_SAVE));
                            if save_response.clicked() {
                                do_save = true;
                            }
                        });
                    });
            });

        self.toolbar_rect = Some(area_res.response.rect);

        if do_cancel {
            self.outcome = Some(SnippingOutcome::Cancelled);
        } else if do_copy {
            self.outcome = Some(self.finalize(monitor_idx, sel, false));
        } else if do_save {
            self.outcome = Some(self.finalize(monitor_idx, sel, true));
        }
    }

    fn finalize(&self, monitor_idx: usize, sel_points: Rect, save: bool) -> SnippingOutcome {
        let view = &self.views[monitor_idx];
        let full_points = Rect::from_min_size(
            Pos2::ZERO,
            egui::vec2(view.info.width as f32 / view.info.scale_factor, view.info.height as f32 / view.info.scale_factor),
        );
        let phys = to_physical(sel_points, full_points, view.info.scale_factor, &view.info);

        if phys.width == 0 || phys.height == 0 {
            return SnippingOutcome::Cancelled;
        }

        // Crop the high-resolution raw image
        let x0 = phys.x.min(view.raw.width().saturating_sub(1));
        let y0 = phys.y.min(view.raw.height().saturating_sub(1));
        let w = phys.width.min(view.raw.width() - x0);
        let h = phys.height.min(view.raw.height() - y0);

        let cropped = xcap::image::imageops::crop_imm(&view.raw, x0, y0, w, h).to_image();
        let baked = bake_annotations(cropped, sel_points, &self.annotations, w as f32 / sel_points.width(), h as f32 / sel_points.height());

        if save {
            SnippingOutcome::Saved(baked)
        } else {
            SnippingOutcome::CopiedOnly(baked)
        }
    }
}

pub fn hit_test_rect_handle_pure(sel: Rect, pos: Pos2) -> Option<ResizeHandle> {
    let tol = 12.0;
    let check = |center: Pos2| pos.distance(center) <= tol;

    if check(sel.left_top()) { return Some(ResizeHandle::TopLeft); }
    if check(sel.right_top()) { return Some(ResizeHandle::TopRight); }
    if check(sel.right_bottom()) { return Some(ResizeHandle::BottomRight); }
    if check(sel.left_bottom()) { return Some(ResizeHandle::BottomLeft); }
    if check(Pos2::new(sel.center().x, sel.top())) { return Some(ResizeHandle::Top); }
    if check(Pos2::new(sel.right(), sel.center().y)) { return Some(ResizeHandle::Right); }
    if check(Pos2::new(sel.center().x, sel.bottom())) { return Some(ResizeHandle::Bottom); }
    if check(Pos2::new(sel.left(), sel.center().y)) { return Some(ResizeHandle::Left); }

    None
}

/// Converts a selection in viewport points to monitor-local physical pixels.
pub fn to_physical(sel: Rect, full: Rect, _ppp: f32, info: &MonitorInfo) -> PhysRect {
    if sel.width() <= 0.0 || sel.height() <= 0.0 {
        return PhysRect { x: 0, y: 0, width: 0, height: 0 };
    }
    let snap_threshold_pt = 3.5;

    let sx = info.width as f32 / full.width().max(1.0);
    let sy = info.height as f32 / full.height().max(1.0);

    let rel_min_x = sel.min.x - full.min.x;
    let rel_min_y = sel.min.y - full.min.y;
    let rel_max_x = sel.max.x - full.min.x;
    let rel_max_y = sel.max.y - full.min.y;

    let snapped_min_x = if rel_min_x <= snap_threshold_pt { 0.0 } else { rel_min_x };
    let snapped_min_y = if rel_min_y <= snap_threshold_pt { 0.0 } else { rel_min_y };
    let snapped_max_x = if (full.width() - rel_max_x) <= snap_threshold_pt {
        full.width()
    } else {
        rel_max_x
    };
    let snapped_max_y = if (full.height() - rel_max_y) <= snap_threshold_pt {
        full.height()
    } else {
        rel_max_y
    };

    let x0 = (snapped_min_x * sx).round().clamp(0.0, info.width as f32) as u32;
    let y0 = (snapped_min_y * sy).round().clamp(0.0, info.height as f32) as u32;
    let x1 = (snapped_max_x * sx).round().clamp(0.0, info.width as f32) as u32;
    let y1 = (snapped_max_y * sy).round().clamp(0.0, info.height as f32) as u32;

    let w = x1.saturating_sub(x0);
    let h = y1.saturating_sub(y0);
    PhysRect { x: x0, y: y0, width: w, height: h }
}

/// Bakes vector and blur annotations directly onto the high-resolution cropped image.
fn bake_annotations(
    mut base: xcap::image::RgbaImage,
    sel_points: Rect,
    annotations: &[Annotation],
    scale_x: f32,
    scale_y: f32,
) -> xcap::image::RgbaImage {
    let scale_avg = (scale_x + scale_y) / 2.0;

    let to_img = |p: Pos2| -> (f32, f32) {
        (
            (p.x - sel_points.min.x) * scale_x,
            (p.y - sel_points.min.y) * scale_y,
        )
    };

    // 1. Apply pixelation first so drawn shapes sit on top of blurred areas
    for ann in annotations {
        if let Annotation::Pixelate { rect, .. } = ann {
            let p0 = to_img(rect.min);
            let p1 = to_img(rect.max);
            let x0 = (p0.0.round() as u32).min(base.width());
            let y0 = (p0.1.round() as u32).min(base.height());
            let x1 = (p1.0.round() as u32).min(base.width());
            let y1 = (p1.1.round() as u32).min(base.height());
            let block_size = ((10.0 * scale_avg).round() as u32).max(6);
            apply_pixelate(&mut base, x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1), block_size);
        }
    }

    // 2. Draw vector annotations
    for ann in annotations {
        match ann {
            Annotation::Rect { p1, p2, color, width } => {
                let p0 = to_img(*p1);
                let p1 = to_img(*p2);
                let thick = *width * scale_avg;
                let c = [color.r(), color.g(), color.b(), color.a()];
                draw_line_segment(&mut base, p0.0, p0.1, p1.0, p0.1, c, thick);
                draw_line_segment(&mut base, p1.0, p0.1, p1.0, p1.1, c, thick);
                draw_line_segment(&mut base, p1.0, p1.1, p0.0, p1.1, c, thick);
                draw_line_segment(&mut base, p0.0, p1.1, p0.0, p0.1, c, thick);
            }
            Annotation::Line { p1, p2, color, width } => {
                let p0 = to_img(*p1);
                let p1 = to_img(*p2);
                let thick = *width * scale_avg;
                draw_line_segment(&mut base, p0.0, p0.1, p1.0, p1.1, [color.r(), color.g(), color.b(), color.a()], thick);
            }
            Annotation::Arrow { p1, p2, color, width } => {
                let p0 = to_img(*p1);
                let p1 = to_img(*p2);
                let thick = *width * scale_avg;
                let c = [color.r(), color.g(), color.b(), color.a()];
                draw_line_segment(&mut base, p0.0, p0.1, p1.0, p1.1, c, thick);

                let dx = p1.0 - p0.0;
                let dy = p1.1 - p0.1;
                let len = (dx * dx + dy * dy).sqrt();
                if len > 2.0 {
                    let ux = dx / len;
                    let uy = dy / len;
                    let nx = -uy;
                    let ny = ux;
                    let h_len = (thick * 3.5).clamp(16.0, 48.0);
                    let h_w = h_len * 0.45;
                    let bx = p1.0 - ux * h_len;
                    let by = p1.1 - uy * h_len;
                    let lx = bx + nx * h_w;
                    let ly = by + ny * h_w;
                    let rx = bx - nx * h_w;
                    let ry = by - ny * h_w;
                    fill_triangle(&mut base, (p1.0, p1.1), (lx, ly), (rx, ry), c);
                }
            }
            Annotation::Pen { points, color, width } => {
                let thick = *width * scale_avg;
                let c = [color.r(), color.g(), color.b(), color.a()];
                for pair in points.windows(2) {
                    let p0 = to_img(pair[0]);
                    let p1 = to_img(pair[1]);
                    draw_line_segment(&mut base, p0.0, p0.1, p1.0, p1.1, c, thick);
                }
            }
            Annotation::Marker { p1, p2, color, width } => {
                let p0 = to_img(*p1);
                let p1 = to_img(*p2);
                let thick = *width * scale_avg * 3.5;
                let c = [color.r(), color.g(), color.b(), 90]; // Semi-transparent
                draw_line_segment(&mut base, p0.0, p0.1, p1.0, p1.1, c, thick);
            }
            Annotation::Text { pos, text, color, font_size, custom_width } => {
                if let Some(renderer) = TextRenderer::new() {
                    let p = to_img(*pos);
                    let px_size = (*font_size * scale_avg).round().clamp(12.0, 96.0);
                    let line_h = px_size * 1.3;
                    let scaled_mw = custom_width.map(|w| w * scale_avg);
                    let lines = wrap_text(text, scaled_mw, px_size, &renderer);

                    let mut max_w = 0.0f32;
                    for l in &lines {
                        max_w = max_w.max(renderer.width(l, px_size));
                    }
                    if let Some(mw) = scaled_mw {
                        max_w = max_w.max(mw.max(40.0 * scale_avg));
                    }
                    let total_h = (lines.len() as f32 * line_h).max(line_h);

                    let pad_x = 8.0 * scale_avg;
                    let pad_y = 5.0 * scale_avg;
                    let sprite_w = ((max_w + pad_x * 2.0).ceil() as u32).max(10);
                    let sprite_h = ((total_h + pad_y * 2.0).ceil() as u32).max(10);

                    let mut sprite = Sprite::new(sprite_w, sprite_h);
                    // Draw translucent background pill
                    for sy in 0..sprite.height {
                        for sx in 0..sprite.width {
                            sprite.put(sx as i32, sy as i32, [18, 18, 18], 0.75);
                        }
                    }
                    let baseline = renderer.baseline(px_size, line_h);
                    for (idx, line) in lines.iter().enumerate() {
                        let y = pad_y + idx as f32 * line_h + baseline;
                        renderer.draw(&mut sprite, line, px_size, (pad_x, y), [color.r(), color.g(), color.b()], 1.0);
                    }

                    // Blend sprite onto image
                    let ox = (p.0 - pad_x).round() as i32;
                    let oy = (p.1 - pad_y).round() as i32;
                    for sy in 0..sprite.height {
                        for sx in 0..sprite.width {
                            let dx = ox + sx as i32;
                            let dy = oy + sy as i32;
                            if dx >= 0 && dy >= 0 && (dx as u32) < base.width() && (dy as u32) < base.height() {
                                let si = (sy as usize * sprite.width as usize + sx as usize) * 4;
                                let sa = sprite.rgba[si + 3] as f32 / 255.0;
                                if sa > 0.0 {
                                    let pixel = base.get_pixel_mut(dx as u32, dy as u32);
                                    for c_idx in 0..3 {
                                        pixel[c_idx] = (sprite.rgba[si + c_idx] as f32 * sa + pixel[c_idx] as f32 * (1.0 - sa)).round() as u8;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Annotation::Pixelate { .. } => {}
        }
    }

    base
}

fn apply_pixelate(img: &mut xcap::image::RgbaImage, x0: u32, y0: u32, x1: u32, y1: u32, block: u32) {
    let (w, h) = (img.width(), img.height());
    let x0 = x0.min(w);
    let y0 = y0.min(h);
    let x1 = x1.min(w);
    let y1 = y1.min(h);

    for by in (y0..y1).step_by(block as usize) {
        for bx in (x0..x1).step_by(block as usize) {
            let bw = block.min(x1.saturating_sub(bx));
            let bh = block.min(y1.saturating_sub(by));
            if bw == 0 || bh == 0 { continue; }

            let mut r_sum = 0u64;
            let mut g_sum = 0u64;
            let mut b_sum = 0u64;
            let mut count = 0u64;

            for py in by..by + bh {
                for px in bx..bx + bw {
                    let p = img.get_pixel(px, py);
                    r_sum += p[0] as u64;
                    g_sum += p[1] as u64;
                    b_sum += p[2] as u64;
                    count += 1;
                }
            }

            if let (Some(r), Some(g), Some(b)) = (
                r_sum.checked_div(count),
                g_sum.checked_div(count),
                b_sum.checked_div(count),
            ) {
                let avg = xcap::image::Rgba([r as u8, g as u8, b as u8, 255]);
                for py in by..by + bh {
                    for px in bx..bx + bw {
                        img.put_pixel(px, py, avg);
                    }
                }
            }
        }
    }
}

fn draw_line_segment(
    img: &mut xcap::image::RgbaImage,
    x0: f32, y0: f32,
    x1: f32, y1: f32,
    color: [u8; 4],
    thick: f32,
) {
    let radius = (thick / 2.0).max(1.0);
    let min_x = (x0.min(x1) - radius - 1.0).max(0.0) as u32;
    let max_x = ((x0.max(x1) + radius + 1.0) as u32).min(img.width().saturating_sub(1));
    let min_y = (y0.min(y1) - radius - 1.0).max(0.0) as u32;
    let max_y = ((y0.max(y1) + radius + 1.0) as u32).min(img.height().saturating_sub(1));

    let vx = x1 - x0;
    let vy = y1 - y0;
    let len_sq = vx * vx + vy * vy;

    for py in min_y..=max_y {
        for px in min_x..=max_x {
            let cx = px as f32 + 0.5;
            let cy = py as f32 + 0.5;
            let ux = cx - x0;
            let uy = cy - y0;

            let dist = if len_sq <= 0.001 {
                (ux * ux + uy * uy).sqrt()
            } else {
                let t = ((ux * vx + uy * vy) / len_sq).clamp(0.0, 1.0);
                let proj_x = x0 + t * vx;
                let proj_y = y0 + t * vy;
                let dx = cx - proj_x;
                let dy = cy - proj_y;
                (dx * dx + dy * dy).sqrt()
            };

            if dist <= radius {
                let edge_alpha = (radius + 0.5 - dist).clamp(0.0, 1.0);
                let alpha = (color[3] as f32 / 255.0) * edge_alpha;
                blend_pixel(img, px, py, [color[0], color[1], color[2]], alpha);
            }
        }
    }
}

fn fill_triangle(
    img: &mut xcap::image::RgbaImage,
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    color: [u8; 4],
) {
    let min_x = p0.0.min(p1.0).min(p2.0).max(0.0) as u32;
    let max_x = (p0.0.max(p1.0).max(p2.0) as u32).min(img.width().saturating_sub(1));
    let min_y = p0.1.min(p1.1).min(p2.1).max(0.0) as u32;
    let max_y = (p0.1.max(p1.1).max(p2.1) as u32).min(img.height().saturating_sub(1));

    let sign = |p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)| -> f32 {
        (p1.0 - p3.0) * (p2.1 - p3.1) - (p2.0 - p3.0) * (p1.1 - p3.1)
    };

    let alpha = color[3] as f32 / 255.0;

    for py in min_y..=max_y {
        for px in min_x..=max_x {
            let pt = (px as f32 + 0.5, py as f32 + 0.5);
            let d1 = sign(pt, p0, p1);
            let d2 = sign(pt, p1, p2);
            let d3 = sign(pt, p2, p0);

            let has_neg = (d1 < 0.0) || (d2 < 0.0) || (d3 < 0.0);
            let has_pos = (d1 > 0.0) || (d2 > 0.0) || (d3 > 0.0);

            if !(has_neg && has_pos) {
                blend_pixel(img, px, py, [color[0], color[1], color[2]], alpha);
            }
        }
    }
}

fn blend_pixel(img: &mut xcap::image::RgbaImage, x: u32, y: u32, rgb: [u8; 3], alpha: f32) {
    if x >= img.width() || y >= img.height() || alpha <= 0.0 {
        return;
    }
    let p = img.get_pixel_mut(x, y);
    let a = alpha.clamp(0.0, 1.0);
    p[0] = (rgb[0] as f32 * a + p[0] as f32 * (1.0 - a)).round() as u8;
    p[1] = (rgb[1] as f32 * a + p[1] as f32 * (1.0 - a)).round() as u8;
    p[2] = (rgb[2] as f32 * a + p[2] as f32 * (1.0 - a)).round() as u8;
    p[3] = 255;
}

/// Copies an RGBA image buffer to the Windows Clipboard.
pub fn copy_image_to_clipboard(img: &xcap::image::RgbaImage) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new().context("access system clipboard")?;
    clipboard.set_image(arboard::ImageData {
        width: img.width() as usize,
        height: img.height() as usize,
        bytes: std::borrow::Cow::Borrowed(img.as_raw()),
    }).context("write image to clipboard")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hit_test_rect_handles() {
        let r = Rect::from_min_max(Pos2::new(100.0, 100.0), Pos2::new(300.0, 200.0));

        // Corner handles within 12pt tolerance
        assert_eq!(hit_test_rect_handle_pure(r, Pos2::new(100.0, 100.0)), Some(ResizeHandle::TopLeft));
        assert_eq!(hit_test_rect_handle_pure(r, Pos2::new(305.0, 95.0)), Some(ResizeHandle::TopRight));
        assert_eq!(hit_test_rect_handle_pure(r, Pos2::new(295.0, 205.0)), Some(ResizeHandle::BottomRight));
        assert_eq!(hit_test_rect_handle_pure(r, Pos2::new(96.0, 196.0)), Some(ResizeHandle::BottomLeft));

        // Edge handles
        assert_eq!(hit_test_rect_handle_pure(r, Pos2::new(200.0, 103.0)), Some(ResizeHandle::Top));
        assert_eq!(hit_test_rect_handle_pure(r, Pos2::new(298.0, 150.0)), Some(ResizeHandle::Right));
        assert_eq!(hit_test_rect_handle_pure(r, Pos2::new(200.0, 197.0)), Some(ResizeHandle::Bottom));
        assert_eq!(hit_test_rect_handle_pure(r, Pos2::new(102.0, 150.0)), Some(ResizeHandle::Left));

        // Far outside
        assert_eq!(hit_test_rect_handle_pure(r, Pos2::new(200.0, 150.0)), None);
        assert_eq!(hit_test_rect_handle_pure(r, Pos2::new(50.0, 50.0)), None);
    }

    #[test]
    fn test_resize_rect_handles() {
        let start = Rect::from_min_max(Pos2::new(100.0, 100.0), Pos2::new(200.0, 200.0));
        let delta = Vec2::new(20.0, 30.0);

        // Resize from BottomRight
        let mut r = start;
        r.max.x += delta.x;
        r.max.y += delta.y;
        let norm = Rect::from_two_pos(
            Pos2::new(r.min.x.min(r.max.x), r.min.y.min(r.max.y)),
            Pos2::new(r.min.x.max(r.max.x), r.min.y.max(r.max.y)),
        );
        assert_eq!(norm, Rect::from_min_max(Pos2::new(100.0, 100.0), Pos2::new(220.0, 230.0)));

        // Resize from TopLeft inverted
        let mut r2 = start;
        r2.min.x += 150.0;
        r2.min.y += 150.0;
        let norm2 = Rect::from_two_pos(
            Pos2::new(r2.min.x.min(r2.max.x), r2.min.y.min(r2.max.y)),
            Pos2::new(r2.min.x.max(r2.max.x), r2.min.y.max(r2.max.y)),
        );
        assert_eq!(norm2, Rect::from_min_max(Pos2::new(200.0, 200.0), Pos2::new(250.0, 250.0)));
    }

    #[test]
    fn test_translate_rect_and_pixelate() {
        let delta = Vec2::new(15.0, -25.0);

        let mut rect_ann = Annotation::Rect {
            p1: Pos2::new(10.0, 20.0),
            p2: Pos2::new(60.0, 80.0),
            color: Color32::RED,
            width: 2.0,
        };
        rect_ann.translate(delta);
        if let Annotation::Rect { p1, p2, .. } = rect_ann {
            assert_eq!(p1, Pos2::new(25.0, -5.0));
            assert_eq!(p2, Pos2::new(75.0, 55.0));
        } else {
            panic!("expected rect");
        }

        let mut pix_ann = Annotation::Pixelate {
            rect: Rect::from_min_max(Pos2::new(50.0, 50.0), Pos2::new(150.0, 150.0)),
            blocks: vec![
                (Rect::from_min_max(Pos2::new(50.0, 50.0), Pos2::new(60.0, 60.0)), Color32::WHITE),
            ],
        };
        pix_ann.translate(delta);
        if let Annotation::Pixelate { rect, blocks } = pix_ann {
            assert_eq!(rect, Rect::from_min_max(Pos2::new(65.0, 25.0), Pos2::new(165.0, 125.0)));
            assert_eq!(blocks[0].0, Rect::from_min_max(Pos2::new(65.0, 25.0), Pos2::new(75.0, 35.0)));
        } else {
            panic!("expected pixelate");
        }
    }

    #[test]
    fn test_to_physical_fullscreen() {
        let mon_125 = MonitorInfo {
            id: 1,
            name: "Q24B36X".into(),
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            scale_factor: 1.25,
            is_primary: true,
        };
        let full_125 = Rect::from_min_size(Pos2::ZERO, Vec2::new(2048.0, 1152.0));

        // 1. Exact full screen selection
        let sel_full = Rect::from_min_size(Pos2::ZERO, Vec2::new(2048.0, 1152.0));
        let phys = to_physical(sel_full, full_125, 1.25, &mon_125);
        assert_eq!(phys.x, 0);
        assert_eq!(phys.y, 0);
        assert_eq!(phys.width, 2560);
        assert_eq!(phys.height, 1440);

        // 2. Cursor clamped by Windows at bottom-right edge (e.g. 2047.2 x 1150.4 pt)
        let sel_clamped = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(2047.2, 1150.4));
        let phys_clamped = to_physical(sel_clamped, full_125, 1.25, &mon_125);
        assert_eq!(phys_clamped.x, 0);
        assert_eq!(phys_clamped.y, 0);
        assert_eq!(phys_clamped.width, 2560);
        assert_eq!(phys_clamped.height, 1440);

        // 3. 100% scale monitor (1920x1080)
        let mon_100 = MonitorInfo {
            id: 2,
            name: "Mon100".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale_factor: 1.0,
            is_primary: false,
        };
        let full_100 = Rect::from_min_size(Pos2::ZERO, Vec2::new(1920.0, 1080.0));
        let phys_100 = to_physical(full_100, full_100, 1.0, &mon_100);
        assert_eq!(phys_100.width, 1920);
        assert_eq!(phys_100.height, 1080);

        // 4. 150% scale monitor (1920x1080 -> 1280x720 pt)
        let mon_150 = MonitorInfo {
            id: 3,
            name: "Mon150".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale_factor: 1.5,
            is_primary: false,
        };
        let full_150 = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));
        let phys_150 = to_physical(full_150, full_150, 1.5, &mon_150);
        assert_eq!(phys_150.width, 1920);
        assert_eq!(phys_150.height, 1080);
    }

    #[test]
    fn test_to_physical_corners_and_zero() {
        let mon = MonitorInfo {
            id: 1,
            name: "TestMon".into(),
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            scale_factor: 1.25,
            is_primary: true,
        };
        let full = Rect::from_min_size(Pos2::ZERO, Vec2::new(2048.0, 1152.0));

        // Zero-size selection
        let sel_zero = Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::ZERO);
        let phys_zero = to_physical(sel_zero, full, 1.25, &mon);
        assert_eq!(phys_zero.width, 0);
        assert_eq!(phys_zero.height, 0);

        // Selection starting at (0, 0)
        let sel_origin = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 80.0));
        let phys_origin = to_physical(sel_origin, full, 1.25, &mon);
        assert_eq!(phys_origin.x, 0);
        assert_eq!(phys_origin.y, 0);
        assert_eq!(phys_origin.width, 125);
        assert_eq!(phys_origin.height, 100);

        // Near top-left snapping (e.g. 1.5 pt from edge)
        let sel_near = Rect::from_min_max(Pos2::new(1.5, 1.2), Pos2::new(101.5, 81.2));
        let phys_near = to_physical(sel_near, full, 1.25, &mon);
        assert_eq!(phys_near.x, 0);
        assert_eq!(phys_near.y, 0);
    }

    #[test]
    fn test_text_word_wrap() {
        let renderer = TextRenderer::new().expect("TextRenderer available");
        let text = "Hello world this is a test of long wrapped text";
        // Wrapped with narrow max_width
        let lines = wrap_text(text, Some(100.0), 16.0, &renderer);
        assert!(lines.len() > 1, "Expected multiple lines when wrapped to 100px");
        // Manual newline preserved
        let text_multiline = "Line 1\nLine 2 is longer and will wrap\nLine 3";
        let lines_multi = wrap_text(text_multiline, Some(80.0), 16.0, &renderer);
        assert!(lines_multi.len() >= 3);
        assert_eq!(lines_multi[0], "Line 1");
    }

    #[test]
    fn test_text_resize_handles_pure() {
        let start_rect = Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(200.0, 50.0));
        let text = "Hello world test";

        // 1. Right handle: changes width, keeps font size
        let (pos_r, fs_r, cw_r) = resize_text_handle_pure(
            ResizeHandle::Right,
            start_rect,
            Pos2::new(350.0, 125.0),
            22.0,
            text,
            None,
        );
        assert_eq!(fs_r, 22.0);
        assert_eq!(pos_r, start_rect.min + Vec2::new(8.0, 5.0));
        assert!(cw_r.is_some());
        assert!(cw_r.unwrap() > 200.0);

        // 2. Left handle: changes pos.x and width, right edge stays anchored
        let (pos_l, fs_l, cw_l) = resize_text_handle_pure(
            ResizeHandle::Left,
            start_rect,
            Pos2::new(50.0, 125.0),
            22.0,
            text,
            None,
        );
        assert_eq!(fs_l, 22.0);
        assert!(pos_l.x < 108.0);
        assert!(cw_l.unwrap() > 200.0);

        // 3. BottomRight handle: scales font size up, anchor top-left stays unchanged
        let (pos_br, fs_br, _) = resize_text_handle_pure(
            ResizeHandle::BottomRight,
            start_rect,
            Pos2::new(400.0, 175.0),
            22.0,
            text,
            None,
        );
        assert!(fs_br > 22.0, "Font size should scale up when dragged outward");
        assert_eq!(pos_br, start_rect.min + Vec2::new(8.0, 5.0));

        // 4. Top/Bottom handles: disabled (no change)
        let (_, fs_top, _) = resize_text_handle_pure(
            ResizeHandle::Top,
            start_rect,
            Pos2::new(200.0, 50.0),
            22.0,
            text,
            None,
        );
        assert_eq!(fs_top, 22.0);

        // 5. Clamping bounds: font size clamped within 12..96
        let (_, fs_tiny, _) = resize_text_handle_pure(
            ResizeHandle::BottomRight,
            start_rect,
            start_rect.left_top() + Vec2::new(5.0, 5.0),
            22.0,
            text,
            None,
        );
        assert_eq!(fs_tiny, 12.0);

        let (_, fs_huge, _) = resize_text_handle_pure(
            ResizeHandle::BottomRight,
            start_rect,
            start_rect.left_top() + Vec2::new(2000.0, 2000.0),
            22.0,
            text,
            None,
        );
        assert_eq!(fs_huge, 96.0);
    }

    #[test]
    fn test_text_annotation_hit_test_and_translate_large_font() {
        let mut text_ann = Annotation::Text {
            pos: Pos2::new(100.0, 200.0),
            text: "LARGE 96PT ANNOTATION TEXT".into(),
            color: Color32::WHITE,
            font_size: 96.0,
            custom_width: None,
        };

        // 1. Hit testing at various points with font size 96
        // Near text origin / anchor
        assert!(text_ann.hit_test(Pos2::new(100.0, 200.0)));
        // Inside top-left padding
        assert!(text_ann.hit_test(Pos2::new(96.0, 196.0)));
        // In the middle of the text line
        assert!(text_ann.hit_test(Pos2::new(300.0, 240.0)));
        // Point far outside should not hit
        assert!(!text_ann.hit_test(Pos2::new(50.0, 50.0)));
        assert!(!text_ann.hit_test(Pos2::new(100.0, 400.0)));

        // 2. Also verify with egui Context fonts
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        assert!(text_ann.hit_test_with_ctx(Pos2::new(100.0, 200.0), Some(&ctx)));
        assert!(text_ann.hit_test_with_ctx(Pos2::new(300.0, 240.0), Some(&ctx)));
        assert!(!text_ann.hit_test_with_ctx(Pos2::new(50.0, 50.0), Some(&ctx)));

        // 3. Translation (moving the annotation)
        let delta = Vec2::new(150.0, -80.0);
        let orig_bbox = text_ann.bounding_box();
        text_ann.translate(delta);

        if let Annotation::Text { pos, .. } = text_ann {
            assert_eq!(pos, Pos2::new(250.0, 120.0));
        } else {
            panic!("expected text annotation");
        }

        let new_bbox = text_ann.bounding_box();
        assert_eq!(new_bbox.min, orig_bbox.min + delta);
        assert_eq!(new_bbox.max, orig_bbox.max + delta);

        // Old position no longer hits
        assert!(!text_ann.hit_test(Pos2::new(100.0, 200.0)));
        // New position hits
        assert!(text_ann.hit_test(Pos2::new(250.0, 120.0)));
        assert!(text_ann.hit_test(Pos2::new(450.0, 160.0)));

        // 4. Undo/Redo simulation of text annotation translation
        let initial = vec![Annotation::Text {
            pos: Pos2::new(100.0, 200.0),
            text: "LARGE 96PT ANNOTATION TEXT".into(),
            color: Color32::WHITE,
            font_size: 96.0,
            custom_width: None,
        }];
        let mut annotations = initial.clone();
        let mut undo_stack: Vec<Vec<Annotation>> = Vec::new();
        let mut redo_stack: Vec<Vec<Annotation>> = Vec::new();

        // Drag move starts & finishes:
        let start_annotations = annotations.clone();
        annotations[0].translate(delta);
        if annotations != start_annotations {
            undo_stack.push(start_annotations);
            redo_stack.clear();
        }
        assert_ne!(annotations, initial);

        // User hits Ctrl+Z (Undo)
        if let Some(prev) = undo_stack.pop() {
            redo_stack.push(std::mem::replace(&mut annotations, prev));
        }
        assert_eq!(annotations, initial);

        // User hits Ctrl+Y (Redo)
        if let Some(next) = redo_stack.pop() {
            undo_stack.push(std::mem::replace(&mut annotations, next));
        }
        assert_ne!(annotations, initial);
        if let Annotation::Text { pos, .. } = annotations[0] {
            assert_eq!(pos, Pos2::new(250.0, 120.0));
        }
    }
}
