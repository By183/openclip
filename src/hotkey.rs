//! Global hotkey manager for starting and stopping recordings across the OS.
//!
//! Uses Windows `RegisterHotKey` with `MOD_NOREPEAT` inside a dedicated background
//! message-loop thread. If `RegisterHotKey` fails or when games run in exclusive
//! fullscreen modes that swallow standard hotkeys, a low-level keyboard hook
//! (`WH_KEYBOARD_LL`) provides a robust fallback.
//!
//! When triggered, emits a short tone via `Beep` (different pitch for start vs stop)
//! and delivers an event to the main UI loop.

use std::fmt;
pub mod egui_map;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    pub enabled: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    /// Virtual key code (e.g. 0x52 for 'R', 0x78 for F9, 0x20 for Space).
    pub vk: u32,
    pub sound: bool,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ctrl: true,
            alt: true,
            shift: false,
            win: false,
            vk: 0x52, // 'R'
            sound: true,
        }
    }
}

impl HotkeyConfig {
    pub fn screenshot_default() -> Self {
        Self {
            enabled: true,
            ctrl: true,
            alt: true,
            shift: false,
            win: false,
            vk: 0x51, // 'Q'
            sound: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.vk == 0
    }

    /// User-facing display string (e.g. "Ctrl+Alt+R").
    pub fn display_string(&self) -> String {
        if self.vk == 0 {
            return String::new();
        }
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.win {
            parts.push("Win");
        }
        let key_name = vk_to_name(self.vk);
        parts.push(&key_name);
        parts.join("+")
    }
}

impl fmt::Display for HotkeyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_string())
    }
}

/// Converts a Windows Virtual-Key code to a clean display string.
pub fn vk_to_name(vk: u32) -> String {
    match vk {
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x13 => "Pause".into(),
        0x14 => "CapsLock".into(),
        0x1B => "Esc".into(),
        0x20 => "Space".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x2C => "PrintScreen".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x30..=0x39 => (vk as u8 as char).to_string(),
        0x41..=0x5A => (vk as u8 as char).to_string(),
        0x60..=0x69 => format!("Num{}", vk - 0x60),
        0x6A => "Num*".into(),
        0x6B => "Num+".into(),
        0x6D => "Num-".into(),
        0x6E => "Num.".into(),
        0x6F => "Num/".into(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        0x90 => "NumLock".into(),
        0x91 => "ScrollLock".into(),
        0xBA => ";".into(),
        0xBB => "=".into(),
        0xBC => ",".into(),
        0xBD => "-".into(),
        0xBE => ".".into(),
        0xBF => "/".into(),
        0xC0 => "`".into(),
        0xDB => "[".into(),
        0xDC => "\\".into(),
        0xDD => "]".into(),
        0xDE => "'".into(),
        _ => format!("Key_{vk:#04X}"),
    }
}

/// Generates a valid in-memory RIFF PCM WAV buffer for tone playback.
pub fn generate_feedback_wav(tones: &[(u32, u32)]) -> Vec<u8> {
    let sample_rate = 44100u32;
    let mut pcm_samples: Vec<i16> = Vec::new();

    for &(freq_hz, duration_ms) in tones {
        let count = ((sample_rate as u64 * duration_ms as u64) / 1000) as usize;
        if count == 0 {
            continue;
        }
        if freq_hz == 0 {
            pcm_samples.resize(pcm_samples.len() + count, 0);
        } else {
            let fade_len = (sample_rate as usize * 5 / 1000).min(count / 2); // 5 ms fade
            let amp = 14000.0f32; // ~42% peak volume to prevent clipping
            let two_pi = 2.0 * std::f32::consts::PI;
            for i in 0..count {
                let t = i as f32 / sample_rate as f32;
                let mut env = 1.0f32;
                if fade_len > 0 {
                    if i < fade_len {
                        env = i as f32 / fade_len as f32;
                    } else if i >= count - fade_len {
                        env = (count - i) as f32 / fade_len as f32;
                    }
                }
                let sample = (t * freq_hz as f32 * two_pi).sin() * amp * env;
                pcm_samples.push(sample.clamp(-32768.0, 32767.0) as i16);
            }
        }
    }

    let data_len = (pcm_samples.len() * 2) as u32;
    let file_len = 36 + data_len;
    let mut wav = Vec::with_capacity(44 + pcm_samples.len() * 2);

    // RIFF chunk
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&file_len.to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // fmt subchunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // subchunk1 size (16 for PCM)
    wav.extend_from_slice(&1u16.to_le_bytes());  // audio format (1 = PCM)
    wav.extend_from_slice(&1u16.to_le_bytes());  // 1 channel (mono)
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate * 2; // sample_rate * channels * 2 bytes
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());  // block align (channels * 2)
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    // data subchunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm_samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }

    wav
}

#[cfg(windows)]
static START_WAV: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
#[cfg(windows)]
static STOP_WAV: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

/// Plays audio feedback synchronously on the calling thread.
/// Plays via `PlaySoundW` inside the process so process loopback exclusion can filter it.
pub fn play_feedback_sound_sync(starting: bool) {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT, SND_SYNC};

        let wav = if starting {
            START_WAV.get_or_init(|| generate_feedback_wav(&[(1000, 90), (0, 30), (1500, 120)]))
        } else {
            STOP_WAV.get_or_init(|| generate_feedback_wav(&[(750, 180)]))
        };

        // SAFETY: `PCWSTR` expects a pointer to 16-bit characters, but when `SND_MEMORY` is passed,
        // the Windows Multimedia API reinterprets `pszSound` as a pointer to the in-memory RIFF WAV
        // data (`const void*`). In Rust's `windows` crate, `PCWSTR` is a wrapper around `*const u16`.
        // Rust heap allocations (`Vec<u8>`) are aligned to at least `align_of::<u8>()` (and in practice >= 8 bytes),
        // and RIFF headers only require byte alignment. The static buffer outlives the synchronous playback.
        let ok = unsafe {
            PlaySoundW(
                PCWSTR(wav.as_ptr() as *const u16),
                None,
                SND_MEMORY | SND_SYNC | SND_NODEFAULT,
            )
            .as_bool()
        };

        if !ok {
            // Kernel beep fallback if PlaySoundW fails
            use windows::Win32::System::Diagnostics::Debug::Beep;
            if starting {
                unsafe {
                    let _ = Beep(1000, 90);
                    std::thread::sleep(std::time::Duration::from_millis(30));
                    let _ = Beep(1500, 120);
                }
            } else {
                unsafe {
                    let _ = Beep(750, 180);
                }
            }
        }
    }
    #[cfg(not(windows))]
    let _ = starting;
}

/// Plays audio feedback for global recording actions asynchronously.
pub fn play_feedback_sound(starting: bool) {
    #[cfg(windows)]
    std::thread::spawn(move || {
        play_feedback_sound_sync(starting);
    });
    #[cfg(not(windows))]
    let _ = starting;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyStatus {
    Registered,
    HookFallback,
    Conflict(u32),
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Record,
    Screenshot,
}

#[cfg(windows)]
enum WorkerCmd {
    UpdateRecord(HotkeyConfig),
    UpdateScreenshot(HotkeyConfig),
    Stop,
}

pub struct HotkeyManager {
    #[cfg(windows)]
    cmd_tx: Sender<WorkerCmd>,
    #[cfg(windows)]
    worker: Option<JoinHandle<()>>,
    event_rx: Receiver<HotkeyEvent>,
    pending_record: AtomicBool,
    pending_screenshot: AtomicBool,
    record_status: Arc<std::sync::Mutex<HotkeyStatus>>,
    screenshot_status: Arc<std::sync::Mutex<HotkeyStatus>>,
}

impl HotkeyManager {
    pub fn new(initial: HotkeyConfig) -> Self {
        Self::with_screenshot(initial, HotkeyConfig::screenshot_default(), None)
    }

    pub fn with_screenshot(
        record_cfg: HotkeyConfig,
        screenshot_cfg: HotkeyConfig,
        repaint_cb: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        let (event_tx, event_rx) = channel();
        let record_status = Arc::new(std::sync::Mutex::new(HotkeyStatus::Disabled));
        let screenshot_status = Arc::new(std::sync::Mutex::new(HotkeyStatus::Disabled));

        #[cfg(windows)]
        {
            let (cmd_tx, cmd_rx) = channel();
            let rec_clone = record_status.clone();
            let sc_clone = screenshot_status.clone();
            let worker = std::thread::Builder::new()
                .name("openclip-hotkey".into())
                .spawn(move || {
                    run_hotkey_worker(
                        record_cfg,
                        screenshot_cfg,
                        cmd_rx,
                        event_tx,
                        rec_clone,
                        sc_clone,
                        repaint_cb,
                    );
                })
                .ok();

            Self {
                cmd_tx,
                worker,
                event_rx,
                pending_record: AtomicBool::new(false),
                pending_screenshot: AtomicBool::new(false),
                record_status,
                screenshot_status,
            }
        }

        #[cfg(not(windows))]
        {
            let _ = (record_cfg, screenshot_cfg, event_tx, repaint_cb);
            Self {
                event_rx,
                pending_record: AtomicBool::new(false),
                pending_screenshot: AtomicBool::new(false),
                record_status,
                screenshot_status,
            }
        }
    }

    /// Sets or updates the active recording hotkey configuration.
    pub fn update(&self, config: HotkeyConfig) {
        #[cfg(windows)]
        let _ = self.cmd_tx.send(WorkerCmd::UpdateRecord(config));
        #[cfg(not(windows))]
        let _ = config;
    }

    /// Alias for updating the recording hotkey.
    pub fn update_record(&self, config: HotkeyConfig) {
        self.update(config);
    }

    /// Sets or updates the active screenshot hotkey configuration.
    pub fn update_screenshot(&self, config: HotkeyConfig) {
        #[cfg(windows)]
        let _ = self.cmd_tx.send(WorkerCmd::UpdateScreenshot(config));
        #[cfg(not(windows))]
        let _ = config;
    }

    /// Returns the current registration status of the recording hotkey.
    pub fn status(&self) -> HotkeyStatus {
        *self.record_status.lock().unwrap()
    }

    /// Returns the current registration status of the screenshot hotkey.
    pub fn screenshot_status(&self) -> HotkeyStatus {
        *self.screenshot_status.lock().unwrap()
    }

    fn drain_events(&self) {
        while let Ok(evt) = self.event_rx.try_recv() {
            match evt {
                HotkeyEvent::Record => self.pending_record.store(true, Ordering::Relaxed),
                HotkeyEvent::Screenshot => self.pending_screenshot.store(true, Ordering::Relaxed),
            }
        }
    }

    /// Non-blocking check whether the recording hotkey was triggered.
    pub fn poll_triggered(&self) -> bool {
        self.drain_events();
        self.pending_record.swap(false, Ordering::Relaxed)
    }

    /// Non-blocking check whether the screenshot hotkey was triggered.
    pub fn poll_screenshot_triggered(&self) -> bool {
        self.drain_events();
        self.pending_screenshot.swap(false, Ordering::Relaxed)
    }
}

impl Drop for HotkeyManager {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            let _ = self.cmd_tx.send(WorkerCmd::Stop);
            if let Some(w) = self.worker.take() {
                let _ = w.join();
            }
        }
    }
}

#[cfg(windows)]
struct HotkeyAtomicState {
    enabled: AtomicBool,
    ctrl: AtomicBool,
    alt: AtomicBool,
    shift: AtomicBool,
    win: AtomicBool,
    vk: std::sync::atomic::AtomicU64,
    last_trigger: std::sync::Mutex<Option<std::time::Instant>>,
}

#[cfg(windows)]
impl HotkeyAtomicState {
    fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            ctrl: AtomicBool::new(false),
            alt: AtomicBool::new(false),
            shift: AtomicBool::new(false),
            win: AtomicBool::new(false),
            vk: std::sync::atomic::AtomicU64::new(0),
            last_trigger: std::sync::Mutex::new(None),
        }
    }

    fn update(&self, cfg: &HotkeyConfig) {
        self.enabled.store(cfg.enabled && cfg.vk != 0, Ordering::Relaxed);
        self.ctrl.store(cfg.ctrl, Ordering::Relaxed);
        self.alt.store(cfg.alt, Ordering::Relaxed);
        self.shift.store(cfg.shift, Ordering::Relaxed);
        self.win.store(cfg.win, Ordering::Relaxed);
        self.vk.store(cfg.vk as u64, Ordering::Relaxed);
    }

    fn matches(&self, vk_code: u32, ctrl_down: bool, alt_down: bool, shift_down: bool, win_down: bool) -> bool {
        if !self.enabled.load(Ordering::Relaxed) {
            return false;
        }
        let target_vk = self.vk.load(Ordering::Relaxed) as u32;
        if target_vk == 0 || vk_code != target_vk {
            return false;
        }
        self.ctrl.load(Ordering::Relaxed) == ctrl_down
            && self.alt.load(Ordering::Relaxed) == alt_down
            && self.shift.load(Ordering::Relaxed) == shift_down
            && self.win.load(Ordering::Relaxed) == win_down
    }

    fn debounce_and_trigger(&self) -> bool {
        let mut last = self.last_trigger.lock().unwrap();
        let debounce = last.is_none_or(|t| t.elapsed() >= std::time::Duration::from_millis(300));
        if debounce {
            *last = Some(std::time::Instant::now());
            true
        } else {
            false
        }
    }
}

#[cfg(windows)]
struct HookState {
    record: HotkeyAtomicState,
    screenshot: HotkeyAtomicState,
    event_tx: std::sync::Mutex<Option<Sender<HotkeyEvent>>>,
    repaint_cb: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

#[cfg(windows)]
static HOOK_STATE: std::sync::OnceLock<Arc<HookState>> = std::sync::OnceLock::new();

#[cfg(windows)]
unsafe extern "system" fn ll_keyboard_proc(code: i32, wparam: windows::Win32::Foundation::WPARAM, lparam: windows::Win32::Foundation::LPARAM) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_MENU, VK_CONTROL as WIN_VK_CONTROL, VK_SHIFT as WIN_VK_SHIFT};
    use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, KBDLLHOOKSTRUCT, WM_KEYDOWN, WM_SYSKEYDOWN};

    if code >= 0
        && (wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN)
        && let Some(state) = HOOK_STATE.get()
    {
        let kbd = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let ctrl_down = unsafe { (GetAsyncKeyState(WIN_VK_CONTROL.0 as i32) as u16 & 0x8000) != 0 };
        let alt_down = unsafe { (GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000) != 0 };
        let shift_down = unsafe { (GetAsyncKeyState(WIN_VK_SHIFT.0 as i32) as u16 & 0x8000) != 0 };
        let win_down = unsafe {
            (GetAsyncKeyState(0x5B) as u16 & 0x8000) != 0 // VK_LWIN
                || (GetAsyncKeyState(0x5C) as u16 & 0x8000) != 0 // VK_RWIN
        };

        if state.record.matches(kbd.vkCode, ctrl_down, alt_down, shift_down, win_down)
            && state.record.debounce_and_trigger()
        {
            if let Ok(guard) = state.event_tx.lock()
                && let Some(tx) = guard.as_ref()
            {
                let _ = tx.send(HotkeyEvent::Record);
            }
            if let Ok(guard) = state.repaint_cb.lock()
                && let Some(cb) = guard.as_ref()
            {
                cb();
            }
        } else if state.screenshot.matches(kbd.vkCode, ctrl_down, alt_down, shift_down, win_down)
            && state.screenshot.debounce_and_trigger()
        {
            if let Ok(guard) = state.event_tx.lock()
                && let Some(tx) = guard.as_ref()
            {
                let _ = tx.send(HotkeyEvent::Screenshot);
            }
            if let Ok(guard) = state.repaint_cb.lock()
                && let Some(cb) = guard.as_ref()
            {
                cb();
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

#[cfg(windows)]
fn run_hotkey_worker(
    initial_record: HotkeyConfig,
    initial_screenshot: HotkeyConfig,
    cmd_rx: Receiver<WorkerCmd>,
    event_tx: Sender<HotkeyEvent>,
    record_status: Arc<std::sync::Mutex<HotkeyStatus>>,
    screenshot_status: Arc<std::sync::Mutex<HotkeyStatus>>,
    repaint_cb: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL,
        MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        MsgWaitForMultipleObjectsEx, PeekMessageW, RegisterClassExW, SetWindowsHookExW,
        TranslateMessage, UnhookWindowsHookEx, HHOOK, MSG, MWMO_INPUTAVAILABLE,
        PEEK_MESSAGE_REMOVE_TYPE, QS_ALLINPUT, WH_KEYBOARD_LL, WINDOW_EX_STYLE, WINDOW_STYLE,
        WM_HOTKEY, WNDCLASSEXW,
    };

    struct HookGuard(Option<HHOOK>);
    impl Drop for HookGuard {
        fn drop(&mut self) {
            if let Some(h) = self.0.take() {
                unsafe {
                    let _ = UnhookWindowsHookEx(h);
                }
            }
        }
    }

    const HOTKEY_RECORD_ID: i32 = 0x4F43; // "OC" (OpenClip Record)
    const HOTKEY_SCREENSHOT_ID: i32 = 0x4F53; // "OS" (OpenClip Screenshot)

    let hook_state = HOOK_STATE.get_or_init(|| {
        Arc::new(HookState {
            record: HotkeyAtomicState::new(),
            screenshot: HotkeyAtomicState::new(),
            event_tx: std::sync::Mutex::new(None),
            repaint_cb: std::sync::Mutex::new(None),
        })
    });
    *hook_state.event_tx.lock().unwrap() = Some(event_tx.clone());
    *hook_state.repaint_cb.lock().unwrap() = repaint_cb.clone();

    unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    let class_name = w!("OpenClipHotkeyMessageWindow");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    unsafe {
        let _ = RegisterClassExW(&wc);
    }

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("OpenClipHotkeyReceiver"),
            WINDOW_STYLE(0),
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

    // Install low-level hook as fallback for exclusive fullscreen games
    let hook_raw: Option<HHOOK> = unsafe {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_keyboard_proc), None, 0).ok()
    };
    let has_hook = hook_raw.is_some();
    let _hook_guard = HookGuard(hook_raw);

    let mut current_record_cfg = initial_record;
    let mut current_screenshot_cfg = initial_screenshot;

    let apply_one = |id: i32,
                     cfg: &HotkeyConfig,
                     st: &Arc<std::sync::Mutex<HotkeyStatus>>,
                     atomic_st: &HotkeyAtomicState,
                     name: &str| {
        atomic_st.update(cfg);

        if !cfg.enabled || cfg.vk == 0 {
            unsafe {
                let _ = UnregisterHotKey(Some(hwnd), id);
            }
            *st.lock().unwrap() = HotkeyStatus::Disabled;
            return;
        }

        let mut mods = MOD_NOREPEAT.0;
        if cfg.alt {
            mods |= MOD_ALT.0;
        }
        if cfg.ctrl {
            mods |= MOD_CONTROL.0;
        }
        if cfg.shift {
            mods |= MOD_SHIFT.0;
        }
        if cfg.win {
            mods |= MOD_WIN.0;
        }

        unsafe {
            let _ = UnregisterHotKey(Some(hwnd), id);
            let res = RegisterHotKey(Some(hwnd), id, HOT_KEY_MODIFIERS(mods), cfg.vk);
            if res.is_ok() {
                log::info!("Registered global {name} hotkey: {}", cfg.display_string());
                *st.lock().unwrap() = HotkeyStatus::Registered;
            } else {
                log::warn!(
                    "RegisterHotKey failed for {name} '{}' (conflict with another application); using low-level keyboard hook fallback",
                    cfg.display_string()
                );
                *st.lock().unwrap() = if has_hook {
                    HotkeyStatus::HookFallback
                } else {
                    HotkeyStatus::Conflict(cfg.vk)
                };
            }
        }
    };

    apply_one(HOTKEY_RECORD_ID, &current_record_cfg, &record_status, &hook_state.record, "record");
    apply_one(HOTKEY_SCREENSHOT_ID, &current_screenshot_cfg, &screenshot_status, &hook_state.screenshot, "screenshot");

    let mut msg = MSG::default();
    'pump: loop {
        // Drain pending worker commands
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                WorkerCmd::UpdateRecord(cfg) => {
                    current_record_cfg = cfg;
                    apply_one(HOTKEY_RECORD_ID, &current_record_cfg, &record_status, &hook_state.record, "record");
                }
                WorkerCmd::UpdateScreenshot(cfg) => {
                    current_screenshot_cfg = cfg;
                    apply_one(HOTKEY_SCREENSHOT_ID, &current_screenshot_cfg, &screenshot_status, &hook_state.screenshot, "screenshot");
                }
                WorkerCmd::Stop => break 'pump,
            }
        }

        // Dispatch all queued messages
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PEEK_MESSAGE_REMOVE_TYPE(1)).as_bool() {
                if msg.message == WM_HOTKEY {
                    let id = msg.wParam.0 as i32;
                    if id == HOTKEY_RECORD_ID {
                        if hook_state.record.debounce_and_trigger() {
                            let _ = event_tx.send(HotkeyEvent::Record);
                            if let Some(cb) = &repaint_cb {
                                cb();
                            }
                        }
                    } else if id == HOTKEY_SCREENSHOT_ID
                        && hook_state.screenshot.debounce_and_trigger()
                    {
                        let _ = event_tx.send(HotkeyEvent::Screenshot);
                        if let Some(cb) = &repaint_cb {
                            cb();
                        }
                    }
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Sleep-free wait: wake up immediately when any window message or input arrives,
        // or timeout after 50 ms to check for WorkerCmd.
        unsafe {
            let _ = MsgWaitForMultipleObjectsEx(None, 50, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
    }

    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_RECORD_ID);
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_SCREENSHOT_ID);
        if !hwnd.is_invalid() {
            let _ = DestroyWindow(hwnd);
        }
    }

    *hook_state.event_tx.lock().unwrap() = None;
    *hook_state.repaint_cb.lock().unwrap() = None;
    hook_state.record.enabled.store(false, Ordering::Relaxed);
    hook_state.screenshot.enabled.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_default_display_string() {
        let cfg = HotkeyConfig::default();
        assert_eq!(cfg.display_string(), "Ctrl+Alt+R");
        assert_eq!(cfg.to_string(), "Ctrl+Alt+R");
        assert!(!cfg.is_empty());
    }

    #[test]
    fn hotkey_screenshot_default() {
        let cfg = HotkeyConfig::screenshot_default();
        assert_eq!(cfg.display_string(), "Ctrl+Alt+Q");
        assert_eq!(cfg.to_string(), "Ctrl+Alt+Q");
        assert!(cfg.enabled);
        assert_eq!(cfg.vk, 0x51);
    }

    #[test]
    fn hotkey_vk_names() {
        assert_eq!(vk_to_name(0x52), "R");
        assert_eq!(vk_to_name(0x78), "F9");
        assert_eq!(vk_to_name(0x20), "Space");
        assert_eq!(vk_to_name(0x1B), "Esc");
    }

    #[test]
    fn hotkey_manager_creation_and_update() {
        let mgr = HotkeyManager::new(HotkeyConfig::default());
        // Background thread takes a few milliseconds to spin up and register the hotkey
        let mut status = mgr.status();
        for _ in 0..20 {
            if status != HotkeyStatus::Disabled {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
            status = mgr.status();
        }
        assert!(matches!(
            status,
            HotkeyStatus::Registered | HotkeyStatus::HookFallback | HotkeyStatus::Conflict(_)
        ));
        mgr.update(HotkeyConfig {
            enabled: false,
            ..Default::default()
        });
        for _ in 0..20 {
            if mgr.status() == HotkeyStatus::Disabled {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        assert_eq!(mgr.status(), HotkeyStatus::Disabled);
    }

    #[test]
    fn feedback_wav_generation_and_header() {
        let wav = generate_feedback_wav(&[(1000, 100), (0, 20), (1500, 100)]);
        assert!(wav.len() > 44);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        let data_len = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
        assert_eq!(wav.len(), 44 + data_len);

        // Ensure stop wav also generates valid header
        let stop_wav = generate_feedback_wav(&[(750, 180)]);
        assert!(stop_wav.len() > 44);
        assert_eq!(&stop_wav[0..4], b"RIFF");
    }
}

