//! Windows backend: Windows.Graphics.Capture through the `windows-capture` crate.
//!
//! Frames stay on the GPU until this module copies them: each callback issues
//! a `CopySubresourceRegion` of the wanted rectangle into one of two
//! persistent staging textures and then maps the *other* one (filled by the
//! previous callback), so the CPU never waits for the copy it just queued.
//! The mapped rows are copied straight into a pooled buffer — one CPU copy
//! per frame, no per-frame texture or heap allocations.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_SAMPLE_DESC,
};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use super::{CaptureConfig, CaptureHandle, FramePool, FrameSink, LiveRect, PhaseLimiter, Source};
use crate::settings::SizeMode;
use crate::video::convert::even_dims;
use crate::video::d3d11_vp::{is_supported_capture_format, GpuCaptureProcessor, GpuNv12Slot};
use crate::video::preview::PreviewImage;
use crate::video::{PixelFormat, RawFrame};

/// One GPU-converted `DXGI_FORMAT_NV12` surface kept in GPU memory for
/// zero-readback hardware Media Foundation encoding.
#[derive(Debug)]
pub struct GpuFrame {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub slot: Arc<GpuNv12Slot>,
    pub src_size: (u32, u32),
    pub pts: Duration,
    pub resized_from_initial: bool,
    pub prep_us: u64,
    pub vp_submit_us: u64,
}

/// Frame delivered by the Windows capture backend: either a zero-readback GPU
/// `NV12` surface or a standard CPU staging `RawFrame`.
#[derive(Debug)]
pub enum CapturedFrame {
    Cpu(RawFrame),
    Gpu(GpuFrame),
}

impl CapturedFrame {
    pub fn pts(&self) -> Duration {
        match self {
            CapturedFrame::Cpu(f) => f.pts,
            CapturedFrame::Gpu(f) => f.pts,
        }
    }

    pub fn set_pts(&mut self, pts: Duration) {
        match self {
            CapturedFrame::Cpu(f) => f.pts = pts,
            CapturedFrame::Gpu(f) => f.pts = pts,
        }
    }

    pub fn src_size(&self) -> (u32, u32) {
        match self {
            CapturedFrame::Cpu(f) => (f.width, f.height),
            CapturedFrame::Gpu(f) => f.src_size,
        }
    }
}

pub type CapturedSink = Box<dyn FnMut(CapturedFrame) -> bool + Send>;

pub struct GpuCaptureOptions {
    pub size_mode: SizeMode,
    pub gpu_active: Arc<AtomicBool>,
    pub preview_visible: Arc<AtomicBool>,
    pub on_preview: Box<dyn FnMut(PreviewImage) + Send>,
}

/// Everything the capture callback needs, passed through `Settings::flags`.
struct Flags {
    sink: CapturedSink,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    /// Re-read every frame so the UI can drag the region while recording.
    crop: Option<LiveRect>,
    fps: u32,
    /// Only when WGC cannot throttle itself (Windows 10).
    limit_ourselves: bool,
    pool: Arc<FramePool>,
    gpu: Option<GpuCaptureOptions>,
}

struct Handler {
    flags: Flags,
    limiter: Option<PhaseLimiter>,
    readback: Readback,
    gpu_vp: Option<GpuCaptureProcessor>,
    gpu_expected_crop: Option<(u32, u32)>,
    last_preview: Instant,
    frames: u64,
    priority_set: bool,
    /// `(base_ts_100ns, base_arrival)` anchoring WGC `SystemRelativeTime` to `epoch`.
    ts_anchor: Option<(i64, Duration)>,
}

impl Handler {
    /// Maps a WGC `SystemRelativeTime` value (in 100-ns ticks) onto our
    /// recording timeline (`epoch`-relative `Duration`).
    ///
    /// Anchoring the first accepted hardware timestamp to its arrival offset
    /// keeps audio/video aligned while computing every subsequent PTS delta in
    /// exact 100-ns hardware ticks instead of noisy callback arrival times.
    fn map_timestamp(anchor: &mut Option<(i64, Duration)>, ts_100ns: i64, arrival: Duration) -> Duration {
        let (base_ts, base_arrival) = *anchor.get_or_insert((ts_100ns, arrival));
        let delta_100ns = ts_100ns.saturating_sub(base_ts);
        if delta_100ns >= 0 {
            base_arrival.saturating_add(Duration::from_nanos((delta_100ns as u64).saturating_mul(100)))
        } else {
            base_arrival.saturating_sub(Duration::from_nanos(delta_100ns.unsigned_abs().saturating_mul(100)))
        }
    }

    fn try_gpu_convert(
        &mut self,
        frame: &mut Frame,
        crop: (u32, u32, u32, u32),
        pts: Duration,
        now: Instant,
    ) -> Result<GpuFrame> {
        let (x0, y0, x1, y1) = crop;
        let (fw, fh) = (frame.width(), frame.height());
        let crop_size = (x1 - x0, y1 - y0);
        let fmt = frame.desc().Format;
        if !is_supported_capture_format(fmt) {
            return Err(anyhow!("unsupported surface format {:?} for GPU VideoProcessor", fmt.0));
        }
        let gpu = self.flags.gpu.as_mut().expect("checked by caller");
        let t0 = Instant::now();
        if self.gpu_vp.is_none() {
            let scaled = match gpu.size_mode {
                SizeMode::Full => crop_size,
                SizeMode::Half if crop_size.0 >= 64 && crop_size.1 >= 64 => {
                    ((crop_size.0 / 2).max(1), (crop_size.1 / 2).max(1))
                }
                SizeMode::Half => crop_size,
                other => other.resolve(crop_size.0, crop_size.1),
            };
            let out_size = even_dims(scaled.0, scaled.1);
            if out_size.0 == 0 || out_size.1 == 0 {
                return Err(anyhow!("frame too small to encode: {}x{}", crop_size.0, crop_size.1));
            }
            let vp = GpuCaptureProcessor::new(
                self.readback.device.clone(),
                self.readback.context.clone(),
                (fw, fh),
                crop_size,
                out_size,
            )?;
            self.gpu_expected_crop = Some(crop_size);
            self.gpu_vp = Some(vp);
        }
        let vp = self.gpu_vp.as_mut().unwrap();
        let rect = RECT { left: x0 as i32, top: y0 as i32, right: x1 as i32, bottom: y1 as i32 };
        let tex = frame.as_raw_texture();
        let (slot, vp_submit_us) = vp.convert_timed(tex, (fw, fh), rect)?;
        if gpu.preview_visible.load(Ordering::Relaxed)
            && now.duration_since(self.last_preview) >= Duration::from_millis(200)
        {
            match vp.render_preview(tex, (fw, fh), rect, 640) {
                Ok(img) => {
                    self.last_preview = now;
                    (gpu.on_preview)(img);
                }
                Err(e) => {
                    log::debug!("GPU capture preview failed: {e:#}");
                }
            }
        }
        let prep_us = t0.elapsed().as_micros() as u64;
        let resized_from_initial = self.gpu_expected_crop.is_some_and(|e| e != crop_size);
        Ok(GpuFrame {
            device: vp.device().clone(),
            context: vp.context().clone(),
            slot,
            src_size: crop_size,
            pts,
            resized_from_initial,
            prep_us,
            vp_submit_us,
        })
    }
}

type HandlerError = Box<dyn std::error::Error + Send + Sync>;

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Flags;
    type Error = HandlerError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let limiter = ctx.flags.limit_ourselves.then(|| PhaseLimiter::new(ctx.flags.fps));
        Ok(Self {
            readback: Readback::new(ctx.device.clone(), ctx.device_context.clone()),
            flags: ctx.flags,
            limiter,
            gpu_vp: None,
            gpu_expected_crop: None,
            last_preview: Instant::now() - Duration::from_millis(200),
            frames: 0,
            priority_set: false,
            ts_anchor: None,
        })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, control: InternalCaptureControl) -> Result<(), Self::Error> {
        if self.flags.stop.load(Ordering::Relaxed) {
            control.stop();
            return Ok(());
        }
        if !self.priority_set {
            // The delivery thread must not be starved by the encoder's worker threads.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) }.ok();
            self.priority_set = true;
        }
        let now = Instant::now();
        if let Some(l) = &mut self.limiter
            && !l.accept(now)
        {
            return Ok(());
        }
        let arrival = now.duration_since(self.flags.epoch);
        let pts = match frame.timestamp() {
            Ok(ts) => Self::map_timestamp(&mut self.ts_anchor, ts.Duration, arrival),
            Err(_) => arrival,
        };
        let (fw, fh) = (frame.width(), frame.height());
        let (x0, y0, x1, y1) = match self.flags.crop.as_ref().map(LiveRect::get) {
            Some(r) => {
                let x0 = r.x.min(fw.saturating_sub(1));
                let y0 = r.y.min(fh.saturating_sub(1));
                ((x0), (y0), (r.x + r.width).min(fw).max(x0 + 1), (r.y + r.height).min(fh).max(y0 + 1))
            }
            None => (0, 0, fw, fh),
        };

        let try_gpu = self.flags.gpu.as_ref().is_some_and(|g| g.gpu_active.load(Ordering::Relaxed));
        if try_gpu {
            if self.frames == 0 {
                self.readback.reset();
            }
            match self.try_gpu_convert(frame, (x0, y0, x1, y1), pts, now) {
                Ok(gf) => {
                    self.frames += 1;
                    if !(self.flags.sink)(CapturedFrame::Gpu(gf)) {
                        control.stop();
                    }
                    return Ok(());
                }
                Err(e) if self.frames == 0 => {
                    log::warn!("capture GPU VideoProcessor failed during startup ({e:#}); using CPU readback");
                    if let Some(g) = &self.flags.gpu {
                        g.gpu_active.store(false, Ordering::SeqCst);
                    }
                    self.gpu_vp = None;
                }
                Err(e) => {
                    return Err(format!("GPU VideoProcessor failed mid-recording: {e:#}").into());
                }
            }
        } else if self.gpu_vp.is_some() {
            self.gpu_vp = None;
        }

        let bx = D3D11_BOX { left: x0, top: y0, front: 0, right: x1, bottom: y1, back: 1 };
        let delivered = self
            .readback
            .submit(frame.as_raw_texture(), frame.desc().Format, bx, pts, &self.flags.pool)
            .map_err(|e| -> HandlerError { format!("frame readback: {e:#}").into() })?;
        if let Some(raw) = delivered {
            self.frames += 1;
            if !(self.flags.sink)(CapturedFrame::Cpu(raw)) {
                control.stop();
            }
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        log::info!("capture item closed after {} frames", self.frames);
        Ok(())
    }
}

/// The [`PixelFormat`] a DXGI surface maps to, or `None` for one the pipeline
/// cannot take. HDR and 10-bit back buffers land here — a game using one is
/// refused with a note rather than recorded with mangled colour.
pub(crate) fn pixel_format(format: DXGI_FORMAT) -> Option<PixelFormat> {
    match format {
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => Some(PixelFormat::Bgra),
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => Some(PixelFormat::Rgba),
        _ => None,
    }
}

/// Double-buffered GPU → CPU readback.
///
/// Shared with the game-capture backend, which reads the hook's shared texture
/// exactly the same way Windows.Graphics.Capture's surface is read here.
pub(crate) struct Readback {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: [Option<ID3D11Texture2D>; 2],
    size: (u32, u32),
    format: DXGI_FORMAT,
    /// Byte order of `format`, resolved once when the staging textures are made.
    pixels: PixelFormat,
    /// Staging slot holding a copy that has not been read yet, with its timestamp.
    pending: Option<(usize, Duration)>,
    /// True once the initial frame (or first frame after resize) has been delivered.
    primed: bool,
}

impl Readback {
    pub(crate) fn new(device: ID3D11Device, context: ID3D11DeviceContext) -> Self {
        Self {
            device,
            context,
            staging: [None, None],
            size: (0, 0),
            format: DXGI_FORMAT(0),
            pixels: PixelFormat::Bgra,
            pending: None,
            primed: false,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.pending = None;
        self.primed = false;
    }

    fn ensure(&mut self, w: u32, h: u32, format: DXGI_FORMAT) -> Result<()> {
        if self.size == (w, h) && self.format == format && self.staging.iter().all(Option::is_some) {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        for slot in &mut self.staging {
            let mut tex = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.context("CreateTexture2D")?;
            *slot = tex;
        }
        self.size = (w, h);
        self.format = format;
        self.pixels = pixel_format(format)
            .ok_or_else(|| anyhow!("unsupported surface format {:?}; 8-bit BGRA or RGBA only", format.0))?;
        self.pending = None;
        self.primed = false;
        log::debug!("readback staging textures {w}×{h}");
        Ok(())
    }

    /// Queues the copy of `bx` from `src` and returns the frame queued by the
    /// previous call (if any), copied into a pooled buffer.
    ///
    /// On a static screen, WGC delivers one frame and then waits up to a couple
    /// of seconds before emitting another; waiting for the second callback to
    /// read out slot 0 would stall recording startup and anchor `pts0` late.
    /// The very first frame therefore reads synchronously so double-buffering
    /// primes cleanly without delaying startup.
    pub(crate) fn submit(
        &mut self,
        src: &ID3D11Texture2D,
        format: DXGI_FORMAT,
        bx: D3D11_BOX,
        pts: Duration,
        pool: &FramePool,
    ) -> Result<Option<RawFrame>> {
        let (w, h) = (bx.right - bx.left, bx.bottom - bx.top);
        self.ensure(w, h, format)?;
        let slot = self.pending.map(|(s, _)| s ^ 1).unwrap_or(0);
        let dst = self.staging[slot].as_ref().unwrap();
        unsafe {
            self.context.CopySubresourceRegion(dst, 0, 0, 0, 0, src, 0, Some(&bx));
            // The capture surface is recycled once this callback returns;
            // make sure the copy is submitted before that.
            self.context.Flush();
        }
        let (read_slot, read_pts) = if !self.primed && self.pending.is_none() {
            self.primed = true;
            (slot, pts)
        } else {
            let previous = self.pending.replace((slot, pts));
            let Some(prev) = previous else { return Ok(None) };
            prev
        };
        let tex = self.staging[read_slot].as_ref().unwrap();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { self.context.Map(tex, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.context("Map staging texture")?;
        let row = (w * 4) as usize;
        let pitch = mapped.RowPitch as usize;
        let total = row * h as usize;
        let mut data = pool.take();
        if data.capacity() < total {
            data.reserve(total);
        }
        unsafe {
            let base = mapped.pData as *const u8;
            if pitch == row {
                data.extend_from_slice(std::slice::from_raw_parts(base, total));
            } else {
                for y in 0..h as usize {
                    data.extend_from_slice(std::slice::from_raw_parts(base.add(y * pitch), row));
                }
            }
            self.context.Unmap(tex, 0);
        }
        Ok(Some(RawFrame { data, width: w, height: h, stride: w * 4, format: self.pixels, pts: read_pts, mouse: None }))
    }
}

pub fn start(config: CaptureConfig, epoch: Instant, mut sink: FrameSink) -> Result<CaptureHandle> {
    if let Source::Game { .. } = &config.source {
        // Frames come from the hook inside the game, not from this backend.
        return super::hook::start(config, epoch, sink);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let crop = config.crop();
    let wrapped: CapturedSink = Box::new(move |frame| match frame {
        CapturedFrame::Cpu(raw) => sink(raw),
        CapturedFrame::Gpu(_) => true,
    });
    match &config.source {
        Source::Monitor { id } => launch(monitor(*id)?, None, &config, epoch, stop, wrapped, None),
        Source::Region { monitor_id, .. } => launch(monitor(*monitor_id)?, crop, &config, epoch, stop, wrapped, None),
        Source::Window { id } => {
            let w = Window::from_raw_hwnd(*id as usize as *mut std::ffi::c_void);
            if !w.is_valid() {
                return Err(anyhow!("window {id} is no longer valid"));
            }
            launch(w, None, &config, epoch, stop, wrapped, None)
        }
        Source::Game { .. } => unreachable!(),
    }
}

/// Starts capturing `config.source` with optional zero-readback GPU `BGRA → NV12`
/// conversion (`GpuCaptureOptions`). When `gpu` is `None` or `gpu.gpu_active`
/// flips to `false`, frames are delivered as `CapturedFrame::Cpu(RawFrame)`.
pub fn start_with_gpu(
    config: CaptureConfig,
    gpu: Option<GpuCaptureOptions>,
    epoch: Instant,
    sink: CapturedSink,
) -> Result<CaptureHandle> {
    if let Source::Game { .. } = &config.source {
        let mut cpu_sink = sink;
        let adapter: FrameSink = Box::new(move |raw| cpu_sink(CapturedFrame::Cpu(raw)));
        return super::hook::start(config, epoch, adapter);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let crop = config.crop();
    match &config.source {
        Source::Monitor { id } => launch(monitor(*id)?, None, &config, epoch, stop, sink, gpu),
        Source::Region { monitor_id, .. } => launch(monitor(*monitor_id)?, crop, &config, epoch, stop, sink, gpu),
        Source::Window { id } => {
            let w = Window::from_raw_hwnd(*id as usize as *mut std::ffi::c_void);
            if !w.is_valid() {
                return Err(anyhow!("window {id} is no longer valid"));
            }
            launch(w, None, &config, epoch, stop, sink, gpu)
        }
        Source::Game { .. } => unreachable!(),
    }
}

fn launch<T>(
    item: T,
    crop: Option<LiveRect>,
    config: &CaptureConfig,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    sink: CapturedSink,
    gpu: Option<GpuCaptureOptions>,
) -> Result<CaptureHandle>
where
    T: TryInto<GraphicsCaptureItemType> + Send + 'static,
{
    let is_auto = config.fps == 0;
    let fps = if is_auto { 0 } else { config.fps.max(1) };
    // Windows 11 throttles delivery itself; Windows 10 delivers every vsync
    // and rejects the setting, so pick frames ourselves there.
    let native_interval = match GraphicsCaptureApi::is_minimum_update_interval_supported() {
        Ok(v) => v,
        Err(e) => {
            log::warn!("capture: minimum-update-interval probe failed ({e:?}); limiting frames ourselves");
            false
        }
    };
    let interval = if is_auto {
        if native_interval {
            // WGC `Default` on Windows 11 24H2 caps capture at ~50–60 Hz (snapping
            // to 48 fps on 144 Hz monitors). Asking for 1 ms lets DWM deliver at
            // the monitor/window's true composition rate without vsync beating.
            let min_interval = Duration::from_millis(1);
            log::info!(
                "capture: dynamic/source fps using WGC minimum update interval ({:.2} ms)",
                min_interval.as_secs_f64() * 1e3
            );
            MinimumUpdateIntervalSettings::Custom(min_interval)
        } else {
            log::info!("capture: dynamic/source fps unthrottled (WGC minimum update interval unsupported)");
            MinimumUpdateIntervalSettings::Default
        }
    } else if native_interval {
        let floor = super::min_update_interval(fps);
        log::info!("capture: {fps} fps throttled by WGC ({:.2} ms minimum update interval)", floor.as_secs_f64() * 1e3);
        MinimumUpdateIntervalSettings::Custom(floor)
    } else {
        log::info!("capture: {fps} fps throttled by PhaseLimiter (WGC minimum update interval unsupported)");
        MinimumUpdateIntervalSettings::Default
    };
    let pool = config.pool.clone().unwrap_or_else(|| FramePool::new(6));
    let flags =
        Flags { sink, epoch, stop: stop.clone(), crop, fps, limit_ourselves: !is_auto && !native_interval, pool, gpu };
    // Both of these are rejected outright — the session never starts — when the
    // OS lacks the property behind them, so ask only for what it supports.
    let border_ok = supported("border", GraphicsCaptureApi::is_border_settings_supported());
    let cursor_ok = supported("cursor", GraphicsCaptureApi::is_cursor_settings_supported());
    let mut notes = Vec::new();
    if !border_ok {
        notes.push(crate::t!(NOTE_CAPTURE_BORDER_UNSUPPORTED).to_string());
    }
    // The system default captures the cursor, which is what `show_cursor` asks
    // for anyway — only hiding it cannot be honoured.
    if !cursor_ok && !config.show_cursor {
        notes.push(crate::t!(NOTE_CAPTURE_CURSOR_UNSUPPORTED).to_string());
    }
    let settings = Settings::new(
        item,
        cursor_setting(config.show_cursor, cursor_ok),
        border_setting(border_ok),
        SecondaryWindowSettings::Default,
        interval,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    );
    let control = Handler::start_free_threaded(settings)
        .map_err(|e| anyhow!("failed to start Windows.Graphics.Capture: {e}"))?;

    let control = Arc::new(Mutex::new(Some(control)));
    let stopper = {
        let stop = stop.clone();
        Box::new(move || -> Result<()> {
            stop.store(true, Ordering::SeqCst);
            let ctl = control.lock().unwrap().take();
            if let Some(ctl) = ctl {
                ctl.stop().map_err(|e| anyhow!("stopping capture: {e}"))?;
            }
            Ok(())
        })
    };
    Ok(CaptureHandle::new(stop, stopper).with_note((!notes.is_empty()).then(|| notes.join("; "))))
}

/// Whether an optional capture setting can be changed on this OS. A failed
/// probe counts as unsupported: asking for a setting the session rejects makes
/// it refuse to start at all, so the safe answer is the system default.
/// `OPENCLIP_LEGACY_WGC` forces every answer to `false` — the Windows 10 path,
/// which has no `IsBorderRequired` (and no `IsCursorCaptureEnabled` before 2004).
fn supported(what: &str, probe: Result<bool, windows_capture::graphics_capture_api::Error>) -> bool {
    if std::env::var_os("OPENCLIP_LEGACY_WGC").is_some() {
        log::info!("capture: OPENCLIP_LEGACY_WGC set; treating the {what} setting as unsupported");
        return false;
    }
    match probe {
        Ok(true) => true,
        Ok(false) => {
            log::info!("capture: this Windows version cannot change the {what} setting; using the system default");
            false
        }
        Err(e) => {
            log::warn!("capture: {what}-setting probe failed ({e}); using the system default");
            false
        }
    }
}

/// Hide the capture border where the OS allows it (Windows 11); elsewhere the
/// system default, which draws the yellow frame around the captured item.
fn border_setting(supported: bool) -> DrawBorderSettings {
    if supported { DrawBorderSettings::WithoutBorder } else { DrawBorderSettings::Default }
}

/// Follow `show_cursor` where the OS allows it; elsewhere the system default,
/// which includes the cursor.
fn cursor_setting(show_cursor: bool, supported: bool) -> CursorCaptureSettings {
    match (supported, show_cursor) {
        (false, _) => CursorCaptureSettings::Default,
        (true, true) => CursorCaptureSettings::WithCursor,
        (true, false) => CursorCaptureSettings::WithoutCursor,
    }
}

fn monitor(id: u32) -> Result<Monitor> {
    let m = Monitor::from_raw_hmonitor(id as usize as *mut std::ffi::c_void);
    // Validate the handle by asking for its size.
    m.width().map_err(|e| anyhow!("monitor {id}: {e:?}")).context("monitor not found")?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_settings_fall_back_to_the_system_default() {
        // Windows 11: exactly what was asked for.
        assert_eq!(border_setting(true), DrawBorderSettings::WithoutBorder);
        assert_eq!(cursor_setting(true, true), CursorCaptureSettings::WithCursor);
        assert_eq!(cursor_setting(false, true), CursorCaptureSettings::WithoutCursor);
        // Windows 10: the session refuses to start unless both are `Default`.
        assert_eq!(border_setting(false), DrawBorderSettings::Default);
        assert_eq!(cursor_setting(true, false), CursorCaptureSettings::Default);
        assert_eq!(cursor_setting(false, false), CursorCaptureSettings::Default);
    }

    #[test]
    fn system_relative_time_anchors_without_jitter() {
        let mut anchor = None;
        let ms = |m| Duration::from_millis(m);
        // First frame at QPC=1_000_000 (100 ms) observed at arrival=25 ms.
        assert_eq!(Handler::map_timestamp(&mut anchor, 1_000_000, ms(25)), ms(25));
        // Second frame +69_444 ticks (6.9444 ms), even though callback woke up late at 40 ms.
        let p2 = Handler::map_timestamp(&mut anchor, 1_069_444, ms(40));
        assert_eq!(p2, ms(25) + Duration::from_nanos(6_944_400));
        // Third frame +138_888 ticks (13.8888 ms) even with early wakeup.
        let p3 = Handler::map_timestamp(&mut anchor, 1_138_888, ms(37));
        assert_eq!(p3, ms(25) + Duration::from_nanos(13_888_800));
        // Slight negative delta saturates safely against arrival anchor.
        let p0 = Handler::map_timestamp(&mut anchor, 900_000, ms(25));
        assert_eq!(p0, ms(15));
    }
}
