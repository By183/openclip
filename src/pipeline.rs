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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use crate::audio::capture::{
    open_microphone, open_system_loopback_device, AudioSource, Chunk, Queue, SharedQueue,
};
use crate::audio::mixer::Mixer;
use crate::audio::{create_audio_encoder, AudioEncoder, AudioFrame};
use crate::capture::monitors::{source_origin, source_refresh_hz};
use crate::capture::{self, CaptureConfig, CaptureHandle, FramePool, LiveRect, Rect, Source};
use crate::mux::{AudioTrackConfig, Muxer, VideoCodecConfig, VideoTrackConfig};
use crate::settings::{AudioTrackMode, FormatSettings, SizeMode};
use crate::video::convert::even_dims;
use crate::video::mouse_fx::{FrameClick, MouseFx, MouseSampler, Patch};
use crate::video::preview::{PreviewImage, Previewer};
use crate::video::watermark::{Watermark, WatermarkRenderer};
use crate::video::{create_video_encoder, Converter, EncodedFrame, EncoderRequest, PixelFormat, RawFrame, Scaler, VideoEncoder};

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
    queue: VecDeque<RawFrame>,
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
    fn send(&self, frame: RawFrame) -> bool {
        let mut guard = self.inbox.state.lock().unwrap();
        if guard.closed {
            drop(guard);
            self.inbox.pool.recycle(frame.data);
            return false;
        }
        let evicted = (guard.queue.len() >= FRAME_QUEUE_CAP).then(|| guard.queue.pop_front()).flatten();
        guard.queue.push_back(frame);
        drop(guard);
        if let Some(old) = evicted {
            self.inbox.stats.frames_superseded.fetch_add(1, Ordering::Relaxed);
            self.inbox.pool.recycle(old.data);
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
    fn try_recv(&self) -> std::result::Result<RawFrame, TryRecvError> {
        let mut guard = self.inbox.state.lock().unwrap();
        if let Some(f) = guard.queue.pop_front() {
            Ok(f)
        } else if guard.closed {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    fn recv_timeout(&self, timeout: Duration) -> std::result::Result<RawFrame, RecvTimeoutError> {
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
            self.inbox.pool.recycle(f.data);
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

    /// Stops recording and finalizes the file.
    pub fn stop(mut self) -> Result<PathBuf> {
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
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(c) = self.capture.take() {
            let _ = c.stop();
        }
        if let Some(t) = self.audio_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.encode_thread.take() {
            let _ = t.join();
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
) -> Result<CaptureHandle> {
    let note_stats = stats.clone();
    let sink: capture::FrameSink = Box::new(move |mut frame| {
        stats.frames_captured.fetch_add(1, Ordering::Relaxed);
        // Sample the pointer right when the frame arrives so effects line up
        // with the captured image even if encoding lags behind.
        if let Some(s) = &sampler {
            frame.mouse = Some(s.lock().unwrap().snapshot());
        }
        tx.send(frame)
    });
    let capture_fps = if config.is_auto_fps() {
        match config.source {
            Source::Game { .. } => nominal_fps,
            _ => 0,
        }
    } else {
        config.fps()
    };
    let mut handle = capture::start(
        CaptureConfig {
            source: config.source.clone(),
            fps: capture_fps,
            show_cursor: config.mouse_fx.native_cursor(),
            pool: Some(pool),
            live_region: region,
        },
        epoch,
        sink,
    )
    .context("starting screen capture")?;
    // Whatever the backend could not honour (an OS without the capture-border
    // or cursor setting) shows up next to the encoder notes.
    if let Some(n) = handle.take_note() {
        note_stats.add_note(n);
    }
    Ok(handle)
}

/// Fixed-cadence timeline: slot `k` is presented at `pts0 + k / fps`
/// (exact rational arithmetic, so long recordings never drift).
#[derive(Debug, Clone)]
pub struct FrameClock {
    fps: u64,
    pts0_ns: u64,
    /// Next slot to encode.
    pub next: u64,
}

const NS: u64 = 1_000_000_000;

impl FrameClock {
    /// Encoding may lag this many slots before slots are skipped.
    pub const MAX_BEHIND: u64 = 2;

    pub fn new(fps: u32, pts0: Duration) -> Self {
        Self { fps: fps.max(1) as u64, pts0_ns: pts0.as_nanos() as u64, next: 0 }
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
    let take_newest = |newest: &mut Option<RawFrame>, f: RawFrame, pool: &FramePool, stats: &Stats| {
        if let Some(old) = newest.replace(f) {
            pool.recycle(old.data);
            stats.frames_superseded.fetch_add(1, Ordering::Relaxed);
        }
    };

    // Wait for the very first captured frame, which defines the timeline origin
    // and the resolution everything downstream is built around.
    let mut first_frame: Option<RawFrame> = None;
    while first_frame.is_none() {
        let stopping = stop.load(Ordering::Relaxed);
        if paused.load(Ordering::Relaxed) && !stopping {
            match video_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(f) => pool.recycle(f.data),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            continue;
        }
        match video_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(mut f) => {
                // Drain if a newer frame arrived right as we woke up.
                while let Ok(newer) = video_rx.try_recv() {
                    pool.recycle(std::mem::take(&mut f.data));
                    f = newer;
                }
                f.pts = f.pts.saturating_sub(paused_dur());
                log::info!(
                    "timeline starts at {:.0} ms (capture latency {:.0} ms)",
                    f.pts.as_secs_f64() * 1e3,
                    rec_now().saturating_sub(f.pts).as_secs_f64() * 1e3
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

    let (mut prep, mut core, conv1, conv2, resolved_fps) =
        init_encode_pipeline(&config, nominal_fps, &first, audio_cfgs.take().unwrap_or_default(), &stats)?;
    let slot_interval = Duration::from_nanos(NS / resolved_fps.max(1) as u64);
    stats.width.store(prep.dims.0 as u64, Ordering::Relaxed);
    stats.height.store(prep.dims.1 as u64, Ordering::Relaxed);
    let is_auto = config.is_auto_fps();
    stats.target_fps.store(resolved_fps as u64, Ordering::Relaxed);
    // Counters so far describe the warm-up, not the file.
    stats.reset_pre_roll();
    stats.stream_start.set(first.pts);

    enum PreparedSlot {
        Converted {
            slot: u64,
            pts: Duration,
            prep_us: u64,
            conv: Converter,
        },
        RepeatPrev {
            slot: u64,
            pts: Duration,
            prep_us: u64,
        },
    }

    let (prep_tx, prep_rx) = mpsc::sync_channel::<PreparedSlot>(1);
    let (recycle_tx, recycle_rx) = mpsc::sync_channel::<Converter>(2);
    recycle_tx.send(conv1).expect("recycle channel has capacity 2");
    recycle_tx.send(conv2).expect("recycle channel has capacity 2");
    let abort_prep = Arc::new(AtomicBool::new(false));
    let mut clock = FrameClock::new(resolved_fps, first.pts);

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

        let prep_handle = std::thread::Builder::new()
            .name("openclip-prep".into())
            .spawn_scoped(scope, move || -> Result<()> {
                let mut newest: Option<RawFrame> = Some(first);
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
                            Ok(f) => prep_pool.recycle(f.data),
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
                            prep_pool.recycle(f.data);
                        }
                        if let Some(f) = newest.take() {
                            prep_pool.recycle(f.data);
                        }
                        clock.resync(rec_now());
                    }

                    // ----- collect frames until ready -----
                    let mut force_fx_frame = false;
                    if is_auto && auto_idle {
                        while let Ok(mut f) = video_rx.try_recv() {
                            f.pts = f.pts.saturating_sub(paused_dur());
                            take_newest(&mut newest, f, prep_pool, &prep_stats);
                        }
                        while newest.is_none()
                            && !prep_stop.load(Ordering::Relaxed)
                            && !disconnected
                            && !prep_abort.load(Ordering::Relaxed)
                        {
                            let fx_animating = prep.frame.is_some()
                                && prep_sampler.is_some()
                                && (!prep_config.mouse_fx.native_cursor() || prep.has_active_clicks());
                            let wait_dur = if fx_animating {
                                Duration::from_millis(16)
                            } else {
                                Duration::from_millis(50)
                            };
                            match video_rx.recv_timeout(wait_dur) {
                                Ok(mut f) => {
                                    f.pts = f.pts.saturating_sub(paused_dur());
                                    take_newest(&mut newest, f, prep_pool, &prep_stats);
                                }
                                Err(RecvTimeoutError::Timeout) => {
                                    if prep_paused.load(Ordering::Relaxed) || prep_stop.load(Ordering::Relaxed) {
                                        break;
                                    }
                                    // Keep non-native cursor or active click ripples animating even
                                    // if WGC is idle, without duplicating static frames otherwise.
                                    if fx_animating && let Some(s) = prep_sampler {
                                        let snap = s.lock().unwrap().snapshot();
                                        let (cursor, clicks) = snap.mapped(origin, prep.fx_scale);
                                        if prep.fx_changed(cursor, &clicks) {
                                            if let Some(frame) = prep.frame.as_mut() {
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
                        if let Some(f) = newest.as_ref() {
                            let wake_now = rec_now();
                            // If Readback flushed an older pre-idle staged frame, anchor at wall time
                            // so returning from static idle does not trigger fake `slots_skipped`.
                            let snap_pts = if wake_now.saturating_sub(f.pts) > slot_interval * 2 {
                                wake_now
                            } else {
                                f.pts
                            };
                            clock.snap_to_pts(snap_pts);
                            auto_idle = false;
                        } else if force_fx_frame {
                            clock.snap_to_pts(rec_now());
                            auto_idle = false;
                        }
                    }
                    if !is_auto || (!auto_idle && !force_fx_frame) {
                        let deadline = clock.deadline(clock.next);
                        loop {
                            match video_rx.try_recv() {
                                Ok(mut f) => {
                                    f.pts = f.pts.saturating_sub(paused_dur());
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
                                    f.pts = f.pts.saturating_sub(paused_dur());
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
                            f.pts = f.pts.saturating_sub(paused_dur());
                            take_newest(&mut newest, f, prep_pool, &prep_stats);
                        }
                        if is_auto && auto_idle && let Some(f) = newest.as_ref() {
                            let wake_now = rec_now();
                            let snap_pts = if wake_now.saturating_sub(f.pts) > slot_interval * 2 {
                                wake_now
                            } else {
                                f.pts
                            };
                            clock.snap_to_pts(snap_pts);
                            auto_idle = false;
                        }
                    }

                    // ----- prepare one slot (scale + overlays + YUV conversion) -----
                    let now = rec_now();
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

                    let p0 = Instant::now();
                    let slot = clock.next;
                    let pts = clock.slot_pts(slot);
                    if let Some(f) = newest.take() {
                        prep.accept(f, prep_pool, &prep_stats);
                    } else {
                        prep_stats.frames_repeated.fetch_add(1, Ordering::Relaxed);
                        // The screen is static but the pointer may move: sample now.
                        if let (Some(s), Some(fx)) = (prep_sampler, prep.frame.as_mut()) {
                            fx.mouse = Some(s.lock().unwrap().snapshot());
                        }
                    }
                    let snap = prep.frame.as_mut().expect("frame present after accept").mouse.take();

                    // Overlays: paint in place, convert, restore — no frame copy.
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
                        let (cursor, clicks) = snap.mapped(origin, prep.fx_scale);
                        let changed = prep.fx_changed(cursor, &clicks);
                        fx = Some((cursor, clicks, changed));
                    }
                    let mut painted = false;
                    if prep.converted_is_new || fx.as_ref().is_some_and(|(_, _, changed)| *changed) {
                        let frame = prep.frame.as_mut().unwrap();
                        if let Some(wm) = &mut prep.watermark {
                            wm.paint(&prep_config.watermark, frame, &mut prep.patches);
                        }
                        if let Some((cursor, clicks, _)) = &fx {
                            let scale = prep.fx_scale.0.min(prep.fx_scale.1);
                            prep_config.mouse_fx.paint(frame, *cursor, clicks, scale, &mut prep.patches);
                        }
                        painted = !prep.patches.is_empty();
                    }
                    if let Some((cursor, clicks, _)) = fx {
                        prep.last_fx = Some((cursor, clicks));
                    }

                    let need_convert = prep.converted_is_new || painted;
                    let mut converted = None;
                    if need_convert {
                        let Ok(mut conv) = recycle_rx.recv() else {
                            break;
                        };
                        if let Err(e) = conv.convert(prep.frame.as_ref().unwrap()) {
                            prep_err = Some(e);
                            break;
                        }
                        prep.converted_is_new = false;
                        converted = Some(conv);
                    }
                    let convert_done = Instant::now();

                    // Build GUI preview while overlays are still painted (skipped if behind).
                    if prep_preview_visible.load(Ordering::Relaxed)
                        && p0.duration_since(last_preview) >= PREVIEW_INTERVAL
                        && (is_auto || clock.due(rec_now()) == 0)
                    {
                        last_preview = p0;
                        let img = prep.previewer.make(prep.frame.as_ref().unwrap());
                        *prep_preview.image.lock().unwrap() = Some(img);
                        if let Some(cb) = prep_on_preview {
                            cb();
                        }
                    }

                    // Restore overlay patches immediately now that both YUV conversion
                    // and preview took their snapshot, keeping the reusable frame clean.
                    let r0 = Instant::now();
                    if painted {
                        MouseFx::restore(prep.frame.as_mut().unwrap(), &mut prep.patches);
                    }
                    let prep_us = (convert_done - p0 + r0.elapsed()).as_micros() as u64;

                    let msg = match converted {
                        Some(conv) => PreparedSlot::Converted { slot, pts, prep_us, conv },
                        None => PreparedSlot::RepeatPrev { slot, pts, prep_us },
                    };
                    if prep_tx.send(msg).is_err() {
                        break;
                    }
                    clock.next += 1;
                    if disconnected && newest.is_none() {
                        break;
                    }
                }

                if let Some(f) = newest.take() {
                    prep_pool.recycle(f.data);
                }
                if let Some(f) = prep.frame.take() {
                    prep_pool.recycle(f.data);
                }
                drop(prep_tx);
                match prep_err {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            })?;

        let mut last_conv: Option<Converter> = None;
        let mut last_report = Instant::now();
        let mut last_sec_report = Instant::now();
        let mut prev_cap = stats.frames_captured.load(Ordering::Relaxed);
        let mut prev_enc_in = stats.frames_to_encoder.load(Ordering::Relaxed);
        let mut prev_drop = stats.frames_dropped.load(Ordering::Relaxed);
        let mut prev_sup = stats.frames_superseded.load(Ordering::Relaxed);
        let mut maybe_log_sec = || {
            let dt = last_sec_report.elapsed();
            if dt >= Duration::from_secs(1) {
                let cur_cap = stats.frames_captured.load(Ordering::Relaxed);
                let cur_enc_in = stats.frames_to_encoder.load(Ordering::Relaxed);
                let cur_drop = stats.frames_dropped.load(Ordering::Relaxed);
                let cur_sup = stats.frames_superseded.load(Ordering::Relaxed);
                log::info!(
                    "1s rates ({:.2} s): wgc {} encoder_in {} dropped {} superseded {}",
                    dt.as_secs_f64(),
                    cur_cap.saturating_sub(prev_cap),
                    cur_enc_in.saturating_sub(prev_enc_in),
                    cur_drop.saturating_sub(prev_drop),
                    cur_sup.saturating_sub(prev_sup),
                );
                prev_cap = cur_cap;
                prev_enc_in = cur_enc_in;
                prev_drop = cur_drop;
                prev_sup = cur_sup;
                last_sec_report = Instant::now();
            }
        };

        let encode_res = (|| -> Result<()> {
            loop {
                match prep_rx.recv_timeout(Duration::from_millis(25)) {
                    Ok(item) => {
                        let (slot, pts, prep_us, new_conv) = match item {
                            PreparedSlot::Converted { slot, pts, prep_us, conv } => (slot, pts, prep_us, Some(conv)),
                            PreparedSlot::RepeatPrev { slot, pts, prep_us } => (slot, pts, prep_us, None),
                        };
                        let e0 = Instant::now();
                        if let Some(conv) = new_conv
                            && let Some(old) = last_conv.replace(conv)
                        {
                            // Return the previous converter before starting GPU encode
                            // so `openclip-prep` can convert the next frame concurrently.
                            let _ = recycle_tx.send(old);
                        }
                        let conv = last_conv.as_ref().expect("first prepared slot is converted");
                        stats.frames_to_encoder.fetch_add(1, Ordering::Relaxed);
                        let encoded = core.encoder.encode(conv.frame(), pts)?;
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
                        let encode_stage_us = (e3 - e0).as_micros() as u64;
                        let bottleneck_us = prep_us.max(encode_stage_us);
                        Stats::rolling(&stats.encode_us, encode_us);
                        Stats::rolling(&stats.mux_us, mux_us);
                        Stats::rolling(&stats.slot_us, bottleneck_us);

                        if Duration::from_micros(bottleneck_us) > slot_interval * 2 {
                            log::warn!(
                                "slow slot {slot}: prepare {:.1} ms, encode {:.1} ms, mux {:.1} ms",
                                prep_us as f64 / 1e3,
                                (e1 - e0).as_secs_f64() * 1e3,
                                (e2 - e1).as_secs_f64() * 1e3
                            );
                        }
                        maybe_log_sec();
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
                        maybe_log_sec();
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
        let prep_res = prep_handle.join().map_err(|_| anyhow!("prep thread panicked"))?;
        encode_res?;
        prep_res?;
        Ok(())
    })?;

    for enc in core.encoder.flush()? {
        core.push(&enc, &stats)?;
    }
    if is_auto {
        let mut end_pts = stats.stop_pts.get().unwrap_or_else(rec_now);
        if let Some(last_pts) = core.last_video_pts {
            end_pts = end_pts.max(last_pts + slot_interval);
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
    dims: (u32, u32),
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

fn init_encode_pipeline(
    config: &RecordConfig,
    nominal_fps: u32,
    first: &RawFrame,
    audio: Vec<AudioTrackConfig>,
    stats: &Stats,
) -> Result<(PrepState, EncodeCore, Converter, Converter, u32)> {
    let fmt = &config.format;
    let src = (first.width, first.height);
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
    let (encoder, note) = match create_video_encoder(&make_req(resolved_fps)) {
        Ok(res) => res,
        Err(e) if config.is_auto_fps() && resolved_fps != 60 => {
            log::warn!("encoder init failed at {resolved_fps} fps ({e:#}); falling back to 60 fps");
            resolved_fps = 60;
            create_video_encoder(&make_req(resolved_fps))?
        }
        Err(e) => return Err(e),
    };
    if let Some(n) = note {
        stats.add_note(n);
    }
    *stats.encoder.lock().unwrap() = Some(encoder.describe());
    let layout = encoder.input_layout();
    let conv1 = Converter::new(scaled.0, scaled.1, layout)?;
    let conv2 = Converter::new(scaled.0, scaled.1, layout)?;
    let scaler = (!half && scaled != src).then(|| Scaler::new(src, scaled));
    let video = VideoTrackConfig {
        width: dims.0,
        height: dims.1,
        fps: resolved_fps as f64,
        codec: if encoder.is_hevc() { VideoCodecConfig::Hevc } else { VideoCodecConfig::H264 },
    };
    let mux = Muxer::create_multi(fmt.container, &config.output, video, audio)?;
    let fx_scale = (dims.0 as f32 / src.0.max(1) as f32, dims.1 as f32 / src.1.max(1) as f32);
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
    let prep = PrepState {
        half,
        scaler,
        src,
        scaled,
        letterbox: None,
        frame: None,
        converted_is_new: false,
        previewer: Previewer::new(scaled.0, scaled.1, PREVIEW_MAX_SIDE),
        dims,
        fx_scale,
        patches: Vec::new(),
        watermark: config.watermark.any_overlay().then(WatermarkRenderer::new).flatten(),
        last_fx: None,
    };
    let core = EncodeCore { encoder, mux, params_sent: false, last_video_pts: None };
    Ok((prep, core, conv1, conv2, resolved_fps))
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

fn push_capped(q: &mut Queue, chunk: Chunk) {
    if q.chunks.len() >= 512 {
        q.chunks.pop_front();
        q.overflow += 1;
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
        let mut sys_src: Option<(AudioSource, f32)> = None;
        let mut mic_src: Option<(AudioSource, f32)> = None;
        let mut notes = Vec::new();
        if config.system_audio {
            match open_system_loopback_device(config.system_audio_device.as_deref()) {
                Ok(s) => sys_src = Some((s, clamp_gain(config.system_audio_gain))),
                Err(e) => notes.push(crate::t!(NOTE_SYSTEM_AUDIO_UNAVAILABLE, format!("{e:#}"))),
            }
        }
        if let Some(mic) = &config.microphone {
            match open_microphone(mic.as_deref()) {
                Ok(s) => mic_src = Some((s, clamp_gain(config.mic_gain))),
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
        let (sys_mix_q, sys_sep_q) = if let Some((s, _)) = &sys_src {
            if fan_out_sys {
                let q1: SharedQueue = Arc::new(Mutex::new(Queue::default()));
                let q2: SharedQueue = Arc::new(Mutex::new(Queue::default()));
                fanouts.push(FanOut { raw: s.queue.clone(), targets: [q1.clone(), q2.clone()] });
                (Some(q1), Some(q2))
            } else {
                (Some(s.queue.clone()), Some(s.queue.clone()))
            }
        } else {
            (None, None)
        };
        let (mic_mix_q, mic_sep_q) = if let Some((s, _)) = &mic_src {
            if fan_out_mic {
                let q1: SharedQueue = Arc::new(Mutex::new(Queue::default()));
                let q2: SharedQueue = Arc::new(Mutex::new(Queue::default()));
                fanouts.push(FanOut { raw: s.queue.clone(), targets: [q1.clone(), q2.clone()] });
                (Some(q1), Some(q2))
            } else {
                (Some(s.queue.clone()), Some(s.queue.clone()))
            }
        } else {
            (None, None)
        };

        let mut tracks: Vec<TrackPipeline> = roles
            .into_iter()
            .zip(encoders)
            .map(|(role, encoder)| {
                let rate = encoder.sample_rate();
                let channels = encoder.channels().clamp(1, 2) as usize;
                let mut mixer = Mixer::new(rate, epoch, origin);
                match role {
                    TrackRole::Mixed => {
                        if let (Some((s, gain)), Some(q)) = (&sys_src, &sys_mix_q) {
                            mixer.add_source(q.clone(), s.sample_rate, s.channels, *gain);
                        }
                        if let (Some((s, gain)), Some(q)) = (&mic_src, &mic_mix_q) {
                            mixer.add_source(q.clone(), s.sample_rate, s.channels, *gain);
                        }
                    }
                    TrackRole::System => {
                        if let (Some((s, gain)), Some(q)) = (&sys_src, &sys_sep_q) {
                            mixer.add_source(q.clone(), s.sample_rate, s.channels, *gain);
                        }
                    }
                    TrackRole::Mic => {
                        if let (Some((s, gain)), Some(q)) = (&mic_src, &mic_sep_q) {
                            mixer.add_source(q.clone(), s.sample_rate, s.channels, *gain);
                        }
                    }
                }
                TrackPipeline { mixer, encoder, channels }
            })
            .collect();

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
            for fan in &fanouts {
                let drained: Vec<_> = fan.raw.lock().unwrap().chunks.drain(..).collect();
                if drained.is_empty() {
                    continue;
                }
                {
                    let mut q0 = fan.targets[0].lock().unwrap();
                    for c in &drained {
                        push_capped(&mut q0, c.clone());
                    }
                }
                let mut q1 = fan.targets[1].lock().unwrap();
                for c in drained {
                    push_capped(&mut q1, c);
                }
            }
            let now = Instant::now();
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
        let mk = |pts_ms: u64| RawFrame {
            data: vec![pts_ms as u8; 16],
            width: 2,
            height: 2,
            stride: 8,
            format: PixelFormat::Bgra,
            pts: ms(pts_ms),
            mouse: None,
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
        let pts: Vec<_> = got.iter().map(|f| f.pts).collect();
        assert_eq!(pts, vec![ms(20), ms(30), ms(40), ms(50)]);
        assert_eq!(got.last().unwrap().data[0], 50);
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
}
