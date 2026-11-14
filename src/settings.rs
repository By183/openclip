//! User settings: the recording format (container, codecs, size, quality …)
//! plus the other preferences that survive a restart. Persisted as JSON in the
//! platform config directory (`%APPDATA%\openclip\settings.json` on Windows).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::i18n::Lang;
use crate::t;
use crate::video::encoder::EncoderInfo;
use crate::video::mouse_fx::MouseFx;
use crate::video::watermark::Watermark;
use openclip_overlay::FpsOverlay;

/// Output container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Container {
    #[default]
    Mp4,
    Avi,
}

impl Container {
    pub fn extension(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Avi => "avi",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Container::Mp4 => "MP4",
            Container::Avi => "AVI",
        }
    }
}

/// The video encoder to use. Media Foundation encoders are identified by the
/// transform CLSID (32 hex digits) so a specific GPU's encoder can be chosen.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum VideoCodec {
    /// The best available H.264 encoder: a hardware encoder when present,
    /// otherwise OpenH264.
    #[default]
    Auto,
    /// Bundled OpenH264 (CPU, every platform).
    OpenH264,
    /// A Windows Media Foundation transform (hardware NVENC / AMF / Quick Sync /
    /// DX12, or a software encoder).
    Mf { hevc: bool, clsid: String },
}

impl VideoCodec {
    pub fn is_hevc(&self) -> bool {
        matches!(self, VideoCodec::Mf { hevc: true, .. })
    }

    /// Whether this codec needs Windows Media Foundation.
    pub fn needs_mf(&self) -> bool {
        matches!(self, VideoCodec::Mf { .. })
    }

    pub fn family(&self) -> &'static str {
        if self.is_hevc() { "H265/HEVC" } else { "H264" }
    }

    /// What `Auto` stands for on this machine: the first hardware H.264
    /// encoder, else OpenH264.
    pub fn resolve_auto(available: &[EncoderInfo]) -> VideoCodec {
        available.iter().find(|e| e.hardware && !e.hevc).map(EncoderInfo::codec).unwrap_or(VideoCodec::OpenH264)
    }

    /// The enumerated encoder behind this choice, if present on this machine.
    pub fn info<'a>(&self, available: &'a [EncoderInfo]) -> Option<&'a EncoderInfo> {
        match self {
            VideoCodec::Auto => Self::resolve_auto(available).info(available),
            VideoCodec::OpenH264 => None,
            VideoCodec::Mf { clsid, .. } => available.iter().find(|e| &e.clsid == clsid),
        }
    }

    /// Label used when no enumerated encoder describes this codec better.
    pub fn generic_label(&self) -> String {
        match self {
            VideoCodec::Auto => t!(CODEC_AUTO_GENERIC).into(),
            VideoCodec::OpenH264 => t!(CODEC_OPENH264).into(),
            VideoCodec::Mf { .. } => t!(CODEC_MF_GENERIC, self.family()),
        }
    }

    /// Display label, preferring the enumerated encoder's vendor-specific name.
    pub fn label(&self, available: &[EncoderInfo]) -> String {
        match self {
            VideoCodec::Auto => t!(CODEC_AUTO_RESOLVED, Self::resolve_auto(available).label(available)),
            _ => self.info(available).map(|e| e.label.clone()).unwrap_or_else(|| self.generic_label()),
        }
    }
}

/// Resolves a command-line style encoder choice: `openh264`, `h264-hw`,
/// `h264-sw`, `hevc`, `hevc-sw`, a substring of an encoder label (`nvenc`,
/// `quick`, `dx12`, …) or a CLSID.
pub fn pick_encoder(query: &str, available: &[EncoderInfo]) -> Option<VideoCodec> {
    let q = query.trim().to_ascii_lowercase();
    let find = |f: &dyn Fn(&EncoderInfo) -> bool| available.iter().find(|e| f(e)).map(EncoderInfo::codec);
    match q.as_str() {
        "auto" => Some(VideoCodec::Auto),
        "openh264" | "cpu" => Some(VideoCodec::OpenH264),
        "h264-hw" | "h264" => find(&|e| !e.hevc && e.hardware),
        "h264-sw" => find(&|e| !e.hevc && !e.hardware),
        "hevc" | "hevc-hw" | "h265" => find(&|e| e.hevc && e.hardware),
        "hevc-sw" | "h265-sw" => find(&|e| e.hevc && !e.hardware),
        _ => find(&|e| e.clsid == q || e.label.to_ascii_lowercase().contains(&q)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum H264Profile {
    #[default]
    Auto,
    Baseline,
    Main,
    High,
}

impl H264Profile {
    pub const ALL: [H264Profile; 4] = [H264Profile::Auto, H264Profile::Baseline, H264Profile::Main, H264Profile::High];

    pub fn label(self) -> &'static str {
        match self {
            H264Profile::Auto => t!(AUTO),
            H264Profile::Baseline => "Baseline",
            H264Profile::Main => "Main",
            H264Profile::High => "High",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum HevcProfile {
    #[default]
    Auto,
    Main,
}

impl HevcProfile {
    pub const ALL: [HevcProfile; 2] = [HevcProfile::Auto, HevcProfile::Main];

    pub fn label(self) -> &'static str {
        match self {
            HevcProfile::Auto => t!(AUTO),
            HevcProfile::Main => "Main",
        }
    }
}

/// Profile choices kept per codec family so switching codecs keeps each one's pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Profiles {
    pub h264: H264Profile,
    pub hevc: HevcProfile,
}

/// Output size relative to the captured source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SizeMode {
    #[default]
    Full,
    Half,
    /// Fit inside `width`×`height` keeping the aspect ratio (never upscales).
    Preset { width: u32, height: u32 },
    /// Scale each axis by a percentage (10–100).
    Percent { x: u32, y: u32 },
}

pub const SIZE_PRESETS: [(u32, u32); 4] = [(1920, 1080), (1280, 720), (854, 480), (640, 360)];

impl SizeMode {
    /// Output dimensions for a source of `src_w`×`src_h`: always even and at least 16 px.
    pub fn resolve(self, src_w: u32, src_h: u32) -> (u32, u32) {
        let (w, h) = match self {
            SizeMode::Full => (src_w, src_h),
            SizeMode::Half => (src_w / 2, src_h / 2),
            SizeMode::Preset { width, height } => {
                let s = (width as f64 / src_w.max(1) as f64).min(height as f64 / src_h.max(1) as f64).min(1.0);
                ((src_w as f64 * s).round() as u32, (src_h as f64 * s).round() as u32)
            }
            SizeMode::Percent { x, y } => {
                let (x, y) = (x.clamp(10, 100), y.clamp(10, 100));
                ((src_w as u64 * x as u64 / 100) as u32, (src_h as u64 * y as u64 / 100) as u32)
            }
        };
        (w.max(16) & !1, h.max(16) & !1)
    }

    pub fn label(self) -> String {
        match self {
            SizeMode::Full => t!(FMT_FULL_SIZE).into(),
            SizeMode::Half => t!(FMT_HALF_SIZE).into(),
            SizeMode::Preset { width, height } => format!("{width}×{height}"),
            SizeMode::Percent { x, y } => t!(FMT_CUSTOM_SIZE_LABEL, x, y),
        }
    }
}

/// Video CBR bounds supported in Format settings (kbps).
pub const MIN_BITRATE_KBPS: u32 = 200;
pub const MAX_BITRATE_KBPS: u32 = 100_000;

/// User-saved custom constant-bitrate preset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BitratePreset {
    pub name: String,
    pub kbps: u32,
}

impl BitratePreset {
    pub fn new(name: impl Into<String>, kbps: u32) -> Self {
        Self {
            name: name.into().trim().to_string(),
            kbps: kbps.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS),
        }
    }
}

/// Built-in bitrate preset with localized label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinBitratePreset {
    pub i18n_key: &'static str,
    pub kbps: u32,
}

impl BuiltinBitratePreset {
    pub fn label(self) -> &'static str {
        crate::i18n::t(self.i18n_key)
    }
}

pub const BUILTIN_BITRATE_PRESETS: [BuiltinBitratePreset; 6] = [
    BuiltinBitratePreset { i18n_key: crate::i18n::key::PRESET_720P_STREAM, kbps: 4_500 },
    BuiltinBitratePreset { i18n_key: crate::i18n::key::PRESET_1080P_STANDARD, kbps: 8_000 },
    BuiltinBitratePreset { i18n_key: crate::i18n::key::PRESET_1080P_HIGH, kbps: 12_000 },
    BuiltinBitratePreset { i18n_key: crate::i18n::key::PRESET_1440P_HIGH, kbps: 20_000 },
    BuiltinBitratePreset { i18n_key: crate::i18n::key::PRESET_144HZ_GAMING, kbps: 35_000 },
    BuiltinBitratePreset { i18n_key: crate::i18n::key::PRESET_4K_MASTER, kbps: 50_000 },
];

/// Cleans user bitrate presets: trims names, drops blanks, clamps bitrates and
/// deduplicates case-insensitively.
pub fn normalize_bitrate_presets(presets: &mut Vec<BitratePreset>) {
    let mut seen: Vec<String> = Vec::with_capacity(presets.len());
    let mut cleaned = Vec::with_capacity(presets.len());
    for p in presets.drain(..) {
        let name = p.name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if seen.contains(&lower) {
            continue;
        }
        seen.push(lower);
        cleaned.push(BitratePreset { name, kbps: p.kbps.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS) });
    }
    *presets = cleaned;
}

/// Inserts or updates a named user bitrate preset (case-insensitive match).
pub fn upsert_bitrate_preset(presets: &mut Vec<BitratePreset>, name: &str, kbps: u32) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return false;
    }
    let kbps = kbps.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS);
    if let Some(existing) = presets.iter_mut().find(|p| p.name.eq_ignore_ascii_case(trimmed)) {
        existing.name = trimmed.to_string();
        existing.kbps = kbps;
    } else {
        presets.push(BitratePreset { name: trimmed.to_string(), kbps });
    }
    true
}

/// Approximate video size in MiB/minute for a bitrate in kbps.
pub fn approx_mb_per_min(kbps: u32) -> f64 {
    kbps as f64 * 60.0 / 8.0 / 1024.0
}

/// How captured frames are converted from RGB (`BGRA`/`RGBA`) to `NV12`/`I420`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ColorConversion {
    /// Use GPU `VideoProcessorBlt` + zero-readback DXGI MFT (`Gpu` path), falling
    /// back to CPU conversion only at initialization if GPU setup fails before frame 0.
    #[default]
    Auto,
    /// Prefer GPU `VideoProcessorBlt` + zero-readback DXGI MFT (falls back to CPU
    /// only at startup if GPU initialization fails or overlays/encoder require CPU).
    Gpu,
    /// Always use CPU readback + software color conversion (`Converter`).
    Cpu,
}

impl ColorConversion {
    pub const ALL: [ColorConversion; 3] = [
        ColorConversion::Auto,
        ColorConversion::Gpu,
        ColorConversion::Cpu,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ColorConversion::Auto => t!(AUTO),
            ColorConversion::Gpu => "GPU",
            ColorConversion::Cpu => "CPU",
        }
    }
}

/// How captured audio sources are mapped to container audio streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AudioTrackMode {
    /// One mixed audio track combining all enabled sources.
    #[default]
    Mixed,
    /// Separate tracks when both System audio and Microphone are active
    /// (Track 1: System audio, Track 2: Microphone).
    Separate,
    /// Track 1 keeps the full mix (so players always hear everything), while
    /// Track 2 (System audio) and Track 3 (Microphone) are saved separately.
    MixedAndSeparate,
}

impl AudioTrackMode {
    pub const ALL: [AudioTrackMode; 3] = [
        AudioTrackMode::Mixed,
        AudioTrackMode::Separate,
        AudioTrackMode::MixedAndSeparate,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AudioTrackMode::Mixed => t!(TRACKS_MIXED),
            AudioTrackMode::Separate => t!(TRACKS_SEPARATE),
            AudioTrackMode::MixedAndSeparate => t!(TRACKS_MIXED_AND_SEPARATE),
        }
    }

    pub fn track_count(self, has_system: bool, has_mic: bool) -> usize {
        match (has_system, has_mic) {
            (false, false) => 0,
            (true, false) | (false, true) => 1,
            (true, true) => match self {
                AudioTrackMode::Mixed => 1,
                AudioTrackMode::Separate => 2,
                AudioTrackMode::MixedAndSeparate => 3,
            },
        }
    }
}

/// How the video bitrate is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateControl {
    /// 10–100 in steps of 10 (like classic recorders); the bitrate is derived from it.
    Quality(u8),
    ConstantBitrate { kbps: u32 },
}

impl Default for RateControl {
    fn default() -> Self {
        RateControl::Quality(80)
    }
}

pub const QUALITY_STEPS: [u8; 10] = [100, 90, 80, 70, 60, 50, 40, 30, 20, 10];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AudioCodec {
    #[default]
    Mp3,
    Aac,
    /// Uncompressed 16-bit PCM (AVI only).
    Pcm,
}

impl AudioCodec {
    pub const ALL: [AudioCodec; 3] = [AudioCodec::Mp3, AudioCodec::Aac, AudioCodec::Pcm];

    pub fn label(self) -> &'static str {
        match self {
            AudioCodec::Mp3 => "MP3",
            AudioCodec::Aac => "AAC",
            AudioCodec::Pcm => "PCM",
        }
    }

    pub fn allowed_bitrates(self) -> &'static [u32] {
        match self {
            AudioCodec::Mp3 => &MP3_BITRATES,
            AudioCodec::Aac => &AAC_BITRATES,
            AudioCodec::Pcm => &[],
        }
    }
}

pub const AUTO_FPS: u32 = 0;
pub const MIN_FPS: u32 = 1;
pub const MAX_FPS: u32 = 144;
pub const FPS_PRESETS: [u32; 13] = [10, 15, 20, 24, 25, 30, 48, 50, 60, 75, 90, 120, 144];
pub const MP3_BITRATES: [u32; 8] = [64, 96, 128, 160, 192, 224, 256, 320];
/// The only rates the Microsoft AAC encoder accepts (12/16/20/24 kB/s).
pub const AAC_BITRATES: [u32; 4] = [96, 128, 160, 192];
pub const SAMPLE_RATES: [u32; 2] = [44_100, 48_000];

/// Everything in the "Format settings" dialog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormatSettings {
    pub container: Container,
    pub size: SizeMode,
    pub fps: u32,
    pub video_codec: VideoCodec,
    pub rate_control: RateControl,
    /// Seconds between keyframes.
    pub keyframe_interval_s: f32,
    pub profiles: Profiles,
    pub audio_codec: AudioCodec,
    pub audio_bitrate_kbps: u32,
    pub audio_channels: u16,
    pub audio_sample_rate: u32,
}

impl Default for FormatSettings {
    fn default() -> Self {
        Self {
            container: Container::Mp4,
            size: SizeMode::Full,
            fps: 30,
            video_codec: VideoCodec::Auto,
            rate_control: RateControl::default(),
            keyframe_interval_s: 2.0,
            profiles: Profiles::default(),
            audio_codec: AudioCodec::Mp3,
            audio_bitrate_kbps: 160,
            audio_channels: 2,
            audio_sample_rate: 48_000,
        }
    }
}

impl FormatSettings {
    /// Whether Media Foundation codecs (AAC, hardware H.264/HEVC) can exist on this platform.
    pub const fn platform_has_mf() -> bool {
        cfg!(windows)
    }

    /// Applies the compatibility rules and returns a note for every change made.
    pub fn normalize(&mut self, available: &[EncoderInfo]) -> Vec<String> {
        let mut notes = Vec::new();

        if self.audio_codec == AudioCodec::Pcm && self.container == Container::Mp4 {
            self.audio_codec = if Self::platform_has_mf() { AudioCodec::Aac } else { AudioCodec::Mp3 };
            notes.push(t!(NOTE_PCM_AVI_ONLY, self.audio_codec.label()));
        }
        if self.audio_codec == AudioCodec::Aac && !Self::platform_has_mf() {
            self.audio_codec = AudioCodec::Mp3;
            notes.push(t!(NOTE_AAC_NEEDS_MF).into());
        }
        if self.video_codec.is_hevc() && self.container == Container::Avi {
            // Prefer a hardware H.264 encoder, then any H.264 encoder, then OpenH264.
            let h264 = available
                .iter()
                .find(|e| !e.hevc && e.hardware)
                .or_else(|| available.iter().find(|e| !e.hevc))
                .map(EncoderInfo::codec)
                .unwrap_or(VideoCodec::OpenH264);
            self.video_codec = h264;
            notes.push(t!(NOTE_HEVC_MP4_ONLY, self.video_codec.label(available)));
        }
        if self.video_codec.needs_mf() && self.video_codec.info(available).is_none() {
            let wanted = self.video_codec.family();
            self.video_codec = VideoCodec::OpenH264;
            notes.push(t!(NOTE_ENCODER_MISSING, wanted));
        }

        let allowed = self.audio_codec.allowed_bitrates();
        if !allowed.is_empty() && !allowed.contains(&self.audio_bitrate_kbps) {
            let want = self.audio_bitrate_kbps;
            self.audio_bitrate_kbps = *allowed.iter().min_by_key(|b| b.abs_diff(want)).unwrap();
        }
        if !matches!(self.audio_channels, 1 | 2) {
            self.audio_channels = 2;
        }
        if !SAMPLE_RATES.contains(&self.audio_sample_rate) {
            self.audio_sample_rate = 48_000;
        }
        if self.fps != AUTO_FPS {
            self.fps = self.fps.clamp(MIN_FPS, MAX_FPS);
        }
        if let SizeMode::Percent { x, y } = &mut self.size {
            *x = (*x).clamp(10, 100);
            *y = (*y).clamp(10, 100);
        }
        match &mut self.rate_control {
            RateControl::Quality(q) => *q = ((*q as u32).clamp(10, 100) as f32 / 10.0).round() as u8 * 10,
            RateControl::ConstantBitrate { kbps } => *kbps = (*kbps).clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS),
        }
        if !self.keyframe_interval_s.is_finite() {
            self.keyframe_interval_s = 2.0;
        }
        self.keyframe_interval_s = self.keyframe_interval_s.clamp(0.5, 10.0);
        notes
    }

    pub const fn is_auto_fps(&self) -> bool {
        self.fps == AUTO_FPS
    }

    /// Nominal frame rate when the active monitor's refresh rate is known,
    /// capped at 120 fps for smooth Auto grid pacing (falling back to 120 fps
    /// if the refresh rate cannot be determined).
    pub fn effective_fps_for(&self, monitor_hz: Option<u32>) -> u32 {
        if self.fps == AUTO_FPS {
            monitor_hz.filter(|&hz| hz >= 15).map(|hz| hz.min(120)).unwrap_or(120)
        } else {
            self.fps
        }
    }

    /// Nominal frame rate for encoding rate-control, muxer headers, and keyframes.
    pub const fn effective_fps(&self) -> u32 {
        if self.fps == AUTO_FPS {
            120
        } else {
            self.fps
        }
    }

    /// Bitrate for an output of `w`×`h` at an explicit nominal `fps`.
    pub fn target_bitrate_kbps_at(&self, w: u32, h: u32, fps: u32) -> u32 {
        match self.rate_control {
            RateControl::ConstantBitrate { kbps } => kbps,
            RateControl::Quality(q) => {
                let q = q.clamp(10, 100) as f64;
                // Bits per pixel per frame: 0.02 at q=10 … 0.20 at q=100.
                let bpp = 0.02 + (q - 10.0) / 90.0 * 0.18;
                let mut bps = w as f64 * h as f64 * fps.max(1) as f64 * bpp;
                if self.video_codec.is_hevc() {
                    bps *= 0.65;
                }
                (bps / 1000.0).round().clamp(500.0, 100_000.0) as u32
            }
        }
    }

    /// Bitrate for an output of `w`×`h` at the configured frame rate.
    pub fn target_bitrate_kbps(&self, w: u32, h: u32) -> u32 {
        self.target_bitrate_kbps_at(w, h, self.effective_fps())
    }

    pub fn keyframe_interval_frames_at(&self, fps: u32) -> u32 {
        (self.keyframe_interval_s.max(0.1) * fps.max(1) as f32).round().max(1.0) as u32
    }

    pub fn keyframe_interval_frames(&self) -> u32 {
        self.keyframe_interval_frames_at(self.effective_fps())
    }

    pub fn quality_label(&self) -> String {
        match self.rate_control {
            RateControl::Quality(q) => t!(QUALITY_LABEL, q),
            RateControl::ConstantBitrate { kbps } => t!(CBR_LABEL, kbps),
        }
    }

    pub fn profile_label(&self) -> &'static str {
        if self.video_codec.is_hevc() { self.profiles.hevc.label() } else { self.profiles.h264.label() }
    }

    /// (title, detail) for the video summary card.
    pub fn video_summary(&self, available: &[EncoderInfo], source: Option<(u32, u32)>) -> (String, String) {
        let size = match source {
            Some((w, h)) if w > 0 => {
                let (ow, oh) = self.size.resolve(w, h);
                format!("{ow}×{oh}")
            }
            _ => self.size.label(),
        };
        let bitrate = match (self.rate_control, source) {
            (RateControl::Quality(_), Some((w, h))) if w > 0 => {
                let (ow, oh) = self.size.resolve(w, h);
                t!(VIDEO_SUMMARY_BITRATE, self.target_bitrate_kbps(ow, oh))
            }
            _ => String::new(),
        };
        let fps_label = if self.fps == AUTO_FPS {
            t!(AUTO).to_string()
        } else {
            self.fps.to_string()
        };
        (
            self.video_codec.label(available),
            t!(VIDEO_SUMMARY, size, fps_label, self.quality_label(), bitrate, self.profile_label()),
        )
    }

    /// (title, detail) for the audio summary card; `sources` describes what is recorded.
    pub fn audio_summary(&self, sources: &str) -> (String, String) {
        let ch = if self.audio_channels == 1 { t!(MONO_LOWER) } else { t!(STEREO_LOWER) };
        let rate = format!("{:.1}KHz", self.audio_sample_rate as f64 / 1000.0);
        let detail = match self.audio_codec {
            AudioCodec::Pcm => t!(AUDIO_SUMMARY_PCM, rate, ch, sources),
            _ => t!(AUDIO_SUMMARY, rate, ch, self.audio_bitrate_kbps, sources),
        };
        (self.audio_codec.label().to_string(), detail)
    }
}

fn default_volume() -> u8 {
    100
}

fn default_true() -> bool {
    true
}

fn default_screenshot_hotkey() -> crate::hotkey::HotkeyConfig {
    crate::hotkey::HotkeyConfig::screenshot_default()
}

/// Sound feedback mode when starting and stopping recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SoundFeedback {
    /// Never play a sound signal. Recording starts immediately without delay.
    Off,
    /// Play a sound signal only when triggered by global hotkey.
    #[default]
    HotkeyOnly,
    /// Play a sound signal on any trigger (hotkey, UI button, minibar, countdown).
    Always,
}

impl SoundFeedback {
    pub const ALL: [SoundFeedback; 3] = [SoundFeedback::Off, SoundFeedback::HotkeyOnly, SoundFeedback::Always];

    pub fn is_enabled_for_hotkey(self) -> bool {
        matches!(self, SoundFeedback::HotkeyOnly | SoundFeedback::Always)
    }

    pub fn is_enabled_for_button(self) -> bool {
        matches!(self, SoundFeedback::Always)
    }

    pub fn is_enabled_for_source(self, from_hotkey: bool) -> bool {
        if from_hotkey {
            self.is_enabled_for_hotkey()
        } else {
            self.is_enabled_for_button()
        }
    }
}

/// Root of `settings.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub format: FormatSettings,
    /// `None` → the default output folder.
    pub output_dir: Option<PathBuf>,
    pub file_prefix: String,
    pub system_audio: bool,
    /// Specific playback endpoint for loopback capture, `None` → system default.
    pub system_audio_device: Option<String>,
    /// System audio capture volume in percent (`0..=200`, 100 = unity gain).
    #[serde(default = "default_volume")]
    pub system_audio_volume: u8,
    pub mic_enabled: bool,
    /// Microphone by name (re-resolved to a device index at load).
    pub mic_name: Option<String>,
    /// Microphone capture volume in percent (`0..=200`, 100 = unity gain).
    #[serde(default = "default_volume")]
    pub mic_volume: u8,
    /// Multi-track layout when recording both system audio and microphone.
    pub audio_track_mode: AudioTrackMode,
    /// User-saved custom video CBR presets shown in Format settings.
    pub bitrate_presets: Vec<BitratePreset>,
    pub mouse_fx: MouseFx,
    /// The openclip badge burned into recordings and snapshots.
    pub watermark: Watermark,
    /// The frame-rate counter openclip's hook draws inside a game.
    pub fps_overlay: FpsOverlay,
    /// Whether the one-time explanation of what game mode loads into a game has
    /// been acknowledged. Game mode does nothing until it has.
    pub game_consented: bool,
    /// Executables the user chose never to hook, by file name.
    pub game_ignored: Vec<String>,
    /// Count down before recording starts.
    pub countdown_enabled: bool,
    pub countdown_secs: u32,
    /// Color conversion path (`Auto` / `Gpu` / `Cpu`).
    #[serde(default)]
    pub color_conversion: ColorConversion,
    /// Sound signal on start and stop recording.
    #[serde(default)]
    pub sound_feedback: SoundFeedback,
    /// Global hotkey for starting / stopping recording.
    #[serde(default)]
    pub hotkey: crate::hotkey::HotkeyConfig,
    /// Global hotkey for taking a screenshot.
    #[serde(default = "default_screenshot_hotkey")]
    pub screenshot_hotkey: crate::hotkey::HotkeyConfig,
    /// Whether taking a screenshot starts interactive region selection.
    #[serde(default)]
    pub region_capture_on_screenshot: bool,
    /// Whether to open the annotation editor after selecting a region.
    #[serde(default = "default_true")]
    pub annotate_screenshot: bool,
    /// Interface language; applied to [`crate::i18n`] by [`Settings::load`].
    pub language: Lang,
    /// Look for a newer release on GitHub when the app starts (see [`crate::update`]).
    pub check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            format: FormatSettings::default(),
            output_dir: None,
            file_prefix: "openclip".into(),
            system_audio: true,
            system_audio_device: None,
            system_audio_volume: default_volume(),
            mic_enabled: false,
            mic_name: None,
            mic_volume: default_volume(),
            audio_track_mode: AudioTrackMode::default(),
            bitrate_presets: Vec::new(),
            mouse_fx: MouseFx::default(),
            watermark: Watermark::default(),
            fps_overlay: FpsOverlay::default(),
            game_consented: false,
            game_ignored: Vec::new(),
            countdown_enabled: true,
            countdown_secs: 3,
            color_conversion: ColorConversion::default(),
            sound_feedback: SoundFeedback::default(),
            hotkey: crate::hotkey::HotkeyConfig::default(),
            screenshot_hotkey: crate::hotkey::HotkeyConfig::screenshot_default(),
            region_capture_on_screenshot: false,
            annotate_screenshot: true,
            language: Lang::default(),
            check_updates: true,
        }
    }
}

impl Settings {
    /// Clamps standalone scalar preferences and cleans up custom bitrate presets.
    pub fn normalize_fields(&mut self) {
        self.system_audio_volume = self.system_audio_volume.clamp(0, 200);
        self.mic_volume = self.mic_volume.clamp(0, 200);
        normalize_bitrate_presets(&mut self.bitrate_presets);
    }
}

pub const SETTINGS_FILE: &str = "settings.json";

/// Picks where settings live: next to the executable (portable) when that
/// folder is writable or already holds a settings file, else the per-user
/// config directory.
pub fn choose_path(exe_dir: Option<&Path>, portable_exists: bool, writable: bool, roaming: Option<PathBuf>) -> Option<PathBuf> {
    match exe_dir {
        Some(dir) if portable_exists || writable => Some(dir.join(SETTINGS_FILE)),
        _ => roaming,
    }
}

pub(crate) fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".openclip-write-test-{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

impl Settings {
    /// The per-user location used before settings became portable.
    pub fn legacy_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("openclip").join(SETTINGS_FILE))
    }

    /// `settings.json` next to the executable when possible (portable install),
    /// otherwise the per-user config directory.
    pub fn path() -> Option<PathBuf> {
        static CHOSEN: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        CHOSEN
            .get_or_init(|| {
                let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
                let portable_exists = exe_dir.as_ref().map(|d| d.join(SETTINGS_FILE).is_file()).unwrap_or(false);
                let writable = exe_dir.as_ref().map(|d| dir_is_writable(d)).unwrap_or(false);
                choose_path(exe_dir.as_deref(), portable_exists, writable, Self::legacy_path())
            })
            .clone()
    }

    /// Loads the settings and activates the saved interface language; missing
    /// or corrupt files yield the defaults. A settings file from the old
    /// per-user location is picked up when the portable one does not exist yet
    /// (it moves on the next save).
    pub fn load() -> Settings {
        let settings = Self::load_raw();
        crate::i18n::set_lang(settings.language);
        settings
    }

    /// Parses settings from a JSON string, ensuring backward compatibility with legacy fields.
    pub fn parse_from_str(json: &str) -> Result<Settings, serde_json::Error> {
        let val: serde_json::Value = serde_json::from_str(json)?;
        let has_sound_feedback = val.get("sound_feedback").is_some();
        let mut s: Settings = serde_json::from_value(val)?;
        if !has_sound_feedback {
            s.sound_feedback = if s.hotkey.sound {
                SoundFeedback::HotkeyOnly
            } else {
                SoundFeedback::Off
            };
        }
        s.normalize_fields();
        Ok(s)
    }

    fn load_raw() -> Settings {
        let Some(path) = Self::path() else { return Settings::default() };
        let mut candidates = vec![path];
        if let Some(legacy) = Self::legacy_path()
            && !candidates.contains(&legacy)
        {
            candidates.push(legacy);
        }
        for path in candidates {
            match std::fs::read_to_string(&path) {
                Ok(content) => match Self::parse_from_str(&content) {
                    Ok(s) => {
                        log::info!("settings loaded from {}", path.display());
                        return s;
                    }
                    Err(e) => {
                        log::warn!("settings: {} is not valid, using defaults: {e}", path.display());
                        return Settings::default();
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    log::warn!("settings: cannot read {}: {e}", path.display());
                    return Settings::default();
                }
            }
        }
        Settings::default()
    }

    /// Writes the settings atomically (temp file + rename).
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("no config directory on this system"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut to_save = self.clone();
        to_save.hotkey.sound = to_save.sound_feedback.is_enabled_for_hotkey();
        let json = serde_json::to_string_pretty(&to_save)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        log::info!("settings saved to {}", path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::encoder::Vendor;

    fn nvenc() -> EncoderInfo {
        EncoderInfo {
            hevc: false,
            label: "H264 (NVIDIA® NVENC)".into(),
            friendly_name: "NVIDIA H.264 Encoder MFT".into(),
            vendor: Vendor::Nvidia,
            hardware: true,
            clsid: "60f445605a204857bfefd29773cb8040".into(),
        }
    }

    #[test]
    fn size_modes_resolve_even_dims() {
        assert_eq!(SizeMode::Full.resolve(1921, 1081), (1920, 1080));
        assert_eq!(SizeMode::Half.resolve(1920, 1080), (960, 540));
        assert_eq!(SizeMode::Preset { width: 1280, height: 720 }.resolve(2560, 1440), (1280, 720));
        // Never upscales.
        assert_eq!(SizeMode::Preset { width: 1920, height: 1080 }.resolve(800, 600), (800, 600));
        assert_eq!(SizeMode::Percent { x: 50, y: 75 }.resolve(1000, 1000), (500, 750));
        assert_eq!(SizeMode::Percent { x: 10, y: 10 }.resolve(20, 20), (16, 16));
    }

    #[test]
    fn normalize_enforces_container_rules() {
        let mut f = FormatSettings { container: Container::Mp4, audio_codec: AudioCodec::Pcm, ..Default::default() };
        let notes = f.normalize(&[]);
        assert_ne!(f.audio_codec, AudioCodec::Pcm);
        assert_eq!(notes.len(), 1);

        let hevc = VideoCodec::Mf { hevc: true, clsid: "abc".into() };
        let mut f = FormatSettings { video_codec: hevc.clone(), ..Default::default() };
        f.normalize(&[]);
        assert_eq!(f.video_codec, VideoCodec::OpenH264);

        // HEVC in AVI falls back to an available hardware H.264 encoder.
        let mut f = FormatSettings { video_codec: hevc, container: Container::Avi, ..Default::default() };
        f.normalize(&[nvenc()]);
        assert_eq!(f.video_codec, nvenc().codec());

        let mut f = FormatSettings { audio_bitrate_kbps: 150, audio_codec: AudioCodec::Aac, ..Default::default() };
        f.normalize(&[]);
        if FormatSettings::platform_has_mf() {
            assert_eq!(f.audio_bitrate_kbps, 160);
        } else {
            assert_eq!(f.audio_codec, AudioCodec::Mp3);
        }
    }

    #[test]
    fn auto_codec_resolves_and_parses_legacy() {
        assert_eq!(FormatSettings::default().video_codec, VideoCodec::Auto);
        assert_eq!(VideoCodec::resolve_auto(&[]), VideoCodec::OpenH264);
        assert_eq!(VideoCodec::resolve_auto(&[nvenc()]), nvenc().codec());
        assert!(VideoCodec::Auto.label(&[nvenc()]).contains("NVENC"));
        assert!(!VideoCodec::Auto.needs_mf());
        let mut f = FormatSettings::default();
        assert!(f.normalize(&[]).is_empty(), "Auto never needs coercion");
        let legacy: FormatSettings = serde_json::from_str(r#"{"video_codec":"OpenH264"}"#).unwrap();
        assert_eq!(legacy.video_codec, VideoCodec::OpenH264);
        assert_eq!(pick_encoder("auto", &[]), Some(VideoCodec::Auto));
    }

    #[test]
    fn settings_path_prefers_portable() {
        let exe = Path::new("C:/apps/openclip");
        let roaming = Some(PathBuf::from("C:/Users/x/AppData/Roaming/openclip/settings.json"));
        assert_eq!(choose_path(Some(exe), false, true, roaming.clone()), Some(exe.join("settings.json")));
        assert_eq!(choose_path(Some(exe), true, false, roaming.clone()), Some(exe.join("settings.json")));
        assert_eq!(choose_path(Some(exe), false, false, roaming.clone()), roaming.clone());
        assert_eq!(choose_path(None, false, true, roaming.clone()), roaming);
        assert!(Settings::default().countdown_enabled);
        assert_eq!(Settings::default().countdown_secs, 3);
        assert!(Settings::default().hotkey.enabled);
        assert_eq!(Settings::default().hotkey.display_string(), "Ctrl+Alt+R");
        assert!(Settings::default().screenshot_hotkey.enabled);
        assert_eq!(Settings::default().screenshot_hotkey.display_string(), "Ctrl+Alt+Q");
    }

    #[test]
    fn picks_encoders_by_name() {
        let list = [nvenc()];
        assert_eq!(pick_encoder("openh264", &list), Some(VideoCodec::OpenH264));
        assert_eq!(pick_encoder("h264-hw", &list), Some(nvenc().codec()));
        assert_eq!(pick_encoder("nvenc", &list), Some(nvenc().codec()));
        assert_eq!(pick_encoder("hevc", &list), None);
        assert_eq!(VideoCodec::OpenH264.label(&list), "H264 (OpenH264, CPU)");
        assert_eq!(nvenc().codec().label(&list), "H264 (NVIDIA® NVENC)");
    }

    #[test]
    fn quality_maps_to_sane_bitrates() {
        let f = FormatSettings { rate_control: RateControl::Quality(80), ..Default::default() };
        let kbps = f.target_bitrate_kbps(1920, 1080);
        assert!((8_000..=12_000).contains(&kbps), "{kbps}");
        let low = FormatSettings { rate_control: RateControl::Quality(10), ..Default::default() };
        assert!(low.target_bitrate_kbps(1920, 1080) < kbps);
        let cbr = FormatSettings { rate_control: RateControl::ConstantBitrate { kbps: 1234 }, ..Default::default() };
        assert_eq!(cbr.target_bitrate_kbps(1920, 1080), 1234);
    }

    #[test]
    fn settings_roundtrip_json() {
        let s = Settings {
            file_prefix: "x".into(),
            format: FormatSettings { video_codec: nvenc().codec(), ..Default::default() },
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.file_prefix, "x");
        assert_eq!(back.format, s.format);
        // Unknown / missing fields are tolerated.
        let partial: Settings = serde_json::from_str(r#"{"format":{"fps":60}}"#).unwrap();
        assert_eq!(partial.format.fps, 60);
        assert_eq!(partial.format.container, Container::Mp4);
    }

    #[test]
    fn fps_presets_reach_144_and_normalize_clamps() {
        assert!(FPS_PRESETS.contains(&75));
        assert!(FPS_PRESETS.contains(&90));
        assert!(FPS_PRESETS.contains(&120));
        assert_eq!(FPS_PRESETS.last().copied(), Some(144));

        let mut f = FormatSettings {
            fps: 240,
            rate_control: RateControl::ConstantBitrate { kbps: 500_000 },
            ..Default::default()
        };
        f.normalize(&[]);
        assert_eq!(f.fps, 144);
        assert_eq!(f.rate_control, RateControl::ConstantBitrate { kbps: MAX_BITRATE_KBPS });

        let mut f = FormatSettings {
            fps: 0,
            rate_control: RateControl::ConstantBitrate { kbps: 10 },
            ..Default::default()
        };
        f.normalize(&[]);
        assert_eq!(f.fps, AUTO_FPS);
        assert!(f.is_auto_fps());
        assert_eq!(f.effective_fps(), 120);
        assert_eq!(f.effective_fps_for(Some(144)), 120);
        assert_eq!(f.effective_fps_for(Some(75)), 75);
        assert_eq!(f.effective_fps_for(Some(60)), 60);
        assert_eq!(f.effective_fps_for(None), 120);
        assert_eq!(f.effective_fps_for(Some(0)), 120);
        assert_eq!(f.rate_control, RateControl::ConstantBitrate { kbps: MIN_BITRATE_KBPS });
    }

    #[test]
    fn audio_track_mode_counts() {
        assert_eq!(AudioTrackMode::Mixed.track_count(false, false), 0);
        assert_eq!(AudioTrackMode::Separate.track_count(true, false), 1);
        assert_eq!(AudioTrackMode::MixedAndSeparate.track_count(false, true), 1);
        assert_eq!(AudioTrackMode::Mixed.track_count(true, true), 1);
        assert_eq!(AudioTrackMode::Separate.track_count(true, true), 2);
        assert_eq!(AudioTrackMode::MixedAndSeparate.track_count(true, true), 3);
    }

    #[test]
    fn bitrate_presets_normalize_and_upsert() {
        let mut presets = vec![
            BitratePreset { name: "  Stream ".into(), kbps: 50 },
            BitratePreset { name: "   ".into(), kbps: 5000 },
            BitratePreset { name: "stream".into(), kbps: 9000 },
            BitratePreset { name: "Ultra".into(), kbps: 999_999 },
        ];
        normalize_bitrate_presets(&mut presets);
        assert_eq!(presets.len(), 2);
        assert_eq!(presets[0], BitratePreset { name: "Stream".into(), kbps: MIN_BITRATE_KBPS });
        assert_eq!(presets[1], BitratePreset { name: "Ultra".into(), kbps: MAX_BITRATE_KBPS });

        assert!(!upsert_bitrate_preset(&mut presets, "   ", 8000));
        assert!(upsert_bitrate_preset(&mut presets, "stream", 6500));
        assert_eq!(presets.len(), 2);
        assert_eq!(presets[0], BitratePreset { name: "stream".into(), kbps: 6500 });
        assert!(upsert_bitrate_preset(&mut presets, "144 Hz", 35_000));
        assert_eq!(presets.len(), 3);
    }

    #[test]
    fn settings_backward_compat_and_new_fields() {
        // Legacy JSON missing new audio/bitrate fields still defaults to unity volume,
        // mixed tracks, empty custom presets, and parses new Ukrainian language code.
        let mut legacy: Settings = serde_json::from_str(
            r#"{"format":{"fps":120,"rate_control":{"Quality":100}},"system_audio":true,"language":"uk"}"#,
        )
        .unwrap();
        legacy.normalize_fields();
        assert_eq!(legacy.format.fps, 120);
        assert_eq!(legacy.system_audio_volume, 100);
        assert_eq!(legacy.mic_volume, 100);
        assert_eq!(legacy.system_audio_device, None);
        assert_eq!(legacy.audio_track_mode, AudioTrackMode::Mixed);
        assert!(legacy.bitrate_presets.is_empty());
        assert_eq!(legacy.language, Lang::Uk);
        assert_eq!(legacy.color_conversion, ColorConversion::Auto);

        let mut clamped: Settings = serde_json::from_str(
            r#"{"system_audio_volume":255,"mic_volume":220,"audio_track_mode":"Separate","color_conversion":"Cpu"}"#,
        )
        .unwrap();
        clamped.normalize_fields();
        assert_eq!(clamped.system_audio_volume, 200);
        assert_eq!(clamped.mic_volume, 200);
        assert_eq!(clamped.audio_track_mode, AudioTrackMode::Separate);
        assert_eq!(clamped.color_conversion, ColorConversion::Cpu);
    }

    #[test]
    fn sound_feedback_modes_and_source_checks() {
        assert_eq!(SoundFeedback::default(), SoundFeedback::HotkeyOnly);

        // Off
        assert!(!SoundFeedback::Off.is_enabled_for_hotkey());
        assert!(!SoundFeedback::Off.is_enabled_for_button());
        assert!(!SoundFeedback::Off.is_enabled_for_source(true));
        assert!(!SoundFeedback::Off.is_enabled_for_source(false));

        // HotkeyOnly
        assert!(SoundFeedback::HotkeyOnly.is_enabled_for_hotkey());
        assert!(!SoundFeedback::HotkeyOnly.is_enabled_for_button());
        assert!(SoundFeedback::HotkeyOnly.is_enabled_for_source(true));
        assert!(!SoundFeedback::HotkeyOnly.is_enabled_for_source(false));

        // Always
        assert!(SoundFeedback::Always.is_enabled_for_hotkey());
        assert!(SoundFeedback::Always.is_enabled_for_button());
        assert!(SoundFeedback::Always.is_enabled_for_source(true));
        assert!(SoundFeedback::Always.is_enabled_for_source(false));
    }

    #[test]
    fn sound_feedback_legacy_migration() {
        // Legacy file with hotkey.sound = true and no sound_feedback field -> HotkeyOnly
        let s_true = Settings::parse_from_str(r#"{"hotkey":{"sound":true}}"#).unwrap();
        assert_eq!(s_true.sound_feedback, SoundFeedback::HotkeyOnly);

        // Legacy file with hotkey.sound = false and no sound_feedback field -> Off
        let s_false = Settings::parse_from_str(r#"{"hotkey":{"sound":false}}"#).unwrap();
        assert_eq!(s_false.sound_feedback, SoundFeedback::Off);

        // Explicit sound_feedback overrides legacy hotkey.sound
        let s_always = Settings::parse_from_str(r#"{"hotkey":{"sound":false},"sound_feedback":"Always"}"#).unwrap();
        assert_eq!(s_always.sound_feedback, SoundFeedback::Always);

        let s_off = Settings::parse_from_str(r#"{"hotkey":{"sound":true},"sound_feedback":"Off"}"#).unwrap();
        assert_eq!(s_off.sound_feedback, SoundFeedback::Off);
    }
}
