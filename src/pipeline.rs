//! Recording pipeline: capture thread → encode thread (video encoder + muxer),
//! plus an audio thread (cpal → mixer → audio encoder) feeding the muxer.
//!
//! Video runs on a **fixed-cadence frame clock**: slot `k` has presentation
//! time `pts0 + k / fps`. Captured frames are collected until a slot's
//! deadline, the newest one is encoded with the slot's timestamp (or the last
//! frame is repeated when nothing new arrived), so the file always has a
//! perfectly regular frame rate whatever the capture timing does. Frame
//! buffers are pooled, the watermark and mouse effects are painted in place and
//! restored, and previews are only produced while the GUI shows them.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::audio::capture::{
    default_input_device_name, default_output_device_name, open_microphone,
    open_system_loopback_device, AudioSource, Chunk, DeviceNotificationListener, Queue, SharedQueue,
};
use crate::audio::mixer::Mixer;
use crate::audio::{create_audio_encoder, AudioEncoder, AudioFrame};
use crate::capture::monitors::{source_origin, source_refresh_hz};
use crate::capture::{self, CaptureConfig, CaptureHandle, CapturedFrame, FramePool, LiveRect, Rect, Source};
use crate::mux::{AudioTrackConfig, Muxer, VideoCodecConfig, VideoTrackConfig};
use crate::settings::{AudioTrackMode, ColorConversion, FormatSettings, SizeMode, VideoCodec};
use crate::video::convert::even_dims;
#[cfg(windows)]
use crate::video::d3d11_vp::GpuNv12Slot;
use crate::video::mouse_fx::{FrameClick, MouseFx, MouseSampler, Patch};
use crate::video::preview::{PreviewImage, Previewer};
use crate::video::watermark::{Watermark, WatermarkRenderer};
use crate::video::{
    available_encoders, Converter, EncodeStageTiming, EncodedFrame, EncoderRequest, FrameInput, InputLayout,
    PixelFormat, RawFrame, Scaler, VideoEncoder,
};
#[cfg(not(windows))]
use crate::video::create_video_encoder;
#[cfg(windows)]
use crate::video::create_video_encoder_with_d3d;

/// Longest side of preview images handed to the GUI.
const PREVIEW_MAX_SIDE: u32 = 640;
/// Minimum time between two preview images.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(200);
/// Audio is mixed this far behind wall-clock so device latency never causes gaps.
const AUDIO_LAG: Duration = Duration::from_millis(200);
/// Frame buffers kept in circulation (queue + in flight + last frame + pool headroom).
const POOL_SIZE: usize = 6;
/// Bounded queue inside [`FrameInbox`] so WGC does not take pool locks on transient bursts.
const FRAME_QUEUE_CAP: usize = 4;

#[derive(Debug, Clone)]
pub struct RecordConfig {
    pub source: Source,
    pub format: FormatSettings,
    pub color_conversion: ColorConversion,
    pub mouse_fx: MouseFx,
    pub watermark: Watermark,
    pub system_audio: bool,
    /// Playback endpoint for loopback capture; `None` picks the system default.
    pub system_audio_device: Option<String>,
    /// Linear gain applied to system audio (`1.0` = 100%).
    pub system_audio_gain: f32,
    /// `Some(None)` = default microphone, `Some(Some(name))` = named device.
    pub microphone: Option<Option<String>>,
    /// Linear gain applied to the microphone (`1.0` = 100%).
    pub mic_gain: f32,
    /// Track layout when recording both system audio and microphone.
    pub audio_track_mode: AudioTrackMode,
    pub output: PathBuf,
}

impl RecordConfig {
    pub fn wants_audio(&self) -> bool {
        self.system_audio || self.microphone.is_some()
    }

    pub fn is_auto_fps(&self) -> bool {
        self.format.is_auto_fps()
    }

    pub fn fps(&self) -> u32 {
        self.format.effective_fps()
    }
}

/// Role assigned to an output audio stream in the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackRole {
    Mixed,
    System,
    Mic,
}

/// Resolves which audio streams should be created in the container for the
/// given open sources and multi-track preference.
pub fn resolve_audio_tracks(has_sys: bool, has_mic: bool, mode: AudioTrackMode) -> Vec<TrackRole> {
    match (has_sys, has_mic) {
        (false, false) => Vec::new(),
        (true, false) => vec![TrackRole::System],
        (false, true) => vec![TrackRole::Mic],
        (true, true) => match mode {
            AudioTrackMode::Mixed => vec![TrackRole::Mixed],
            AudioTrackMode::Separate => vec![TrackRole::System, TrackRole::Mic],
            AudioTrackMode::MixedAndSeparate => vec![TrackRole::Mixed, TrackRole::System, TrackRole::Mic],
        },
    }
}

/// Timeline origin: pts of the first encoded frame, i.e. where the output file
/// starts. Unknown until the encode thread has seen a frame.
#[derive(Debug)]
pub struct Origin(AtomicU64);

impl Default for Origin {
    fn default() -> Self {
        Self(AtomicU64::new(u64::MAX))
    }
}

impl Origin {
    pub fn get(&self) -> Option<Duration> {
        match self.0.load(Ordering::SeqCst) {
            u64::MAX => None,
            us => Some(Duration::from_micros(us)),
        }
    }

    fn set(&self, pts: Duration) {
        self.0.store(pts.as_micros() as u64, Ordering::SeqCst);
    }
}

/// Live counters, readable from the GUI.
#[derive(Debug, Default)]
pub struct Stats {
    pub frames_captured: AtomicU64,
    /// Frames submitted to the video encoder.
    pub frames_to_encoder: AtomicU64,
    pub frames_encoded: AtomicU64,
    /// Frames lost because the capture channel was full.
    pub frames_dropped: AtomicU64,
    /// Frames the encoder chose not to output (rate control) or the container could not place.
    pub frames_skipped: AtomicU64,
    /// Slots filled by repeating the previous frame (static screen or late capture).
    pub frames_repeated: AtomicU64,
    /// Captured frames replaced by a newer one before their slot came (source faster than fps).
    pub frames_superseded: AtomicU64,
    /// Slots the encode loop never reached because it was running behind.
    pub slots_skipped: AtomicU64,
    /// Slots the encode loop processed; `slots_done + slots_skipped ≈ fps × elapsed`.
    pub slots_done: AtomicU64,
    /// Configured frame rate, so the GUI can show the per-slot time budget.
    pub target_fps: AtomicU64,
    /// Rolling average encode time per frame, in microseconds.
    pub encode_us: AtomicU64,
    /// Rolling average of the whole per-slot work (scale + effects + convert + encode + mux).
    pub slot_us: AtomicU64,
    /// Rolling average time spent writing to the container per frame.
    pub mux_us: AtomicU64,
    pub audio_frames: AtomicU64,
    pub bytes_written: AtomicU64,
    pub width: AtomicU64,
    pub height: AtomicU64,
    /// Where the output file's timeline starts, in the recording timebase.
    pub stream_start: Origin,
    /// Recording timestamp when Stop was requested, used to extend the final Auto frame duration.
    pub stop_pts: Origin,
    pub error: Mutex<Option<String>>,
    pub audio_note: Mutex<Option<String>>,
    /// Encoder / container notes (e.g. a hardware encoder fell back to OpenH264).
    pub note: Mutex<Option<String>>,
    /// The video encoder actually in use, as reported by `VideoEncoder::describe`.
    pub encoder: Mutex<Option<String>>,
}

impl Stats {
    fn set_error(&self, msg: impl Into<String>) {
        let msg = msg.into();
        log::error!("{msg}");
        let mut e = self.error.lock().unwrap();
        if e.is_none() {
            *e = Some(msg);
        }
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    pub fn add_note(&self, msg: impl Into<String>) {
        let msg = msg.into();
        log::warn!("{msg}");
        let mut n = self.note.lock().unwrap();
        match n.as_mut() {
            Some(existing) => {
                existing.push_str("; ");
                existing.push_str(&msg);
            }
            None => *n = Some(msg),
        }
    }

    pub fn note(&self) -> Option<String> {
        self.note.lock().unwrap().clone()
    }

    pub fn encoder(&self) -> Option<String> {
        self.encoder.lock().unwrap().clone()
    }

    /// Zeroes the counters accumulated before the timeline existed (capture
    /// warm-up), so every number the GUI shows describes the recorded file.
    fn reset_pre_roll(&self) {
        for c in [&self.frames_captured, &self.frames_to_encoder, &self.frames_dropped, &self.frames_superseded] {
            c.store(0, Ordering::Relaxed);
        }
    }

    fn rolling(cell: &AtomicU64, sample_us: u64) {
        let prev = cell.load(Ordering::Relaxed);
        let next = if prev == 0 { sample_us } else { (prev * 9 + sample_us) / 10 };
        cell.store(next, Ordering::Relaxed);
    }
}

/// Shared slot for the latest preview image.
#[derive(Default)]
pub struct PreviewSlot {
    pub image: Mutex<Option<PreviewImage>>,
}

impl PreviewSlot {
    pub fn take(&self) -> Option<PreviewImage> {
        self.image.lock().unwrap().take()
    }
}

/// Called whenever a new preview image is available (e.g. to request a repaint).
pub type PreviewCallback = Arc<dyn Fn() + Send + Sync>;

/// Raises the system timer resolution to 1 ms while recording so the frame
/// clock and the encoder polling wake up on time (Windows defaults to 15.6 ms).
struct TimerResolution;

impl TimerResolution {
    fn acquire() -> Self {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Media::timeBeginPeriod;
            use windows::Win32::System::Threading::{
                GetCurrentProcess, SetProcessInformation, ProcessPowerThrottling, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
            };
            // Windows 11 ignores the request for occluded / minimized windows unless told otherwise.
            let state = PROCESS_POWER_THROTTLING_STATE {
                Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
                StateMask: 0,
            };
            let _ = SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &state as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
            timeBeginPeriod(1);
        }
        Self
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            windows::Win32::Media::timeEndPeriod(1);
        }
    }
}

struct FrameInboxState {
    queue: VecDeque<CapturedFrame>,
    closed: bool,
}

struct FrameInbox {
    state: Mutex<FrameInboxState>,
    cv: Condvar,
    pool: Arc<FramePool>,
    stats: Arc<Stats>,
}

struct FrameSender {
    inbox: Arc<FrameInbox>,
}

struct FrameReceiver {
    inbox: Arc<FrameInbox>,
}

fn recycle_captured(pool: &FramePool, frame: CapturedFrame) {
    match frame {
        CapturedFrame::Cpu(f) => pool.recycle(f.data),
        #[cfg(windows)]
        CapturedFrame::Gpu(g) => drop(g),
    }
}

fn frame_mailbox(pool: Arc<FramePool>, stats: Arc<Stats>) -> (FrameSender, FrameReceiver) {
    let inbox = Arc::new(FrameInbox {
        state: Mutex::new(FrameInboxState { queue: VecDeque::with_capacity(FRAME_QUEUE_CAP), closed: false }),
        cv: Condvar::new(),
        pool,
        stats,
    });
    (FrameSender { inbox: inbox.clone() }, FrameReceiver { inbox })
}

impl FrameSender {
    /// Enqueues `frame`. Only when the bounded queue reaches `FRAME_QUEUE_CAP`
    /// is the oldest unconsumed frame evicted and recycled, keeping WGC free of
    /// pool mutex contention during normal bursts while never dropping newer
    /// incoming frames. Returns `false` once the receiver has hung up.
    fn send(&self, frame: CapturedFrame) -> bool {
        let mut guard = self.inbox.state.lock().unwrap();
        if guard.closed {
            drop(guard);
            recycle_captured(&self.inbox.pool, frame);
            return false;
        }
        let evicted = (guard.queue.len() >= FRAME_QUEUE_CAP).then(|| guard.queue.pop_front()).flatten();
        guard.queue.push_back(frame);
        drop(guard);
        if let Some(old) = evicted {
            self.inbox.stats.frames_superseded.fetch_add(1, Ordering::Relaxed);
            recycle_captured(&self.inbox.pool, old);
        }
        self.inbox.cv.notify_one();
        true
    }
}

impl Drop for FrameSender {
    fn drop(&mut self) {
        let mut guard = self.inbox.state.lock().unwrap();
        guard.closed = true;
        drop(guard);
        self.inbox.cv.notify_all();
    }
}

impl FrameReceiver {
    fn try_recv(&self) -> std::result::Result<CapturedFrame, TryRecvError> {
        let mut guard = self.inbox.state.lock().unwrap();
        if let Some(f) = guard.queue.pop_front() {
            Ok(f)
        } else if guard.closed {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    fn recv_timeout(&self, timeout: Duration) -> std::result::Result<CapturedFrame, RecvTimeoutError> {
        let mut guard = self.inbox.state.lock().unwrap();
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(f) = guard.queue.pop_front() {
                return Ok(f);
            }
            if guard.closed {
                return Err(RecvTimeoutError::Disconnected);
            }
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            let (next_guard, res) = self.inbox.cv.wait_timeout(guard, rem).unwrap();
            guard = next_guard;
            if res.timed_out() && guard.queue.is_empty() && !guard.closed {
                return Err(RecvTimeoutError::Timeout);
            }
        }
    }
}

impl Drop for FrameReceiver {
    fn drop(&mut self) {
        let mut guard = self.inbox.state.lock().unwrap();
        guard.closed = true;
        let leftovers: Vec<_> = guard.queue.drain(..).collect();
        drop(guard);
        for f in leftovers {
            recycle_captured(&self.inbox.pool, f);
        }
        self.inbox.cv.notify_all();
    }
}

/// Ensures presentation timestamps are strictly monotonic (`pts > prev`).
fn monotonic_pts(prev: Option<Duration>, pts: Duration) -> Duration {
    match prev {
        Some(p) if pts <= p => p + Duration::from_micros(100),
        _ => pts,
    }
}

pub struct Recorder {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    /// Total paused wall time in microseconds (grows only at resume).
    paused_total_us: Arc<AtomicU64>,
    pause_started: Option<Instant>,
    capture: Option<CaptureHandle>,
    encode_thread: Option<JoinHandle<Result<()>>>,
    audio_thread: Option<JoinHandle<Result<()>>>,
    stats: Arc<Stats>,
    preview: Arc<PreviewSlot>,
    preview_visible: Arc<AtomicBool>,
    /// Live crop rect for a [`Source::Region`] recording, so the on-screen
    /// border can be dragged while recording. `None` for other sources.
    region: Option<LiveRect>,
    started: Instant,
    output: PathBuf,
    _timer: TimerResolution,
}

/// Recorded length, given the current recording-timebase clock and the timeline
/// origin. Extracted so the anchoring math is testable without a live session.
fn recorded_span(now: Duration, origin: Option<Duration>) -> Duration {
    origin.map(|o| now.saturating_sub(o)).unwrap_or_default()
}

impl Recorder {
    pub fn start(config: RecordConfig, on_preview: Option<PreviewCallback>) -> Result<Recorder> {
        let timer = TimerResolution::acquire();
        let epoch = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let paused_total_us = Arc::new(AtomicU64::new(0));
        let stats = Arc::new(Stats::default());
        let preview = Arc::new(PreviewSlot::default());
        let preview_visible = Arc::new(AtomicBool::new(false));
        let pool = FramePool::new(POOL_SIZE);

        let monitor_hz = config.is_auto_fps().then(|| source_refresh_hz(&config.source)).flatten();
        let nominal_fps = if config.is_auto_fps() {
            config.format.effective_fps_for(monitor_hz)
        } else {
            config.fps()
        };
        if config.is_auto_fps() {
            log::info!("auto fps: monitor refresh {monitor_hz:?} Hz -> nominal encoder fps {nominal_fps}");
        }

        let is_openh264 = config.format.video_codec == VideoCodec::OpenH264
            || (config.format.video_codec == VideoCodec::Auto
                && VideoCodec::resolve_auto(&available_encoders()) == VideoCodec::OpenH264);
        let gpu_off_reason: Option<&'static str> = if !cfg!(windows) {
            Some("non-Windows platform")
        } else if config.color_conversion == ColorConversion::Cpu {
            Some("color conversion set to CPU")
        } else if matches!(config.source, Source::Game { .. }) {
            Some("in-game hook capture")
        } else if is_openh264 {
            Some("OpenH264 CPU encoder selected")
        } else {
            None
        };
        let gpu_active = Arc::new(AtomicBool::new(gpu_off_reason.is_none()));
        if let Some(reason) = gpu_off_reason {
            log::info!("video capture pipeline: CPU readback ({reason})");
        }

        let (video_tx, video_rx) = frame_mailbox(pool.clone(), stats.clone());
        let (audio_tx, audio_rx) = mpsc::sync_channel::<(usize, AudioFrame)>(512);

        let sampler = config.mouse_fx.any_overlay().then(|| Arc::new(Mutex::new(MouseSampler::new())));
        // A region recording keeps its crop rect shared so `set_region` can move
        // it (never resize it) while the encoder runs at a fixed frame size.
        let region = match &config.source {
            Source::Region { rect, .. } => Some(LiveRect::new(*rect)),
            _ => None,
        };
        // Capture first: WGC/D3D initialisation and the first-frame latency now
        // overlap opening the audio devices instead of following them, so the
        // file starts sooner after the button press. On any error below the
        // handle drops and its `Drop` stops the backend.
        let capture = start_capture(
            &config,
            nominal_fps,
            epoch,
            video_tx,
            stats.clone(),
            sampler.clone(),
            pool.clone(),
            region.clone(),
            gpu_active.clone(),
            preview.clone(),
            preview_visible.clone(),
            on_preview.clone(),
        )?;

        let (audio_thread, audio_cfgs) = if config.wants_audio() {
            let (handle, cfgs) = spawn_audio_thread(AudioArgs {
                config: config.clone(),
                epoch,
                stop: stop.clone(),
                paused: paused.clone(),
                paused_total: paused_total_us.clone(),
                tx: audio_tx,
                stats: stats.clone(),
            })?;
            (Some(handle), cfgs)
        } else {
            drop(audio_tx);
            (None, Vec::new())
        };

        let encode_thread = spawn_encode_thread(EncodeArgs {
            config: config.clone(),
            nominal_fps,
            epoch,
            video_rx,
            audio_rx,
            audio_cfgs,
            stop: stop.clone(),
            paused: paused.clone(),
            paused_total: paused_total_us.clone(),
            stats: stats.clone(),
            preview: preview.clone(),
            preview_visible: preview_visible.clone(),
            on_preview,
            sampler,
            pool,
            region: region.clone(),
            gpu_active,
        })?;

        Ok(Recorder {
            stop,
            paused,
            paused_total_us,
            pause_started: None,
            capture: Some(capture),
            encode_thread: Some(encode_thread),
            audio_thread,
            stats,
            preview,
            preview_visible,
            region,
            started: epoch,
            output: config.output,
            _timer: timer,
        })
    }

    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    pub fn preview(&self) -> &Arc<PreviewSlot> {
        &self.preview
    }

    /// Tells the encode thread whether anyone is looking at previews.
    pub fn set_preview_visible(&self, visible: bool) {
        self.preview_visible.store(visible, Ordering::Relaxed);
    }

    /// Moves the captured region of a [`Source::Region`] recording; a no-op for
    /// other sources. `width`/`height` are ignored: the encoder, converter and
    /// container header are all fixed to the first frame's size, so only the
    /// crop origin may change once recording has started.
    pub fn set_region(&self, rect: Rect) {
        if let Some(live) = &self.region {
            let current = live.get();
            live.set(Rect { x: rect.x, y: rect.y, ..current });
        }
    }

    /// Wall time since [`Recorder::start`] minus everything spent paused — the
    /// timebase every frame and audio chunk timestamp lives in.
    fn recording_now(&self) -> Duration {
        let paused = Duration::from_micros(self.paused_total_us.load(Ordering::Relaxed))
            + self.pause_started.map(|t| t.elapsed()).unwrap_or_default();
        self.started.elapsed().saturating_sub(paused)
    }

    /// Length of the recording **as it appears in the file**: measured from the
    /// first captured frame, which is where the file's timeline starts — not
    /// from the button press, which is up to half a second earlier while the
    /// audio devices and the capture backend are still starting up.
    pub fn elapsed(&self) -> Duration {
        recorded_span(self.recording_now(), self.stats.stream_start.get())
    }

    pub fn is_paused(&self) -> bool {
        self.pause_started.is_some()
    }

    /// Pauses recording: frames and audio captured until [`Self::resume`] are discarded.
    pub fn pause(&mut self) {
        if self.pause_started.is_none() {
            self.pause_started = Some(Instant::now());
            self.paused.store(true, Ordering::SeqCst);
        }
    }

    /// Resumes recording; the paused wall time is removed from the timeline.
    pub fn resume(&mut self) {
        if let Some(t) = self.pause_started.take() {
            self.paused_total_us.fetch_add(t.elapsed().as_micros() as u64, Ordering::SeqCst);
            self.paused.store(false, Ordering::SeqCst);
        }
    }

    pub fn output(&self) -> &PathBuf {
        &self.output
    }

    /// True if a worker thread has terminated (with or without error).
    pub fn is_finished(&self) -> bool {
        self.encode_thread.as_ref().map(|t| t.is_finished()).unwrap_or(true)
    }

    /// Completely stops screen capture and audio capture threads, locking the recording stop pts.
    /// Audio streams are closed and joined. Encode thread remains running to finish draining frames.
    pub fn stop_capture_and_audio(&mut self) {
        self.resume();
        self.stats.stop_pts.set(self.recording_now());
        self.stop.store(true, Ordering::SeqCst);
        if let Some(capture) = self.capture.take()
            && let Err(e) = capture.stop()
        {
            log::warn!("capture stop: {e:#}");
        }
        if let Some(t) = self.audio_thread.take() {
            match t.join() {
                Ok(Err(e)) => self.stats.set_error(format!("audio: {e:#}")),
                Err(_) => self.stats.set_error("audio thread panicked"),
                _ => {}
            }
        }
    }

    /// Stops capture and audio, invokes `on_audio_stopped` (e.g. to play stop feedback tone),
    /// and then joins the encode thread to finalize the container.
    pub fn stop_with_feedback<F: FnOnce()>(mut self, on_audio_stopped: F) -> Result<PathBuf> {
        self.stop_capture_and_audio();
        on_audio_stopped();
        if let Some(t) = self.encode_thread.take() {
            match t.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(anyhow!("encode thread panicked")),
            }
        }
        if let Some(e) = self.stats.error() {
            return Err(anyhow!(e));
        }
        Ok(self.output.clone())
    }

    /// Stops recording and finalizes the file.
    pub fn stop(self) -> Result<PathBuf> {
        self.stop_with_feedback(|| {})
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop_capture_and_audio();
        if let Some(t) = self.encode_thread.take() {
            match t.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log::warn!("Recorder dropped: encode thread returned error: {e:#}"),
                Err(_) => log::error!("Recorder dropped: encode thread panicked"),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_capture(
    config: &RecordConfig,
    nominal_fps: u32,
    epoch: Instant,
    tx: FrameSender,
    stats: Arc<Stats>,
    sampler: Option<Arc<Mutex<MouseSampler>>>,
    pool: Arc<FramePool>,
    region: Option<LiveRect>,
    gpu_active: Arc<AtomicBool>,
    preview: Arc<PreviewSlot>,
    preview_visible: Arc<AtomicBool>,
    on_preview: Option<PreviewCallback>,
) -> Result<CaptureHandle> {
    let note_stats = stats.clone();
    let capture_fps = if config.is_auto_fps() {
        match config.source {
            Source::Game { .. } => nominal_fps,
            _ => 0,
        }
    } else {
        config.fps()
    };
    let cap_cfg = CaptureConfig {
        source: config.source.clone(),
        fps: capture_fps,
        show_cursor: config.mouse_fx.native_cursor(),
        pool: Some(pool),
        live_region: region.clone(),
    };
    let mut handle = {
        #[cfg(windows)]
        {
            let origin_tracker = Arc::new(crate::capture::monitors::OriginTracker::new(config.source.clone(), region.clone()));
            let gpu_opts = gpu_active.load(Ordering::Relaxed).then(|| {
                let prev_slot = preview;
                let prev_cb = on_preview;
                capture::GpuCaptureOptions {
                    size_mode: config.format.size,
                    gpu_active,
                    preview_visible,
                    on_preview: Box::new(move |img| {
                        *prev_slot.image.lock().unwrap() = Some(img);
                        if let Some(cb) = &prev_cb {
                            cb();
                        }
                    }),
                    mouse_fx: config.mouse_fx.clone(),
                    watermark: config.watermark.clone(),
                    sampler: sampler.clone(),
                    origin_tracker: Some(origin_tracker),
                }
            });
            let sink: capture::windows::CapturedSink = Box::new(move |mut frame| {
                stats.frames_captured.fetch_add(1, Ordering::Relaxed);
                // Sample the pointer right when the frame arrives so effects line up
                // with the captured image even if encoding lags behind.
                if let Some(s) = &sampler
                    && let CapturedFrame::Cpu(ref mut raw) = frame
                {
                    raw.mouse = Some(s.lock().unwrap().snapshot());
                }
                tx.send(frame)
            });
            capture::windows::start_with_gpu(cap_cfg, gpu_opts, epoch, sink).context("starting screen capture")?
        }
        #[cfg(not(windows))]
        {
            let _ = (gpu_active, preview, preview_visible, on_preview);
            let sink: capture::FrameSink = Box::new(move |mut frame| {
                stats.frames_captured.fetch_add(1, Ordering::Relaxed);
                if let Some(s) = &sampler {
                    frame.mouse = Some(s.lock().unwrap().snapshot());
                }
                tx.send(CapturedFrame::Cpu(frame))
            });
            capture::start(cap_cfg, epoch, sink).context("starting screen capture")?
        }
    };
    // Whatever the backend could not honour (an OS without the capture-border
    // or cursor setting) shows up next to the encoder notes.
    if let Some(n) = handle.take_note() {
        note_stats.add_note(n);
    }
    Ok(handle)
}

/// Frame timeline: slot `k` in the current epoch is presented at `pts0 + k / fps`
/// (exact rational arithmetic, so long recordings never drift). Adaptive grid
/// retargeting via [`Self::set_fps`] re-anchors the epoch right after the last
/// emitted frame so cadence changes never introduce backward jumps or bursts.
#[derive(Debug, Clone)]
pub struct FrameClock {
    fps: u64,
    pts0_ns: u64,
    /// Next slot to encode in the current epoch.
    pub next: u64,
    /// PTS of the most recently emitted slot across grid changes.
    last_emitted_pts_ns: Option<u64>,
}

const NS: u64 = 1_000_000_000;

impl FrameClock {
    /// Encoding may lag this many slots before slots are skipped.
    pub const MAX_BEHIND: u64 = 2;

    pub fn new(fps: u32, pts0: Duration) -> Self {
        Self { fps: fps.max(1) as u64, pts0_ns: pts0.as_nanos() as u64, next: 0, last_emitted_pts_ns: None }
    }

    pub fn fps(&self) -> u32 {
        self.fps as u32
    }

    pub fn mark_emitted(&mut self, pts: Duration) {
        self.last_emitted_pts_ns = Some(pts.as_nanos() as u64);
    }

    /// Retargets the frame clock to `new_fps` smoothly from the last emitted
    /// frame timestamp onward, guaranteeing strictly monotonic `>= 1 / new_fps`
    /// spacing across grid transitions.
    pub fn set_fps(&mut self, new_fps: u32) -> bool {
        let new_fps = new_fps.max(1) as u64;
        if new_fps == self.fps {
            return false;
        }
        let new_interval_ns = NS / new_fps;
        if let Some(prev_ns) = self.last_emitted_pts_ns {
            self.pts0_ns = prev_ns + new_interval_ns;
            self.next = 0;
        } else {
            self.pts0_ns += self.next * NS / self.fps;
            self.next = 0;
        }
        self.fps = new_fps;
        true
    }

    pub fn interval(&self) -> Duration {
        Duration::from_nanos(NS / self.fps)
    }

    pub fn slot_pts(&self, k: u64) -> Duration {
        Duration::from_nanos(self.pts0_ns + k * NS / self.fps)
    }

    /// When slot `k` must be encoded: half an interval after its pts, so a
    /// frame captured anywhere around the slot time can still be used.
    pub fn deadline(&self, k: u64) -> Duration {
        Duration::from_nanos(self.pts0_ns + (2 * k + 1) * NS / (2 * self.fps))
    }

    /// Slots (from `next`) whose deadline has passed at `now`.
    pub fn due(&self, now: Duration) -> u64 {
        let now = now.as_nanos() as u64;
        if now < self.pts0_ns {
            return 0;
        }
        // Largest k with deadline(k) <= now: (2k+1)/(2 fps) <= t  ⇔  k <= (2 t fps − 1)/2.
        let twice = (now - self.pts0_ns) * 2 * self.fps / NS;
        if twice == 0 {
            return 0;
        }
        let k_max = (twice - 1) / 2;
        (k_max + 1).saturating_sub(self.next)
    }

    /// Skips slots so that at most one is due; returns how many were skipped.
    pub fn catch_up(&mut self, now: Duration) -> u64 {
        let due = self.due(now);
        if due > Self::MAX_BEHIND {
            let skipped = due - 1;
            self.next += skipped;
            skipped
        } else {
            0
        }
    }

    /// After a pause: continue with the slot nearest to `now` (never backwards).
    pub fn resync(&mut self, now: Duration) {
        self.snap_to_pts(now);
    }

    /// Advances `next` to the slot nearest to `pts` without moving backwards,
    /// so resuming after a static pause in Auto mode stays aligned to the grid
    /// without counting idle time as skipped slots.
    pub fn snap_to_pts(&mut self, pts: Duration) {
        let pts_ns = pts.as_nanos() as u64;
        if pts_ns > self.pts0_ns {
            let k = ((pts_ns - self.pts0_ns) as u128 * self.fps as u128 + (NS / 2) as u128) / NS as u128;
            self.next = self.next.max(k as u64);
        }
    }
}

struct EncodeArgs {
    config: RecordConfig,
    nominal_fps: u32,
    epoch: Instant,
    video_rx: FrameReceiver,
    audio_rx: Receiver<(usize, AudioFrame)>,
    audio_cfgs: Vec<AudioTrackConfig>,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    paused_total: Arc<AtomicU64>,
    stats: Arc<Stats>,
    preview: Arc<PreviewSlot>,
    preview_visible: Arc<AtomicBool>,
    on_preview: Option<PreviewCallback>,
    /// Shared with the capture sink; used here only for repeated frames.
    sampler: Option<Arc<Mutex<MouseSampler>>>,
    pool: Arc<FramePool>,
    /// Live crop rect of a region recording; used to keep mouse effects aligned
    /// while the region is dragged.
    region: Option<LiveRect>,
    gpu_active: Arc<AtomicBool>,
}

fn spawn_encode_thread(args: EncodeArgs) -> Result<JoinHandle<Result<()>>> {
    let stats = args.stats.clone();
    Ok(std::thread::Builder::new().name("openclip-encode".into()).spawn(move || {
        let r = encode_loop(args);
        if let Err(e) = &r {
            stats.set_error(format!("encoding: {e:#}"));
        }
        r
    })?)
}

fn encode_loop(args: EncodeArgs) -> Result<()> {
    let EncodeArgs {
        config,
        nominal_fps,
        epoch,
        video_rx,
        audio_rx,
        audio_cfgs,
        stop,
        paused,
        paused_total,
        stats,
        preview,
        preview_visible,
        on_preview,
        sampler,
        pool,
        region,
        gpu_active,
    } = args;
    let mut audio_cfgs = Some(audio_cfgs);
    // Recording-time clock: wall time since epoch minus paused time.
    let paused_dur = || Duration::from_micros(paused_total.load(Ordering::Relaxed));
    let rec_now = || epoch.elapsed().saturating_sub(paused_dur());

    let push_audio = |mux: &mut Muxer, stats: &Stats| -> Result<()> {
        if mux.has_audio() {
            while let Ok((track, f)) = audio_rx.try_recv() {
                mux.push_audio_track(track, &f)?;
                stats.audio_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    };
    let take_newest = |newest: &mut Option<CapturedFrame>, f: CapturedFrame, pool: &FramePool, stats: &Stats| {
        if let Some(old) = newest.replace(f) {
            recycle_captured(pool, old);
            stats.frames_superseded.fetch_add(1, Ordering::Relaxed);
        }
    };

    // Wait for the very first captured frame, which defines the timeline origin
    // and the resolution everything downstream is built around.
    let mut first_frame: Option<CapturedFrame> = None;
    while first_frame.is_none() {
        let stopping = stop.load(Ordering::Relaxed);
        if paused.load(Ordering::Relaxed) && !stopping {
            match video_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(f) => recycle_captured(&pool, f),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            continue;
        }
        match video_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(mut f) => {
                // Drain if a newer frame arrived right as we woke up.
                while let Ok(newer) = video_rx.try_recv() {
                    recycle_captured(&pool, f);
                    f = newer;
                }
                let adj = f.pts().saturating_sub(paused_dur());
                f.set_pts(adj);
                log::info!(
                    "timeline starts at {:.0} ms (capture latency {:.0} ms)",
                    f.pts().as_secs_f64() * 1e3,
                    rec_now().saturating_sub(f.pts()).as_secs_f64() * 1e3
                );
                first_frame = Some(f);
            }
            Err(RecvTimeoutError::Timeout) => {
                if stopping {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let Some(first) = first_frame else {
        return Err(anyhow!("no frames were captured"));
    };

    let first_pts = first.pts();
    let InitEncodePipeline {
        cpu_spec,
        mut core,
        cpu: init_cpu,
        fallback_nv12: init_nv12,
        resolved_fps,
    } = init_encode_pipeline(
        &config,
        nominal_fps,
        &first,
        audio_cfgs.take().unwrap_or_default(),
        &stats,
        &gpu_active,
    )?;
    let encoder_uses_d3d11 = core.encoder.supports_d3d11();
    let shared_grid_fps = Arc::new(AtomicU32::new(resolved_fps));
    stats.width.store(cpu_spec.dims.0 as u64, Ordering::Relaxed);
    stats.height.store(cpu_spec.dims.1 as u64, Ordering::Relaxed);
    let is_auto = config.is_auto_fps();
    stats.target_fps.store(resolved_fps as u64, Ordering::Relaxed);
    // Counters so far describe the warm-up, not the file.
    stats.reset_pre_roll();
    stats.stream_start.set(first_pts);

    struct FallbackNv12 {
        y: Vec<u8>,
        uv: Vec<u8>,
        stride_y: usize,
        stride_uv: usize,
        dims: (u32, u32),
    }

    enum PreparedSlot {
        #[cfg(windows)]
        Gpu {
            slot: u64,
            pts: Duration,
            prep_us: u64,
            vp_submit_us: u64,
            frame: capture::GpuFrame,
        },
        #[cfg(windows)]
        GpuRepeat {
            slot: u64,
            pts: Duration,
            prep_us: u64,
        },
        Cpu {
            slot: u64,
            pts: Duration,
            prep_us: u64,
            conv: Converter,
        },
        CpuRepeat {
            slot: u64,
            pts: Duration,
            prep_us: u64,
        },
        FallbackNv12 {
            slot: u64,
            pts: Duration,
            prep_us: u64,
            nv12: FallbackNv12,
        },
    }

    let (prep_tx, prep_rx) = mpsc::sync_channel::<PreparedSlot>(1);
    let (recycle_tx, recycle_rx) = mpsc::sync_channel::<Converter>(2);
    let mut cpu_prep = if let Some((prep, conv1, conv2)) = init_cpu {
        recycle_tx.send(conv1).expect("recycle channel has capacity 2");
        recycle_tx.send(conv2).expect("recycle channel has capacity 2");
        Some(prep)
    } else {
        None
    };
    let abort_prep = Arc::new(AtomicBool::new(false));
    let mut clock = FrameClock::new(resolved_fps, first_pts);

    std::thread::scope(|scope| -> Result<()> {
        let mut origin = source_origin(&config.source).unwrap_or((0, 0));
        // A dragged region moves under us: resolve its monitor's corner once and add
        // the live offset per frame instead of re-querying the monitor list.
        let region_origin = match (&config.source, &region) {
            (Source::Region { monitor_id, .. }, Some(_)) => source_origin(&Source::Monitor { id: *monitor_id }).ok(),
            _ => None,
        };
        let prep_stop = stop.clone();
        let prep_paused = paused.clone();
        let prep_abort = abort_prep.clone();
        let prep_stats = stats.clone();
        let prep_config = &config;
        let prep_preview = &preview;
        let prep_preview_visible = &preview_visible;
        let prep_on_preview = &on_preview;
        let prep_sampler = &sampler;
        let prep_pool = &pool;
        let prep_region = &region;
        let prep_spec = &cpu_spec;
        let prep_grid_fps = shared_grid_fps.clone();
        let mut prep_recycle_tx = Some(recycle_tx.clone());

        let prep_handle = std::thread::Builder::new()
            .name("openclip-prep".into())
            .spawn_scoped(scope, move || -> Result<()> {
                let mut fallback_first: Option<FallbackNv12> = init_nv12.map(|(y, uv, sy, suv)| FallbackNv12 {
                    y,
                    uv,
                    stride_y: sy,
                    stride_uv: suv,
                    dims: prep_spec.dims,
                });
                let mut newest: Option<CapturedFrame> = if fallback_first.is_some() {
                    drop(first);
                    None
                } else {
                    Some(first)
                };
                let mut last_was_gpu = false;
                let mut gpu_resize_noted = false;
                let mut frames_since_origin = 0u32;
                let mut was_paused = false;
                let mut auto_idle = false;
                let mut last_preview = Instant::now() - PREVIEW_INTERVAL;
                let mut disconnected = false;
                let mut prep_err: Option<anyhow::Error> = None;

                loop {
                    let stopping = prep_stop.load(Ordering::Relaxed);
                    if prep_abort.load(Ordering::Relaxed) {
                        break;
                    }
                    // ----- pause: discard what arrives, keep the timeline frozen -----
                    if prep_paused.load(Ordering::Relaxed) && !stopping {
                        was_paused = true;
                        match video_rx.recv_timeout(Duration::from_millis(20)) {
                            Ok(f) => recycle_captured(prep_pool, f),
                            Err(RecvTimeoutError::Disconnected) => disconnected = true,
                            Err(RecvTimeoutError::Timeout) => {}
                        }
                        if disconnected {
                            break;
                        }
                        continue;
                    }
                    if was_paused {
                        was_paused = false;
                        auto_idle = false;
                        // Frames queued before the pause ended belong to paused time.
                        while let Ok(f) = video_rx.try_recv() {
                            recycle_captured(prep_pool, f);
                        }
                        if let Some(f) = newest.take() {
                            recycle_captured(prep_pool, f);
                        }
                        clock.resync(rec_now());
                    }

                    // ----- collect frames until ready -----
                    clock.set_fps(prep_grid_fps.load(Ordering::Relaxed));
                    let mut force_fx_frame = false;
                    if is_auto && auto_idle {
                        while let Ok(mut f) = video_rx.try_recv() {
                            let adj = f.pts().saturating_sub(paused_dur());
                            f.set_pts(adj);
                            take_newest(&mut newest, f, prep_pool, &prep_stats);
                        }
                        while newest.is_none()
                            && !prep_stop.load(Ordering::Relaxed)
                            && !disconnected
                            && !prep_abort.load(Ordering::Relaxed)
                        {
                            let fx_animating = cpu_prep.as_ref().is_some_and(|p| {
                                p.frame.is_some()
                                    && prep_sampler.is_some()
                                    && (!prep_config.mouse_fx.native_cursor() || p.has_active_clicks())
                            });
                            let wait_dur = if fx_animating {
                                Duration::from_millis(16)
                            } else {
                                Duration::from_millis(50)
                            };
                            match video_rx.recv_timeout(wait_dur) {
                                Ok(mut f) => {
                                    let adj = f.pts().saturating_sub(paused_dur());
                                    f.set_pts(adj);
                                    take_newest(&mut newest, f, prep_pool, &prep_stats);
                                }
                                Err(RecvTimeoutError::Timeout) => {
                                    if prep_paused.load(Ordering::Relaxed) || prep_stop.load(Ordering::Relaxed) {
                                        break;
                                    }
                                    // Keep non-native cursor or active click ripples animating even
                                    // if WGC is idle, without duplicating static frames otherwise.
                                    if fx_animating
                                        && let (Some(s), Some(p)) = (prep_sampler, cpu_prep.as_mut())
                                    {
                                        let snap = s.lock().unwrap().snapshot();
                                        let (cursor, clicks) = snap.mapped(origin, p.fx_scale);
                                        if p.fx_changed(cursor, &clicks) {
                                            if let Some(frame) = p.frame.as_mut() {
                                                frame.mouse = Some(snap);
                                            }
                                            force_fx_frame = true;
                                            break;
                                        }
                                    }
                                }
                                Err(RecvTimeoutError::Disconnected) => {
                                    disconnected = true;
                                    break;
                                }
                            }
                        }
                        clock.set_fps(prep_grid_fps.load(Ordering::Relaxed));
                        if let Some(f) = newest.as_ref() {
                            let wake_now = rec_now();
                            // If Readback flushed an older pre-idle staged frame, anchor at wall time
                            // so returning from static idle does not trigger fake `slots_skipped`.
                            let snap_pts = if wake_now.saturating_sub(f.pts()) > clock.interval() * 2 {
                                wake_now
                            } else {
                                f.pts()
                            };
                            clock.snap_to_pts(snap_pts);
                            auto_idle = false;
                        } else if force_fx_frame {
                            clock.snap_to_pts(rec_now());
                            auto_idle = false;
                        }
                    }
                    if (!is_auto || (!auto_idle && !force_fx_frame)) && fallback_first.is_none() {
                        let deadline = clock.deadline(clock.next);
                        loop {
                            match video_rx.try_recv() {
                                Ok(mut f) => {
                                    let adj = f.pts().saturating_sub(paused_dur());
                                    f.set_pts(adj);
                                    take_newest(&mut newest, f, prep_pool, &prep_stats);
                                }
                                Err(TryRecvError::Empty) => break,
                                Err(TryRecvError::Disconnected) => {
                                    disconnected = true;
                                    break;
                                }
                            }
                        }
                        loop {
                            let now = rec_now();
                            if now >= deadline
                                || stopping
                                || prep_stop.load(Ordering::Relaxed)
                                || disconnected
                                || prep_abort.load(Ordering::Relaxed)
                            {
                                break;
                            }
                            let wait = (deadline - now).min(Duration::from_millis(50));
                            match video_rx.recv_timeout(wait) {
                                Ok(mut f) => {
                                    let adj = f.pts().saturating_sub(paused_dur());
                                    f.set_pts(adj);
                                    take_newest(&mut newest, f, prep_pool, &prep_stats);
                                }
                                Err(RecvTimeoutError::Timeout) => {
                                    if rec_now() >= deadline {
                                        break;
                                    }
                                }
                                Err(RecvTimeoutError::Disconnected) => {
                                    disconnected = true;
                                    break;
                                }
                            }
                        }
                    }
                    if prep_abort.load(Ordering::Relaxed) {
                        break;
                    }
                    let stopping = stopping || prep_stop.load(Ordering::Relaxed);
                    if stopping || disconnected {
                        // Take whatever is still queued so the last captured frame is not lost.
                        while let Ok(mut f) = video_rx.try_recv() {
                            let adj = f.pts().saturating_sub(paused_dur());
                            f.set_pts(adj);
                            take_newest(&mut newest, f, prep_pool, &prep_stats);
                        }
                        if is_auto && auto_idle && let Some(f) = newest.as_ref() {
                            let wake_now = rec_now();
                            let snap_pts = if wake_now.saturating_sub(f.pts()) > clock.interval() * 2 {
                                wake_now
                            } else {
                                f.pts()
                            };
                            clock.snap_to_pts(snap_pts);
                            auto_idle = false;
                        }
                    }

                    // ----- prepare one slot (GPU passthrough OR CPU scale + overlays + YUV conversion) -----
                    let now = rec_now();
                    if let Some(nv12) = fallback_first.take() {
                        let slot = clock.next;
                        let pts = clock.slot_pts(slot);
                        if prep_tx.send(PreparedSlot::FallbackNv12 { slot, pts, prep_us: 0, nv12 }).is_err() {
                            break;
                        }
                        clock.mark_emitted(pts);
                        clock.next += 1;
                        continue;
                    }

                    let have_new = newest.is_some();
                    if stopping && !have_new && !force_fx_frame {
                        break;
                    }
                    if is_auto && !have_new && !force_fx_frame {
                        auto_idle = true;
                        continue;
                    }
                    let skipped = if is_auto && force_fx_frame { 0 } else { clock.catch_up(now) };
                    if skipped > 0 {
                        prep_stats.slots_skipped.fetch_add(skipped, Ordering::Relaxed);
                    }
                    if !have_new && clock.due(now) == 0 && !(is_auto && force_fx_frame) {
                        continue;
                    }

                    let slot = clock.next;
                    let pts = clock.slot_pts(slot);
                    let taken = newest.take();

                    let msg = match taken {
                        #[cfg(windows)]
                        Some(CapturedFrame::Gpu(gf)) => {
                            last_was_gpu = encoder_uses_d3d11;
                            if gf.resized_from_initial && !gpu_resize_noted {
                                gpu_resize_noted = true;
                                prep_stats.add_note(crate::t!(NOTE_SOURCE_RESIZED, prep_spec.scaled.0, prep_spec.scaled.1));
                            }
                            PreparedSlot::Gpu {
                                slot,
                                pts,
                                prep_us: gf.prep_us,
                                vp_submit_us: gf.vp_submit_us,
                                frame: gf,
                            }
                        }
                        None if last_was_gpu => {
                            prep_stats.frames_repeated.fetch_add(1, Ordering::Relaxed);
                            PreparedSlot::GpuRepeat { slot, pts, prep_us: 0 }
                        }
                        other_cpu => {
                            if encoder_uses_d3d11 || last_was_gpu {
                                if let Some(f) = other_cpu {
                                    recycle_captured(prep_pool, f);
                                }
                                prep_err = Some(anyhow!("GPU capture unexpectedly fell back to CPU mid-recording"));
                                break;
                            }
                            let mut lazy_conv: Option<Converter> = None;
                            if cpu_prep.is_none() {
                                match build_cpu_prep(prep_config, prep_spec) {
                                    Ok((p, c1, c2)) => {
                                        if let Some(tx) = prep_recycle_tx.take() {
                                            let _ = tx.send(c2);
                                        }
                                        lazy_conv = Some(c1);
                                        cpu_prep = Some(p);
                                    }
                                    Err(e) => {
                                        if let Some(f) = other_cpu {
                                            recycle_captured(prep_pool, f);
                                        }
                                        prep_err = Some(e);
                                        break;
                                    }
                                }
                            }
                            let p = cpu_prep.as_mut().unwrap();
                            let p0 = Instant::now();
                            let captured_raw = match other_cpu {
                                Some(CapturedFrame::Cpu(r)) => Some(r),
                                #[cfg(windows)]
                                Some(CapturedFrame::Gpu(_)) => unreachable!(),
                                None => None,
                            };
                            if let Some(r) = captured_raw {
                                p.accept(r, prep_pool, &prep_stats);
                            } else {
                                prep_stats.frames_repeated.fetch_add(1, Ordering::Relaxed);
                                // Static screen repeat: if CPU frame was not yet received after a
                                // frame-0 GPU fallback, let encode_loop repeat the fallback NV12 frame.
                                if p.frame.is_none() {
                                    if prep_tx.send(PreparedSlot::CpuRepeat { slot, pts, prep_us: 0 }).is_err() {
                                        break;
                                    }
                                    clock.mark_emitted(pts);
                                    clock.next += 1;
                                    continue;
                                }
                                if let (Some(s), Some(fx)) = (prep_sampler, p.frame.as_mut()) {
                                    fx.mouse = Some(s.lock().unwrap().snapshot());
                                }
                            }

                            let snap = p.frame.as_mut().expect("frame present after accept").mouse.take();
                            let mut fx = None;
                            if let Some(snap) = snap.filter(|_| prep_sampler.is_some()) {
                                if let (Some(base), Some(live)) = (region_origin, prep_region) {
                                    let r = live.get();
                                    origin = (base.0 + r.x as i32, base.1 + r.y as i32);
                                } else if matches!(prep_config.source, Source::Window { .. }) {
                                    frames_since_origin += 1;
                                    if frames_since_origin >= 30 {
                                        frames_since_origin = 0;
                                        if let Ok(o) = source_origin(&prep_config.source) {
                                            origin = o;
                                        }
                                    }
                                }
                                let (cursor, clicks) = snap.mapped(origin, p.fx_scale);
                                let changed = p.fx_changed(cursor, &clicks);
                                fx = Some((cursor, clicks, changed));
                            }
                            let mut painted = false;
                            if p.converted_is_new || fx.as_ref().is_some_and(|(_, _, changed)| *changed) {
                                let frame = p.frame.as_mut().unwrap();
                                if let Some(wm) = &mut p.watermark {
                                    wm.paint(&prep_config.watermark, frame, &mut p.patches);
                                }
                                if let Some((cursor, clicks, _)) = &fx {
                                    let scale = p.fx_scale.0.min(p.fx_scale.1);
                                    prep_config.mouse_fx.paint(frame, *cursor, clicks, scale, &mut p.patches);
                                }
                                painted = !p.patches.is_empty();
                            }
                            if let Some((cursor, clicks, _)) = fx {
                                p.last_fx = Some((cursor, clicks));
                            }

                            let need_convert = p.converted_is_new || painted;
                            let mut converted = None;
                            if need_convert {
                                let conv_opt = lazy_conv.take().or_else(|| recycle_rx.recv().ok());
                                let Some(mut conv) = conv_opt else {
                                    break;
                                };
                                if let Err(e) = conv.convert(p.frame.as_ref().unwrap()) {
                                    prep_err = Some(e);
                                    break;
                                }
                                p.converted_is_new = false;
                                converted = Some(conv);
                            }
                            let convert_done = Instant::now();

                            // Build GUI preview while overlays are still painted (skipped if behind).
                            if prep_preview_visible.load(Ordering::Relaxed)
                                && p0.duration_since(last_preview) >= PREVIEW_INTERVAL
                                && (is_auto || clock.due(rec_now()) == 0)
                            {
                                last_preview = p0;
                                let img = p.previewer.make(p.frame.as_ref().unwrap());
                                *prep_preview.image.lock().unwrap() = Some(img);
                                if let Some(cb) = prep_on_preview {
                                    cb();
                                }
                            }

                            // Restore overlay patches immediately now that both YUV conversion
                            // and preview took their snapshot, keeping the reusable frame clean.
                            let r0 = Instant::now();
                            if painted {
                                MouseFx::restore(p.frame.as_mut().unwrap(), &mut p.patches);
                            }
                            let prep_us = (convert_done - p0 + r0.elapsed()).as_micros() as u64;

                            match converted {
                                Some(conv) => PreparedSlot::Cpu { slot, pts, prep_us, conv },
                                None => PreparedSlot::CpuRepeat { slot, pts, prep_us },
                            }
                        }
                    };

                    if prep_tx.send(msg).is_err() {
                        break;
                    }
                    clock.mark_emitted(pts);
                    clock.next += 1;
                    if disconnected && newest.is_none() {
                        break;
                    }
                }

                if let Some(f) = newest.take() {
                    recycle_captured(prep_pool, f);
                }
                if let Some(mut p) = cpu_prep
                    && let Some(f) = p.frame.take()
                {
                    prep_pool.recycle(f.data);
                }
                drop(prep_tx);
                match prep_err {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            })?;

        let mut last_conv: Option<Converter> = None;
        let mut last_fallback_nv12: Option<FallbackNv12> = None;
        #[cfg(windows)]
        let mut last_gpu: Option<Arc<GpuNv12Slot>> = None;
        let mut adaptive_grid = AdaptiveFrameGrid::new(resolved_fps, is_auto);
        let mut sec_acc = SecTimingAccum::default();
        let mut last_report = Instant::now();
        let mut last_sec_report = Instant::now();
        let mut prev_cap = stats.frames_captured.load(Ordering::Relaxed);
        let mut prev_enc_in = stats.frames_to_encoder.load(Ordering::Relaxed);
        let mut prev_slot_done = stats.slots_done.load(Ordering::Relaxed);
        let mut prev_slot_skip = stats.slots_skipped.load(Ordering::Relaxed);
        let mut prev_drop = stats.frames_dropped.load(Ordering::Relaxed);
        let mut prev_sup = stats.frames_superseded.load(Ordering::Relaxed);
        let gpu_mode_label = if encoder_uses_d3d11 { "GPU" } else { "CPU" };
        let mut maybe_log_sec = |acc: &mut SecTimingAccum, grid: &mut AdaptiveFrameGrid| {
            let now = Instant::now();
            let dt = now.saturating_duration_since(last_sec_report);
            if dt >= Duration::from_secs(1) {
                let cur_cap = stats.frames_captured.load(Ordering::Relaxed);
                let cur_enc_in = stats.frames_to_encoder.load(Ordering::Relaxed);
                let cur_done = stats.slots_done.load(Ordering::Relaxed);
                let cur_skip = stats.slots_skipped.load(Ordering::Relaxed);
                let cur_drop = stats.frames_dropped.load(Ordering::Relaxed);
                let cur_sup = stats.frames_superseded.load(Ordering::Relaxed);
                let delta_done = cur_done.saturating_sub(prev_slot_done);
                let delta_skip = cur_skip.saturating_sub(prev_slot_skip);
                let avg_work_us = acc.avg_bottleneck_us();
                let summary = acc.take_summary();
                log::info!(
                    "1s rates ({:.2} s): wgc {} encoder_in {} skipped {} dropped {} superseded {} | mode {} grid {}fps | {summary}",
                    dt.as_secs_f64(),
                    cur_cap.saturating_sub(prev_cap),
                    cur_enc_in.saturating_sub(prev_enc_in),
                    delta_skip,
                    cur_drop.saturating_sub(prev_drop),
                    cur_sup.saturating_sub(prev_sup),
                    gpu_mode_label,
                    grid.current_fps(),
                );
                if paused.load(Ordering::Relaxed) {
                    grid.reset_streaks();
                } else if let GridAction::Step { from, to, reason } =
                    grid.observe(dt, delta_done, delta_skip, avg_work_us, now)
                {
                    log::info!("adaptive frame grid: {from} -> {to} fps ({reason})");
                    shared_grid_fps.store(to, Ordering::Relaxed);
                    stats.target_fps.store(to as u64, Ordering::Relaxed);
                }
                prev_cap = cur_cap;
                prev_enc_in = cur_enc_in;
                prev_slot_done = cur_done;
                prev_slot_skip = cur_skip;
                prev_drop = cur_drop;
                prev_sup = cur_sup;
                last_sec_report = now;
            }
        };

        let encode_fallback_nv12 = |enc: &mut dyn VideoEncoder, nv12: &FallbackNv12, pts: Duration| -> Result<Vec<EncodedFrame>> {
            match enc.input_layout() {
                InputLayout::Nv12 => enc.encode(
                    FrameInput::Nv12 {
                        y: &nv12.y,
                        uv: &nv12.uv,
                        strides: (nv12.stride_y, nv12.stride_uv),
                        dims: nv12.dims,
                    },
                    pts,
                ),
                InputLayout::I420 => {
                    let w = nv12.dims.0 as usize;
                    let h = nv12.dims.1 as usize;
                    let uv_w = w / 2;
                    let uv_h = h / 2;
                    let mut u = Vec::with_capacity(uv_w * uv_h);
                    let mut v = Vec::with_capacity(uv_w * uv_h);
                    for row in 0..uv_h {
                        let line = &nv12.uv[row * nv12.stride_uv..row * nv12.stride_uv + w];
                        for pair in line.as_chunks::<2>().0 {
                            u.push(pair[0]);
                            v.push(pair[1]);
                        }
                    }
                    enc.encode(
                        FrameInput::I420 {
                            y: &nv12.y,
                            u: &u,
                            v: &v,
                            strides: (nv12.stride_y, uv_w, uv_w),
                            dims: nv12.dims,
                        },
                        pts,
                    )
                }
            }
        };

        let encode_res = (|| -> Result<()> {
            loop {
                match prep_rx.recv_timeout(Duration::from_millis(25)) {
                    Ok(item) => {
                        #[cfg(windows)]
                        let (slot, prep_us, vp_submit_us, gpu_wait_us, e0, encoded) = match item {
                            PreparedSlot::Gpu { slot, pts, prep_us, vp_submit_us, frame } => {
                                if core.encoder.supports_d3d11() {
                                    last_gpu = Some(frame.slot.clone());
                                    let gpu_wait_us = frame.slot.wait_complete(&frame.context);
                                    let e0 = Instant::now();
                                    stats.frames_to_encoder.fetch_add(1, Ordering::Relaxed);
                                    let encoded = core.encoder.encode_d3d11(&frame.slot.texture, pts)?;
                                    (slot, prep_us, vp_submit_us, gpu_wait_us, e0, encoded)
                                } else {
                                    // Rare startup race: a second GPU frame queued while frame-0 init
                                    // fell back to CPU encoder. Read it back once without reopening the encoder.
                                    let gpu_wait_us = frame.slot.wait_complete(&frame.context);
                                    let (y, uv, stride_y, stride_uv) = crate::video::d3d11_vp::readback_nv12_texture(
                                        &frame.device,
                                        &frame.slot.texture,
                                        cpu_spec.dims.0,
                                        cpu_spec.dims.1,
                                    )?;
                                    let nv12 = FallbackNv12 { y, uv, stride_y, stride_uv, dims: cpu_spec.dims };
                                    let e0 = Instant::now();
                                    stats.frames_to_encoder.fetch_add(1, Ordering::Relaxed);
                                    let encoded = encode_fallback_nv12(core.encoder.as_mut(), &nv12, pts)?;
                                    last_fallback_nv12 = Some(nv12);
                                    (slot, prep_us, vp_submit_us, gpu_wait_us, e0, encoded)
                                }
                            }
                            PreparedSlot::GpuRepeat { slot, pts, prep_us } => {
                                if !core.encoder.supports_d3d11() {
                                    bail!("GPU repeat slot dispatched to CPU encoder");
                                }
                                let e0 = Instant::now();
                                let gpu_slot = last_gpu.as_ref().expect("first GPU slot sets last_gpu");
                                stats.frames_to_encoder.fetch_add(1, Ordering::Relaxed);
                                let encoded = core.encoder.encode_d3d11(&gpu_slot.texture, pts)?;
                                (slot, prep_us, 0, 0, e0, encoded)
                            }
                            PreparedSlot::FallbackNv12 { slot, pts, prep_us, nv12 } => {
                                let e0 = Instant::now();
                                stats.frames_to_encoder.fetch_add(1, Ordering::Relaxed);
                                let encoded = encode_fallback_nv12(core.encoder.as_mut(), &nv12, pts)?;
                                last_fallback_nv12 = Some(nv12);
                                (slot, prep_us, 0, 0, e0, encoded)
                            }
                            PreparedSlot::Cpu { slot, pts, prep_us, conv } => {
                                last_gpu = None;
                                last_fallback_nv12 = None;
                                if let Some(old) = last_conv.replace(conv) {
                                    // Return the previous converter before starting encode
                                    // so `openclip-prep` can convert the next frame concurrently.
                                    let _ = recycle_tx.send(old);
                                }
                                let conv = last_conv.as_ref().expect("just set");
                                if core.encoder.supports_d3d11() {
                                    bail!("CPU prepared slot dispatched to GPU D3D11 encoder");
                                }
                                let e0 = Instant::now();
                                stats.frames_to_encoder.fetch_add(1, Ordering::Relaxed);
                                let encoded = core.encoder.encode(conv.frame(), pts)?;
                                (slot, prep_us, 0, 0, e0, encoded)
                            }
                            PreparedSlot::CpuRepeat { slot, pts, prep_us } => {
                                if core.encoder.supports_d3d11() {
                                    bail!("CPU repeat slot dispatched to GPU D3D11 encoder");
                                }
                                let e0 = Instant::now();
                                stats.frames_to_encoder.fetch_add(1, Ordering::Relaxed);
                                let encoded = if let Some(conv) = last_conv.as_ref() {
                                    core.encoder.encode(conv.frame(), pts)?
                                } else if let Some(nv12) = last_fallback_nv12.as_ref() {
                                    encode_fallback_nv12(core.encoder.as_mut(), nv12, pts)?
                                } else {
                                    bail!("CPU repeat slot before any CPU or fallback frame");
                                };
                                (slot, prep_us, 0, 0, e0, encoded)
                            }
                        };
                        #[cfg(not(windows))]
                        let (slot, prep_us, vp_submit_us, gpu_wait_us, e0, encoded) = match item {
                            PreparedSlot::FallbackNv12 { slot, pts, prep_us, nv12 } => {
                                let e0 = Instant::now();
                                stats.frames_to_encoder.fetch_add(1, Ordering::Relaxed);
                                let encoded = encode_fallback_nv12(core.encoder.as_mut(), &nv12, pts)?;
                                last_fallback_nv12 = Some(nv12);
                                (slot, prep_us, 0u64, 0u64, e0, encoded)
                            }
                            PreparedSlot::Cpu { slot, pts, prep_us, conv } => {
                                last_fallback_nv12 = None;
                                if let Some(old) = last_conv.replace(conv) {
                                    let _ = recycle_tx.send(old);
                                }
                                let conv = last_conv.as_ref().expect("just set");
                                let e0 = Instant::now();
                                stats.frames_to_encoder.fetch_add(1, Ordering::Relaxed);
                                let encoded = core.encoder.encode(conv.frame(), pts)?;
                                (slot, prep_us, 0u64, 0u64, e0, encoded)
                            }
                            PreparedSlot::CpuRepeat { slot, pts, prep_us } => {
                                let e0 = Instant::now();
                                stats.frames_to_encoder.fetch_add(1, Ordering::Relaxed);
                                let encoded = if let Some(conv) = last_conv.as_ref() {
                                    core.encoder.encode(conv.frame(), pts)?
                                } else if let Some(nv12) = last_fallback_nv12.as_ref() {
                                    encode_fallback_nv12(core.encoder.as_mut(), nv12, pts)?
                                } else {
                                    bail!("CPU repeat slot before any CPU or fallback frame");
                                };
                                (slot, prep_us, 0u64, 0u64, e0, encoded)
                            }
                        };
                        let stage = core.encoder.last_timing().unwrap_or_default();
                        let e1 = Instant::now();
                        if encoded.is_empty() {
                            stats.frames_skipped.fetch_add(1, Ordering::Relaxed);
                        }
                        for enc in encoded {
                            core.push(&enc, &stats)?;
                        }
                        let e2 = Instant::now();
                        stats.slots_done.fetch_add(1, Ordering::Relaxed);
                        push_audio(&mut core.mux, &stats)?;
                        let e3 = Instant::now();

                        let encode_us = (e1 - e0).as_micros() as u64;
                        let mux_us = (e2 - e1).as_micros() as u64;
                        let encode_stage_us = gpu_wait_us + (e3 - e0).as_micros() as u64;
                        let bottleneck_us = prep_us.max(encode_stage_us);
                        Stats::rolling(&stats.encode_us, encode_us + gpu_wait_us);
                        Stats::rolling(&stats.mux_us, mux_us);
                        Stats::rolling(&stats.slot_us, bottleneck_us);
                        sec_acc.record(prep_us, vp_submit_us, gpu_wait_us, stage, encode_us, mux_us, bottleneck_us);

                        let cur_grid_fps = shared_grid_fps.load(Ordering::Relaxed).max(1) as u64;
                        let cur_slot_interval = Duration::from_nanos(NS / cur_grid_fps);
                        if Duration::from_micros(bottleneck_us) > cur_slot_interval * 2 {
                            log::warn!(
                                "slow slot {slot}: prepare {:.1} ms (vp_submit {:.1}), gpu_wait {:.1} ms, encode {:.1} ms (mft_in {:.1}, mft_wait {:.1} [need_in {:.1}, proc_in {:.1}, out {:.1}]), mux {:.1} ms",
                                prep_us as f64 / 1e3,
                                vp_submit_us as f64 / 1e3,
                                gpu_wait_us as f64 / 1e3,
                                (e1 - e0).as_secs_f64() * 1e3,
                                stage.mft_in_us as f64 / 1e3,
                                stage.mft_wait_us as f64 / 1e3,
                                stage.mft_need_in_us as f64 / 1e3,
                                stage.mft_process_in_us as f64 / 1e3,
                                stage.mft_out_us as f64 / 1e3,
                                (e2 - e1).as_secs_f64() * 1e3,
                            );
                        }
                        maybe_log_sec(&mut sec_acc, &mut adaptive_grid);
                        if last_report.elapsed() >= Duration::from_secs(5) {
                            last_report = Instant::now();
                            log::info!(
                                "slot {slot}: captured {} encoded {} dropped {} slots skipped {} repeated {} superseded {} | encode {:.1} ms, slot {:.1} ms",
                                stats.frames_captured.load(Ordering::Relaxed),
                                stats.frames_encoded.load(Ordering::Relaxed),
                                stats.frames_dropped.load(Ordering::Relaxed),
                                stats.slots_skipped.load(Ordering::Relaxed),
                                stats.frames_repeated.load(Ordering::Relaxed),
                                stats.frames_superseded.load(Ordering::Relaxed),
                                stats.encode_us.load(Ordering::Relaxed) as f64 / 1e3,
                                stats.slot_us.load(Ordering::Relaxed) as f64 / 1e3
                            );
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        push_audio(&mut core.mux, &stats)?;
                        maybe_log_sec(&mut sec_acc, &mut adaptive_grid);
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            Ok(())
        })();

        // Wake `openclip-prep` cleanly before joining if `encode_res` exited early.
        abort_prep.store(true, Ordering::Relaxed);
        drop(prep_rx);
        drop(recycle_tx);
        drop(last_conv);
        #[cfg(windows)]
        drop(last_gpu);
        let prep_res = prep_handle.join().map_err(|_| anyhow!("prep thread panicked"))?;
        encode_res?;
        prep_res?;
        Ok(())
    })?;

    for enc in core.encoder.flush()? {
        core.push(&enc, &stats)?;
    }
    if is_auto {
        let cur_grid_fps = shared_grid_fps.load(Ordering::Relaxed).max(1) as u64;
        let final_slot_interval = Duration::from_nanos(NS / cur_grid_fps);
        let mut end_pts = stats.stop_pts.get().unwrap_or_else(rec_now);
        if let Some(last_pts) = core.last_video_pts {
            end_pts = end_pts.max(last_pts + final_slot_interval);
        }
        core.mux.finish_video(end_pts)?;
    }
    let EncodeCore { mut mux, .. } = core;
    // Audio thread flushes and closes its channel after the stop flag; drain everything.
    if mux.has_audio() {
        for (track, f) in audio_rx.iter() {
            mux.push_audio_track(track, &f)?;
            stats.audio_frames.fetch_add(1, Ordering::Relaxed);
        }
    }
    stats.bytes_written.store(mux.bytes_written(), Ordering::Relaxed);
    mux.finalize()?;
    Ok(())
}

/// Consecutive overloaded seconds before stepping down the adaptive frame grid.
const GRID_STEP_DOWN_SECS: u32 = 3;
/// Consecutive seconds with sufficient headroom before stepping the grid back up.
const GRID_STEP_UP_SECS: u32 = 5;
const GRID_INITIAL_COOLDOWN_SECS: u64 = 10;
const GRID_MAX_COOLDOWN_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
enum GridAction {
    None,
    Step { from: u32, to: u32, reason: String },
}

fn grid_ladder(base_fps: u32) -> Vec<u32> {
    if base_fps <= 60 {
        return vec![base_fps.max(1)];
    }
    let mut ladder = vec![base_fps];
    for cand in [120u32, 90, 72, 60] {
        let prev = *ladder.last().unwrap();
        if cand < prev && (cand == 60 || cand * 100 <= prev * 94) {
            ladder.push(cand);
        }
    }
    if *ladder.last().unwrap() > 60 {
        ladder.push(60);
    }
    ladder
}

#[derive(Default)]
struct SecTimingAccum {
    frames: u64,
    prep_us: u64,
    vp_submit_us: u64,
    gpu_wait_us: u64,
    mft_in_us: u64,
    mft_wait_us: u64,
    encode_us: u64,
    mux_us: u64,
    bottleneck_us: u64,
}

impl SecTimingAccum {
    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        prep_us: u64,
        vp_submit_us: u64,
        gpu_wait_us: u64,
        stage: EncodeStageTiming,
        encode_us: u64,
        mux_us: u64,
        bottleneck_us: u64,
    ) {
        self.frames += 1;
        self.prep_us += prep_us;
        self.vp_submit_us += vp_submit_us;
        self.gpu_wait_us += gpu_wait_us;
        self.mft_in_us += stage.mft_in_us;
        self.mft_wait_us += stage.mft_wait_us;
        self.encode_us += encode_us;
        self.mux_us += mux_us;
        self.bottleneck_us += bottleneck_us;
    }

    fn avg_bottleneck_us(&self) -> u64 {
        self.bottleneck_us.checked_div(self.frames).unwrap_or(0)
    }

    fn take_summary(&mut self) -> String {
        let n = self.frames;
        if n == 0 {
            return "no frames".into();
        }
        let avg = |v: u64| v as f64 / n as f64 / 1e3;
        let out = format!(
            "prep {:.2}ms (vp_submit {:.2}ms) gpu_wait {:.2}ms encode {:.2}ms (mft_in {:.2}ms mft_wait {:.2}ms) mux {:.2}ms",
            avg(self.prep_us),
            avg(self.vp_submit_us),
            avg(self.gpu_wait_us),
            avg(self.encode_us),
            avg(self.mft_in_us),
            avg(self.mft_wait_us),
            avg(self.mux_us),
        );
        *self = Self::default();
        out
    }
}

struct AdaptiveFrameGrid {
    is_auto: bool,
    ladder: Vec<u32>,
    idx: usize,
    over_secs: u32,
    under_secs: u32,
    last_step_down: Option<Instant>,
    last_step_up: Option<Instant>,
    cooldown_secs: u64,
}

impl AdaptiveFrameGrid {
    fn new(base_fps: u32, is_auto: bool) -> Self {
        Self {
            is_auto,
            ladder: grid_ladder(base_fps),
            idx: 0,
            over_secs: 0,
            under_secs: 0,
            last_step_down: None,
            last_step_up: None,
            cooldown_secs: GRID_INITIAL_COOLDOWN_SECS,
        }
    }

    fn current_fps(&self) -> u32 {
        self.ladder[self.idx]
    }

    fn reset_streaks(&mut self) {
        self.over_secs = 0;
        self.under_secs = 0;
    }

    fn observe(
        &mut self,
        dt: Duration,
        slots_done: u64,
        slots_skipped: u64,
        avg_work_us: u64,
        now: Instant,
    ) -> GridAction {
        if self.ladder.len() <= 1 {
            return GridAction::None;
        }
        let cur_fps = self.current_fps();
        let budget_us = 1_000_000u64 / cur_fps as u64;
        let expected = ((cur_fps as f64) * dt.as_secs_f64()).round().max(1.0) as u64;
        let active = slots_done + slots_skipped;

        let overloaded = if self.is_auto {
            // Auto FPS pauses emission on static screens; only judge capacity when
            // frames are actively flowing and either slots skipped or encode lags.
            active >= (cur_fps as u64 / 3).max(12)
                && ((slots_done * 100 < active * 90) || (slots_skipped >= 3 && avg_work_us > budget_us))
        } else {
            (slots_done * 100 < expected * 90) || (slots_skipped >= 3 && avg_work_us > budget_us)
        };

        if overloaded && self.idx + 1 < self.ladder.len() {
            self.under_secs = 0;
            self.over_secs += 1;
            if self.over_secs >= GRID_STEP_DOWN_SECS {
                self.over_secs = 0;
                let quick_relapse = self
                    .last_step_up
                    .is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(25));
                if quick_relapse {
                    self.cooldown_secs = (self.cooldown_secs * 2).min(GRID_MAX_COOLDOWN_SECS);
                } else {
                    self.cooldown_secs = GRID_INITIAL_COOLDOWN_SECS;
                }
                self.last_step_down = Some(now);
                let from = cur_fps;
                self.idx += 1;
                let to = self.current_fps();
                let denom = if self.is_auto { active } else { expected };
                let reason = format!(
                    "{slots_done}/{denom} slots kept, skipped {slots_skipped}, work avg {:.1} ms > budget {:.1} ms for {}s",
                    avg_work_us as f64 / 1e3,
                    budget_us as f64 / 1e3,
                    GRID_STEP_DOWN_SECS,
                );
                return GridAction::Step { from, to, reason };
            }
            return GridAction::None;
        }

        self.over_secs = 0;

        if self.idx > 0 {
            let up_fps = self.ladder[self.idx - 1];
            let up_budget_us = 1_000_000u64 / up_fps as u64;
            let active_enough = if self.is_auto {
                slots_done >= (cur_fps as u64 / 2).max(15)
            } else {
                slots_done * 100 >= expected * 95
            };
            if active_enough && slots_skipped == 0 && avg_work_us * 100 <= up_budget_us * 82 {
                self.under_secs += 1;
            } else {
                self.under_secs = 0;
            }
            if self.under_secs >= GRID_STEP_UP_SECS
                && self
                    .last_step_down
                    .is_some_and(|t| now.saturating_duration_since(t) >= Duration::from_secs(self.cooldown_secs))
            {
                self.under_secs = 0;
                self.last_step_up = Some(now);
                let from = cur_fps;
                self.idx -= 1;
                let to = self.current_fps();
                let reason = format!(
                    "headroom recovered after {}s cooldown (work avg {:.1} ms <= 82% of {} fps budget {:.1} ms for {}s)",
                    self.cooldown_secs,
                    avg_work_us as f64 / 1e3,
                    up_fps,
                    up_budget_us as f64 / 1e3,
                    GRID_STEP_UP_SECS,
                );
                return GridAction::Step { from, to, reason };
            }
        }

        GridAction::None
    }
}

struct PrepState {
    /// Half size uses the exact 2×2 box filter; everything else the scaler.
    half: bool,
    scaler: Option<Scaler>,
    /// Size of the first captured frame, which everything below was built from.
    src: (u32, u32),
    /// Size the frames are converted at (before the encoder's even rounding).
    scaled: (u32, u32),
    /// Fits a differently-sized source into `scaled`; built the first time the
    /// source changes size and reused after that.
    letterbox: Option<Scaler>,
    /// The frame being encoded (already scaled). Kept for repeats.
    frame: Option<RawFrame>,
    /// True when `frame` changed since the converter last saw it.
    converted_is_new: bool,
    previewer: Previewer,
    /// Encoded-frame / source ratio per axis, for mouse-effect placement.
    fx_scale: (f32, f32),
    patches: Vec<Patch>,
    /// Composes the watermark badge once for this recording; `None` when the
    /// badge is off or the bundled artwork could not be loaded.
    watermark: Option<WatermarkRenderer>,
    last_fx: Option<((i32, i32), Vec<FrameClick>)>,
}

struct EncodeCore {
    encoder: Box<dyn VideoEncoder>,
    mux: Muxer,
    params_sent: bool,
    last_video_pts: Option<Duration>,
}

struct CpuPipelineSpec {
    src: (u32, u32),
    scaled: (u32, u32),
    dims: (u32, u32),
    half: bool,
    layout: InputLayout,
}

type Nv12Planes = (Vec<u8>, Vec<u8>, usize, usize);

struct InitEncodePipeline {
    cpu_spec: CpuPipelineSpec,
    core: EncodeCore,
    cpu: Option<(PrepState, Converter, Converter)>,
    fallback_nv12: Option<Nv12Planes>,
    resolved_fps: u32,
}

fn build_cpu_prep(config: &RecordConfig, spec: &CpuPipelineSpec) -> Result<(PrepState, Converter, Converter)> {
    let conv1 = Converter::new(spec.scaled.0, spec.scaled.1, spec.layout)?;
    let conv2 = Converter::new(spec.scaled.0, spec.scaled.1, spec.layout)?;
    let scaler = (!spec.half && spec.scaled != spec.src).then(|| Scaler::new(spec.src, spec.scaled));
    let fx_scale = (spec.dims.0 as f32 / spec.src.0.max(1) as f32, spec.dims.1 as f32 / spec.src.1.max(1) as f32);
    let prep = PrepState {
        half: spec.half,
        scaler,
        src: spec.src,
        scaled: spec.scaled,
        letterbox: None,
        frame: None,
        converted_is_new: false,
        previewer: Previewer::new(spec.scaled.0, spec.scaled.1, PREVIEW_MAX_SIDE),
        fx_scale,
        patches: Vec::new(),
        watermark: config.watermark.any_overlay().then(WatermarkRenderer::new).flatten(),
        last_fx: None,
    };
    Ok((prep, conv1, conv2))
}

fn init_encode_pipeline(
    config: &RecordConfig,
    nominal_fps: u32,
    first: &CapturedFrame,
    audio: Vec<AudioTrackConfig>,
    stats: &Stats,
    gpu_active: &AtomicBool,
) -> Result<InitEncodePipeline> {
    let fmt = &config.format;
    let src = first.src_size();
    let (half, scaled) = match fmt.size {
        SizeMode::Full => (false, src),
        SizeMode::Half if src.0 >= 64 && src.1 >= 64 => (true, ((src.0 / 2).max(1), (src.1 / 2).max(1))),
        SizeMode::Half => (false, src),
        other => (false, other.resolve(src.0, src.1)),
    };
    let dims = even_dims(scaled.0, scaled.1);
    if dims.0 == 0 || dims.1 == 0 {
        return Err(anyhow!("frame too small to encode: {}x{}", src.0, src.1));
    }
    let make_req = |fps: u32| EncoderRequest {
        codec: fmt.video_codec.clone(),
        width: dims.0,
        height: dims.1,
        fps,
        rate_control: fmt.rate_control,
        target_bitrate_bps: fmt.target_bitrate_kbps_at(dims.0, dims.1, fps).saturating_mul(1000),
        keyframe_interval_frames: fmt.keyframe_interval_frames_at(fps),
        profiles: fmt.profiles,
    };
    let mut resolved_fps = nominal_fps;
    #[cfg(windows)]
    let d3d_device = match first {
        CapturedFrame::Gpu(g) => Some(&g.device),
        CapturedFrame::Cpu(_) => None,
    };
    let create = |req: &EncoderRequest| {
        #[cfg(windows)]
        {
            create_video_encoder_with_d3d(req, d3d_device)
        }
        #[cfg(not(windows))]
        {
            create_video_encoder(req)
        }
    };
    let (encoder, note) = match create(&make_req(resolved_fps)) {
        Ok(res) => res,
        Err(e) if config.is_auto_fps() && resolved_fps != 60 => {
            log::warn!("encoder init failed at {resolved_fps} fps ({e:#}); falling back to 60 fps");
            resolved_fps = 60;
            create(&make_req(resolved_fps))?
        }
        Err(e) => return Err(e),
    };
    if let Some(n) = note {
        stats.add_note(n);
    }
    *stats.encoder.lock().unwrap() = Some(encoder.describe());
    let layout = encoder.input_layout();
    let spec = CpuPipelineSpec { src, scaled, dims, half, layout };
    let video = VideoTrackConfig {
        width: dims.0,
        height: dims.1,
        fps: resolved_fps as f64,
        codec: if encoder.is_hevc() { VideoCodecConfig::Hevc } else { VideoCodecConfig::H264 },
    };
    let mux = Muxer::create_multi(fmt.container, &config.output, video, audio)?;
    log::info!(
        "encoding {}×{} → {}×{} @ {} fps into {} with {}",
        src.0,
        src.1,
        dims.0,
        dims.1,
        resolved_fps,
        fmt.container.label(),
        encoder.describe()
    );
    let (cpu, fallback_nv12) = match first {
        #[cfg(windows)]
        CapturedFrame::Gpu(g) if encoder.supports_d3d11() => {
            let _ = g;
            log::info!("video capture pipeline: GPU zero-readback (ID3D11VideoProcessor BGRA→NV12 + DXGI MFT)");
            (None, None)
        }
        #[cfg(windows)]
        CapturedFrame::Gpu(g) => {
            gpu_active.store(false, Ordering::SeqCst);
            log::info!(
                "video capture pipeline: CPU readback (fallback: encoder {} does not accept D3D11 textures)",
                encoder.describe()
            );
            let nv12 = crate::video::d3d11_vp::readback_nv12_texture(&g.device, &g.slot.texture, dims.0, dims.1)?;
            (Some(build_cpu_prep(config, &spec)?), Some(nv12))
        }
        CapturedFrame::Cpu(_) => {
            gpu_active.store(false, Ordering::SeqCst);
            log::info!("video capture pipeline: CPU readback + Converter");
            (Some(build_cpu_prep(config, &spec)?), None)
        }
    };
    let core = EncodeCore { encoder, mux, params_sent: false, last_video_pts: None };
    Ok(InitEncodePipeline { cpu_spec: spec, core, cpu, fallback_nv12, resolved_fps })
}

impl PrepState {
    /// Makes `captured` the current frame (scaling into the reusable buffer),
    /// returning consumed buffers to the pool.
    fn accept(&mut self, captured: RawFrame, pool: &FramePool, stats: &Stats) {
        if (captured.width, captured.height) != self.src {
            // The source was resized under us — a window dragged to another
            // resolution, or a game toggling fullscreen. The encoder, converter
            // and container header were all built from the first frame and
            // cannot be renegotiated, so fit the new picture into the size they
            // expect instead of failing the recording.
            let scaled = self.scaled;
            let fit = self.letterbox.get_or_insert_with(|| {
                stats.add_note(crate::t!(NOTE_SOURCE_RESIZED, scaled.0, scaled.1));
                Scaler::new((captured.width, captured.height), scaled)
            });
            let mut dst = self.frame.take().unwrap_or_else(|| RawFrame::empty(PixelFormat::Bgra));
            fit.letterbox_into(&captured, &mut dst);
            pool.recycle(captured.data);
            self.frame = Some(dst);
            self.converted_is_new = true;
            return;
        }
        if self.half || self.scaler.is_some() {
            let mut dst = self.frame.take().unwrap_or_else(|| RawFrame::empty(PixelFormat::Bgra));
            if self.half {
                captured.downscale_half_into(&mut dst);
            } else if let Some(s) = &mut self.scaler {
                s.scale_into(&captured, &mut dst);
            }
            pool.recycle(captured.data);
            self.frame = Some(dst);
        } else if let Some(old) = self.frame.replace(captured) {
            pool.recycle(old.data);
        }
        self.converted_is_new = true;
    }

    fn has_active_clicks(&self) -> bool {
        self.last_fx.as_ref().is_some_and(|(_, clicks)| !clicks.is_empty())
    }

    /// Whether the effects would look different from the last painted frame.
    fn fx_changed(&self, cursor: (i32, i32), clicks: &[FrameClick]) -> bool {
        match &self.last_fx {
            // Ripples animate: any live click means repaint.
            Some((c, prev)) => *c != cursor || !clicks.is_empty() || !prev.is_empty(),
            None => true,
        }
    }
}

impl EncodeCore {
    fn push(&mut self, enc: &EncodedFrame, stats: &Stats) -> Result<()> {
        if !self.params_sent
            && let Some(p) = self.encoder.codec_params()
        {
            self.mux.set_codec_params(p);
            self.params_sent = true;
        }
        let pts = monotonic_pts(self.last_video_pts, enc.pts);
        if self.mux.push_video(&enc.data, pts, enc.keyframe)? {
            self.last_video_pts = Some(pts);
            stats.frames_encoded.fetch_add(1, Ordering::Relaxed);
            stats.bytes_written.store(self.mux.bytes_written(), Ordering::Relaxed);
        } else {
            stats.frames_skipped.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

struct AudioArgs {
    config: RecordConfig,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    paused_total: Arc<AtomicU64>,
    tx: SyncSender<(usize, AudioFrame)>,
    stats: Arc<Stats>,
}

struct FanOut {
    raw: SharedQueue,
    targets: [SharedQueue; 2],
}

struct SourceSlot {
    track_idx: usize,
    source_id: usize,
    target_queue: Option<SharedQueue>,
}

fn push_capped(q: &mut Queue, chunk: Chunk, tag: &str) {
    if q.chunks.len() >= 512
        && let Some(dropped) = q.chunks.pop_front()
    {
        q.overflow += 1;
        log::warn!(
            "audio: [{tag}] fan-out queue overflow! Dropped {} samples, total overflows: {}",
            dropped.data.len(),
            q.overflow
        );
    }
    q.chunks.push_back(chunk);
}

struct TrackPipeline {
    mixer: Mixer,
    encoder: Box<dyn AudioEncoder>,
    channels: usize,
}

fn spawn_audio_thread(args: AudioArgs) -> Result<(JoinHandle<Result<()>>, Vec<AudioTrackConfig>)> {
    let AudioArgs { config, epoch, stop, paused, paused_total, tx, stats } = args;
    // Open devices and create the encoders on the worker thread (Media
    // Foundation objects are thread-affine), reporting back before mixing.
    let (ready_tx, ready_rx) = mpsc::channel::<Result<Vec<AudioTrackConfig>>>();
    let handle = std::thread::Builder::new().name("openclip-audio".into()).spawn(move || {
        let clamp_gain = |g: f32| if g.is_finite() { g.clamp(0.0, 2.0) } else { 1.0 };
        let sys_gain = clamp_gain(config.system_audio_gain);
        let mic_gain = clamp_gain(config.mic_gain);
        let mut sys_src: Option<AudioSource> = None;
        let mut mic_src: Option<AudioSource> = None;
        let mut notes = Vec::new();
        if config.system_audio {
            match open_system_loopback_device(config.system_audio_device.as_deref()) {
                Ok(s) => sys_src = Some(s),
                Err(e) => notes.push(crate::t!(NOTE_SYSTEM_AUDIO_UNAVAILABLE, format!("{e:#}"))),
            }
        }
        if let Some(mic) = &config.microphone {
            match open_microphone(mic.as_deref()) {
                Ok(s) => mic_src = Some(s),
                Err(e) => notes.push(crate::t!(NOTE_MIC_UNAVAILABLE, format!("{e:#}"))),
            }
        }
        if !notes.is_empty() {
            *stats.audio_note.lock().unwrap() = Some(notes.join("; "));
        }
        let roles = resolve_audio_tracks(sys_src.is_some(), mic_src.is_some(), config.audio_track_mode);
        if roles.is_empty() {
            let _ = ready_tx.send(Err(anyhow!("no audio source could be opened")));
            return Ok(());
        }
        let fmt = &config.format;
        let mut encoders = Vec::with_capacity(roles.len());
        let mut track_cfgs = Vec::with_capacity(roles.len());
        for (idx, _role) in roles.iter().enumerate() {
            let (enc, note) = match create_audio_encoder(
                fmt.audio_codec,
                fmt.audio_sample_rate,
                fmt.audio_channels,
                fmt.audio_bitrate_kbps,
            ) {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return Ok(());
                }
            };
            if idx == 0 {
                if let Some(n) = note {
                    stats.add_note(n);
                }
                log::info!("audio ({} track(s)): {}", roles.len(), enc.describe());
            }
            track_cfgs.push(AudioTrackConfig {
                sample_rate: enc.sample_rate(),
                channels: enc.channels().clamp(1, 2),
                bitrate_bps: enc.bitrate_bps(),
                samples_per_frame: enc.samples_per_frame(),
                codec: enc.codec_config(),
            });
            encoders.push(enc);
        }
        let _ = ready_tx.send(Ok(track_cfgs));

        // Wait for the first video frame to define the timeline origin.
        let origin = loop {
            if let Some(o) = stats.stream_start.get() {
                break o;
            }
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        };

        // When both Mixed and per-source tracks consume the same AudioSource,
        // duplicate incoming chunks into paired queues so Mixer::ingest /
        // shift_origin never steal chunks from the sibling track.
        let fan_out_sys = roles.contains(&TrackRole::Mixed) && roles.contains(&TrackRole::System);
        let fan_out_mic = roles.contains(&TrackRole::Mixed) && roles.contains(&TrackRole::Mic);
        let mut fanouts: Vec<FanOut> = Vec::new();
        let mut sys_fanout_idx: Option<usize> = None;
        let mut mic_fanout_idx: Option<usize> = None;
        let (sys_mix_q, sys_sep_q) = if let Some(s) = &sys_src {
            if fan_out_sys {
                let q1: SharedQueue = Arc::new(Mutex::new(Queue::default()));
                let q2: SharedQueue = Arc::new(Mutex::new(Queue::default()));
                sys_fanout_idx = Some(fanouts.len());
                fanouts.push(FanOut { raw: s.queue.clone(), targets: [q1.clone(), q2.clone()] });
                (Some(q1), Some(q2))
            } else {
                (Some(s.queue.clone()), Some(s.queue.clone()))
            }
        } else {
            (None, None)
        };
        let (mic_mix_q, mic_sep_q) = if let Some(s) = &mic_src {
            if fan_out_mic {
                let q1: SharedQueue = Arc::new(Mutex::new(Queue::default()));
                let q2: SharedQueue = Arc::new(Mutex::new(Queue::default()));
                mic_fanout_idx = Some(fanouts.len());
                fanouts.push(FanOut { raw: s.queue.clone(), targets: [q1.clone(), q2.clone()] });
                (Some(q1), Some(q2))
            } else {
                (Some(s.queue.clone()), Some(s.queue.clone()))
            }
        } else {
            (None, None)
        };

        let mut sys_slots: Vec<SourceSlot> = Vec::new();
        let mut mic_slots: Vec<SourceSlot> = Vec::new();
        let mut tracks: Vec<TrackPipeline> = Vec::with_capacity(roles.len());

        for (track_idx, (role, encoder)) in roles.into_iter().zip(encoders).enumerate() {
            let rate = encoder.sample_rate();
            let channels = encoder.channels().clamp(1, 2) as usize;
            let mut mixer = Mixer::new(rate, epoch, origin);
            match role {
                TrackRole::Mixed => {
                    if let (Some(s), Some(q)) = (&sys_src, &sys_mix_q) {
                        let target_queue = if fan_out_sys { Some(q.clone()) } else { None };
                        let source_id = mixer.add_source("sys", q.clone(), s.sample_rate, s.channels, sys_gain);
                        sys_slots.push(SourceSlot { track_idx, source_id, target_queue });
                    }
                    if let (Some(s), Some(q)) = (&mic_src, &mic_mix_q) {
                        let target_queue = if fan_out_mic { Some(q.clone()) } else { None };
                        let source_id = mixer.add_source("mic", q.clone(), s.sample_rate, s.channels, mic_gain);
                        mic_slots.push(SourceSlot { track_idx, source_id, target_queue });
                    }
                }
                TrackRole::System => {
                    if let (Some(s), Some(q)) = (&sys_src, &sys_sep_q) {
                        let target_queue = if fan_out_sys { Some(q.clone()) } else { None };
                        let source_id = mixer.add_source("sys", q.clone(), s.sample_rate, s.channels, sys_gain);
                        sys_slots.push(SourceSlot { track_idx, source_id, target_queue });
                    }
                }
                TrackRole::Mic => {
                    if let (Some(s), Some(q)) = (&mic_src, &mic_sep_q) {
                        let target_queue = if fan_out_mic { Some(q.clone()) } else { None };
                        let source_id = mixer.add_source("mic", q.clone(), s.sample_rate, s.channels, mic_gain);
                        mic_slots.push(SourceSlot { track_idx, source_id, target_queue });
                    }
                }
            }
            tracks.push(TrackPipeline { mixer, encoder, channels });
        }

        let device_listener = DeviceNotificationListener::new();
        let mut sys_reconnect_due: Option<Instant> = None;
        let mut mic_reconnect_due: Option<Instant> = None;
        let mut pcm = Vec::new();
        let mut mono = Vec::new();
        let send = |track_idx: usize, frames: Vec<AudioFrame>| -> Result<()> {
            for f in frames {
                if tx.send((track_idx, f)).is_err() {
                    return Err(anyhow!("encoder went away"));
                }
            }
            Ok(())
        };
        let mut seen_paused_us = 0u64;
        let mut last_telemetry_log = Instant::now();
        let mut last_sys_frames = 0u64;
        let mut last_mic_frames = 0u64;

        loop {
            let stopping = stop.load(Ordering::Relaxed);
            let total_paused_us = paused_total.load(Ordering::SeqCst);
            if total_paused_us != seen_paused_us {
                let delta = Duration::from_micros(total_paused_us - seen_paused_us);
                for fan in &fanouts {
                    fan.raw.lock().unwrap().chunks.clear();
                }
                for tp in &mut tracks {
                    tp.mixer.shift_origin(delta);
                }
                seen_paused_us = total_paused_us;
            }
            if paused.load(Ordering::Relaxed) && !stopping {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }

            let now = Instant::now();

            // Core Audio device notifications (plug/unplug or default device changed):
            if device_listener.changed.swap(false, Ordering::SeqCst) {
                // If system audio captures the default device, check whether the default device changed:
                if config.system_audio
                    && config.system_audio_device.is_none()
                    && let Some(src) = &sys_src
                {
                    let current_default = default_output_device_name();
                    if current_default.as_ref().is_some_and(|name| name != &src.name) {
                        log::info!(
                            "audio: [sys] default output changed from '{}' to '{}', switching...",
                            src.name,
                            current_default.as_deref().unwrap_or("")
                        );
                        sys_src = None;
                        sys_reconnect_due = Some(now + Duration::from_millis(300));
                    }
                }
                // If microphone captures the default device, check whether the default mic changed:
                if config.microphone.as_ref().is_some_and(|m| m.is_none())
                    && let Some(src) = &mic_src
                {
                    let current_default = default_input_device_name();
                    if current_default.as_ref().is_some_and(|name| name != &src.name) {
                        log::info!(
                            "audio: [mic] default microphone changed from '{}' to '{}', switching...",
                            src.name,
                            current_default.as_deref().unwrap_or("")
                        );
                        mic_src = None;
                        mic_reconnect_due = Some(now + Duration::from_millis(300));
                    }
                }
                // Hasten reconnect attempts if devices reappeared:
                if sys_src.is_none() && !sys_slots.is_empty() {
                    let debounce = now + Duration::from_millis(300);
                    if sys_reconnect_due.is_none_or(|due| due > debounce) {
                        sys_reconnect_due = Some(debounce);
                    }
                }
                if mic_src.is_none() && !mic_slots.is_empty() {
                    let debounce = now + Duration::from_millis(300);
                    if mic_reconnect_due.is_none_or(|due| due > debounce) {
                        mic_reconnect_due = Some(debounce);
                    }
                }
            }

            // Detect broken CPAL streams:
            if let Some(src) = &sys_src
                && src.needs_reconnect()
            {
                log::warn!("audio: [sys] system loopback stream failed ('{}'), scheduling reconnect...", src.name);
                sys_src = None;
                sys_reconnect_due = Some(now + Duration::from_millis(300));
            }
            if let Some(src) = &mic_src
                && src.needs_reconnect()
            {
                log::warn!("audio: [mic] microphone stream failed ('{}'), scheduling reconnect...", src.name);
                mic_src = None;
                mic_reconnect_due = Some(now + Duration::from_millis(300));
            }

            // Reconnection attempts:
            if let Some(due) = sys_reconnect_due
                && now >= due
            {
                match open_system_loopback_device(config.system_audio_device.as_deref()) {
                    Ok(new_s) => {
                        log::info!(
                            "audio: [sys] system loopback recovered: '{}' {} Hz, {} ch",
                            new_s.name,
                            new_s.sample_rate,
                            new_s.channels
                        );
                        if let Some(idx) = sys_fanout_idx {
                            fanouts[idx].raw = new_s.queue.clone();
                        }
                        for slot in &sys_slots {
                            let q = slot.target_queue.as_ref().cloned().unwrap_or_else(|| new_s.queue.clone());
                            tracks[slot.track_idx].mixer.update_source(
                                slot.source_id,
                                q,
                                new_s.sample_rate,
                                new_s.channels,
                                Some(sys_gain),
                            );
                        }
                        sys_src = Some(new_s);
                        sys_reconnect_due = None;
                    }
                    Err(e) => {
                        log::debug!("audio: [sys] system loopback reconnect attempt failed: {e:#}, retrying in 1s");
                        sys_reconnect_due = Some(now + Duration::from_secs(1));
                    }
                }
            }
            if let Some(due) = mic_reconnect_due
                && now >= due
            {
                match open_microphone(config.microphone.as_ref().and_then(|m| m.as_deref())) {
                    Ok(new_s) => {
                        log::info!(
                            "audio: [mic] microphone recovered: '{}' {} Hz, {} ch",
                            new_s.name,
                            new_s.sample_rate,
                            new_s.channels
                        );
                        if let Some(idx) = mic_fanout_idx {
                            fanouts[idx].raw = new_s.queue.clone();
                        }
                        for slot in &mic_slots {
                            let q = slot.target_queue.as_ref().cloned().unwrap_or_else(|| new_s.queue.clone());
                            tracks[slot.track_idx].mixer.update_source(
                                slot.source_id,
                                q,
                                new_s.sample_rate,
                                new_s.channels,
                                Some(mic_gain),
                            );
                        }
                        mic_src = Some(new_s);
                        mic_reconnect_due = None;
                    }
                    Err(e) => {
                        log::debug!("audio: [mic] microphone reconnect attempt failed: {e:#}, retrying in 1s");
                        mic_reconnect_due = Some(now + Duration::from_secs(1));
                    }
                }
            }

            for fan in &fanouts {
                let drained: Vec<_> = fan.raw.lock().unwrap().chunks.drain(..).collect();
                if drained.is_empty() {
                    continue;
                }
                {
                    let mut q0 = fan.targets[0].lock().unwrap();
                    for c in &drained {
                        push_capped(&mut q0, c.clone(), "fan-out #1");
                    }
                }
                let mut q1 = fan.targets[1].lock().unwrap();
                for c in drained {
                    push_capped(&mut q1, c, "fan-out #2");
                }
            }
            let until = if stopping { now } else { now - AUDIO_LAG };
            for (track_idx, tp) in tracks.iter_mut().enumerate() {
                pcm.clear();
                tp.mixer.mix_until(until, &mut pcm);
                if !pcm.is_empty() {
                    let input: &[f32] = if tp.channels == 1 {
                        // The mixer always produces stereo; fold it down.
                        mono.clear();
                        mono.extend(pcm.as_chunks::<2>().0.iter().map(|[l, r]| (l + r) * 0.5));
                        &mono
                    } else {
                        &pcm
                    };
                    send(track_idx, tp.encoder.encode(input)?)?;
                }
            }

            // Periodic 1-second telemetry log:
            if now.duration_since(last_telemetry_log) >= Duration::from_secs(1) {
                let (sys_delta, sys_interval_ms) = if let Some(s) = &sys_src {
                    let total = s.stats.total_frames.load(Ordering::Relaxed);
                    let delta = total.saturating_sub(last_sys_frames);
                    last_sys_frames = total;
                    let max_us = s.stats.max_interval_us.swap(0, Ordering::Relaxed);
                    (delta, max_us as f64 / 1000.0)
                } else {
                    (0, 0.0)
                };

                let (mic_delta, mic_interval_ms) = if let Some(s) = &mic_src {
                    let total = s.stats.total_frames.load(Ordering::Relaxed);
                    let delta = total.saturating_sub(last_mic_frames);
                    last_mic_frames = total;
                    let max_us = s.stats.max_interval_us.swap(0, Ordering::Relaxed);
                    (delta, max_us as f64 / 1000.0)
                } else {
                    (0, 0.0)
                };

                let buf_summary = if let Some(first_track) = tracks.first() {
                    first_track
                        .mixer
                        .buffer_status()
                        .iter()
                        .map(|(name, frames, ms)| format!("{name}: {frames}f/{ms:.1}ms"))
                        .collect::<Vec<_>>()
                        .join(", ")
                } else {
                    String::new()
                };

                log::info!(
                    "audio: 1s stats | sys: {sys_delta} frames (max interval {sys_interval_ms:.1} ms) | mic: {mic_delta} frames (max interval {mic_interval_ms:.1} ms) | mixer buffers: [{buf_summary}]"
                );
                last_telemetry_log = now;
            }

            if stopping {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        for (track_idx, tp) in tracks.iter_mut().enumerate() {
            send(track_idx, tp.encoder.flush()?)?;
        }
        let silence_total: u64 = tracks.iter().map(|tp| tp.mixer.silence_inserted).sum();
        log::info!("audio: done, {silence_total} silence samples inserted");
        drop(sys_src);
        drop(mic_src);
        Ok(())
    })?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(cfgs)) => Ok((handle, cfgs)),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => Err(anyhow!("audio thread did not start")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn clock_slots_are_regular() {
        let c = FrameClock::new(30, ms(1000));
        assert_eq!(c.slot_pts(0), ms(1000));
        assert_eq!(c.slot_pts(30), ms(2000));
        assert_eq!(c.deadline(0).as_micros(), 1_016_666);
        assert_eq!(c.due(ms(1010)), 0);
        assert_eq!(c.due(ms(1017)), 1);
        assert_eq!(c.due(ms(1050)), 2);
    }

    #[test]
    fn clock_catches_up_after_a_stall() {
        let mut c = FrameClock::new(30, ms(0));
        // Two slots late: encode them one by one, no skip.
        assert_eq!(c.catch_up(ms(60)), 0);
        assert_eq!(c.due(ms(60)), 2);
        // A 5-slot stall: keep only the latest due slot.
        let skipped = c.catch_up(ms(170));
        assert_eq!(skipped, 4);
        assert_eq!(c.next, 4);
        assert_eq!(c.due(ms(170)), 1);
    }

    #[test]
    fn elapsed_is_measured_from_the_first_frame() {
        // Nothing captured yet: the file is empty, so the timer reads zero.
        assert_eq!(recorded_span(ms(400), None), Duration::ZERO);
        // 0.65 s of start-up is not in the file and must not be in the denominator.
        assert_eq!(recorded_span(ms(5650), Some(ms(650))), ms(5000));
        // Paused time is already removed from `now` *and* from the stored
        // origin, so it is subtracted exactly once.
        assert_eq!(recorded_span(ms(7000), Some(ms(650))), ms(6350));
        // A clock that has not reached the origin never goes negative.
        assert_eq!(recorded_span(ms(100), Some(ms(650))), Duration::ZERO);
    }

    #[test]
    fn origin_is_unknown_until_the_first_frame() {
        let o = Origin::default();
        assert_eq!(o.get(), None);
        o.set(ms(650));
        assert_eq!(o.get(), Some(ms(650)));
    }

    #[test]
    fn pre_roll_counters_are_cleared_but_encoded_frames_are_not() {
        let s = Stats::default();
        for c in [&s.frames_captured, &s.frames_to_encoder, &s.frames_dropped, &s.frames_superseded, &s.frames_encoded]
        {
            c.store(7, Ordering::Relaxed);
        }
        s.reset_pre_roll();
        assert_eq!(s.frames_captured.load(Ordering::Relaxed), 0);
        assert_eq!(s.frames_to_encoder.load(Ordering::Relaxed), 0);
        assert_eq!(s.frames_dropped.load(Ordering::Relaxed), 0);
        assert_eq!(s.frames_superseded.load(Ordering::Relaxed), 0);
        // Encoded frames belong to the file, not the warm-up.
        assert_eq!(s.frames_encoded.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn frame_mailbox_overwrites_stale_and_recycles_buffer() {
        let pool = FramePool::new(4);
        let stats = Arc::new(Stats::default());
        let (tx, rx) = frame_mailbox(pool, stats.clone());
        let mk = |pts_ms: u64| {
            CapturedFrame::Cpu(RawFrame {
                data: vec![pts_ms as u8; 16],
                width: 2,
                height: 2,
                stride: 8,
                format: PixelFormat::Bgra,
                pts: ms(pts_ms),
                mouse: None,
            })
        };
        assert!(tx.send(mk(10)));
        assert!(tx.send(mk(20)));
        assert!(tx.send(mk(30)));
        assert!(tx.send(mk(40)));
        // Up to FRAME_QUEUE_CAP (4) frames queue without lock eviction.
        assert_eq!(stats.frames_superseded.load(Ordering::Relaxed), 0);
        // Pushing a 5th evicts the oldest (10) without dropping newer incoming frames.
        assert!(tx.send(mk(50)));
        assert_eq!(stats.frames_superseded.load(Ordering::Relaxed), 1);
        assert_eq!(stats.frames_dropped.load(Ordering::Relaxed), 0);
        let got: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let pts: Vec<_> = got.iter().map(CapturedFrame::pts).collect();
        assert_eq!(pts, vec![ms(20), ms(30), ms(40), ms(50)]);
        let CapturedFrame::Cpu(last) = got.last().unwrap() else { unreachable!() };
        assert_eq!(last.data[0], 50);
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        drop(rx);
        assert!(!tx.send(mk(60)));
    }

    #[test]
    fn clock_snaps_to_pts_without_going_backwards() {
        let mut c = FrameClock::new(120, ms(0));
        c.snap_to_pts(ms(1000));
        assert_eq!(c.next, 120);
        // Never snaps backwards.
        c.snap_to_pts(ms(500));
        assert_eq!(c.next, 120);
    }

    #[test]
    fn monotonic_pts_never_goes_backwards_or_repeats() {
        assert_eq!(monotonic_pts(None, ms(10)), ms(10));
        assert_eq!(monotonic_pts(Some(ms(10)), ms(20)), ms(20));
        assert_eq!(monotonic_pts(Some(ms(20)), ms(20)), Duration::from_micros(20_100));
        assert_eq!(monotonic_pts(Some(ms(20)), ms(15)), Duration::from_micros(20_100));
    }

    #[test]
    fn clock_resyncs_after_pause() {
        let mut c = FrameClock::new(30, ms(0));
        c.next = 10;
        c.resync(ms(1000));
        assert_eq!(c.next, 30);
        assert_eq!(c.due(ms(1000)), 0);
        // Never moves backwards.
        c.resync(ms(100));
        assert_eq!(c.next, 30);
    }

    #[test]
    fn resolve_audio_tracks_layout() {
        assert_eq!(resolve_audio_tracks(false, false, AudioTrackMode::Mixed), Vec::new());
        assert_eq!(resolve_audio_tracks(true, false, AudioTrackMode::Separate), vec![TrackRole::System]);
        assert_eq!(resolve_audio_tracks(false, true, AudioTrackMode::MixedAndSeparate), vec![TrackRole::Mic]);
        assert_eq!(resolve_audio_tracks(true, true, AudioTrackMode::Mixed), vec![TrackRole::Mixed]);
        assert_eq!(
            resolve_audio_tracks(true, true, AudioTrackMode::Separate),
            vec![TrackRole::System, TrackRole::Mic]
        );
        assert_eq!(
            resolve_audio_tracks(true, true, AudioTrackMode::MixedAndSeparate),
            vec![TrackRole::Mixed, TrackRole::System, TrackRole::Mic]
        );
    }

    #[test]
    fn frame_clock_set_fps_keeps_monotonic_smooth_cadence() {
        let mut c = FrameClock::new(120, ms(0));
        let mut pts = Vec::new();
        // Emit 120 slots at 120 fps.
        for _ in 0..120 {
            let p = c.slot_pts(c.next);
            c.mark_emitted(p);
            pts.push(p);
            c.next += 1;
        }
        assert_eq!(pts.last().unwrap().as_micros(), 991_666);

        // Step down 120 -> 90 -> 72 -> 60 -> 90, emitting a few frames at each rung.
        for target_fps in [90u32, 72, 60, 90] {
            assert!(c.set_fps(target_fps));
            assert!(!c.set_fps(target_fps), "no-op if unchanged");
            let expected_step_us = 1_000_000i64 / target_fps as i64;
            for _ in 0..5 {
                let p = c.slot_pts(c.next);
                let prev = *pts.last().unwrap();
                assert!(p > prev, "pts must strictly increase: {p:?} vs {prev:?}");
                let delta_us = (p - prev).as_micros() as i64;
                assert!(
                    (delta_us - expected_step_us).abs() <= 1,
                    "transition/step delta {delta_us} us vs expected {expected_step_us} us at {target_fps} fps",
                );
                c.mark_emitted(p);
                pts.push(p);
                c.next += 1;
            }
        }
    }

    #[test]
    fn adaptive_frame_grid_steps_down_recovers_and_ignores_auto_idle() {
        assert_eq!(grid_ladder(60), vec![60]);
        assert_eq!(grid_ladder(30), vec![30]);
        assert_eq!(grid_ladder(120), vec![120, 90, 72, 60]);
        assert_eq!(grid_ladder(144), vec![144, 120, 90, 72, 60]);

        let sec = Duration::from_secs(1);
        let mut t = Instant::now();

        // Fixed 60 fps never adapts.
        let mut g60 = AdaptiveFrameGrid::new(60, false);
        for _ in 0..10 {
            t += sec;
            assert_eq!(g60.observe(sec, 30, 30, 20_000, t), GridAction::None);
        }

        // Fixed 120 fps under game load (~75 fps capability, avg work ~12 ms):
        // steps 120 -> 90 after 3 overloaded seconds, then 90 -> 72 after 3 more.
        let mut g120 = AdaptiveFrameGrid::new(120, false);
        for i in 0..3 {
            t += sec;
            let act = g120.observe(sec, 78, 42, 12_500, t);
            if i < 2 {
                assert_eq!(act, GridAction::None);
            } else {
                assert!(matches!(act, GridAction::Step { from: 120, to: 90, .. }));
            }
        }
        assert_eq!(g120.current_fps(), 90);

        // At 90 fps, if encoder still averages 13 ms (>11.1 ms) and keeps 75/90 slots -> steps to 72.
        for i in 0..3 {
            t += sec;
            let act = g120.observe(sec, 75, 15, 13_000, t);
            if i < 2 {
                assert_eq!(act, GridAction::None);
            } else {
                assert!(matches!(act, GridAction::Step { from: 90, to: 72, .. }));
            }
        }
        assert_eq!(g120.current_fps(), 72);

        // At 72 fps with encoder at 10.2 ms (>82% of 90 fps budget = 9.11 ms): stays at 72 even after cooldown.
        for _ in 0..12 {
            t += sec;
            assert_eq!(g120.observe(sec, 72, 0, 10_200, t), GridAction::None);
        }
        assert_eq!(g120.current_fps(), 72);

        // Game closes: avg work drops to 4 ms (<= 82% of 90 fps budget) -> steps back up after 5s.
        let mut stepped_up = false;
        for _ in 0..5 {
            t += sec;
            if let GridAction::Step { from: 72, to: 90, .. } = g120.observe(sec, 72, 0, 4_000, t) {
                stepped_up = true;
            }
        }
        assert!(stepped_up);
        assert_eq!(g120.current_fps(), 90);

        // Auto FPS on static desktop (2 frames/sec, 0 skipped, avg work 4 ms): never steps down.
        let mut g_auto = AdaptiveFrameGrid::new(120, true);
        for _ in 0..20 {
            t += sec;
            assert_eq!(g_auto.observe(sec, 2, 0, 4_000, t), GridAction::None);
            assert_eq!(g_auto.observe(sec, 30, 0, 4_000, t), GridAction::None);
        }
        assert_eq!(g_auto.current_fps(), 120);

        // Auto FPS under heavy game load (75 done + 45 skipped): steps down 120 -> 90.
        for i in 0..3 {
            t += sec;
            let act = g_auto.observe(sec, 75, 45, 13_000, t);
            if i < 2 {
                assert_eq!(act, GridAction::None);
            } else {
                assert!(matches!(act, GridAction::Step { from: 120, to: 90, .. }));
            }
        }
    }
}
