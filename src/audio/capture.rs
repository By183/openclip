//! Audio capture through `cpal`: microphone input and system-audio loopback.
//!
//! Streams deliver interleaved f32 chunks (at the device's native rate and
//! channel count) into a shared queue together with their arrival time, which
//! the mixer uses to place them on the recording timeline.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, InputCallbackInfo, Sample, SampleFormat, Stream, StreamConfig, I24};

/// A block of interleaved samples with its arrival time.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub at: Instant,
    pub data: Vec<f32>,
}

#[derive(Debug, Default)]
pub struct Queue {
    pub chunks: VecDeque<Chunk>,
    pub overflow: u64,
}

pub type SharedQueue = Arc<Mutex<Queue>>;

const MAX_QUEUED_CHUNKS: usize = 512;

/// Maximum age clamped from `InputCallbackInfo` timestamps when reconstructing
/// packet end times on WASAPI.
const MAX_CALLBACK_AGE: Duration = Duration::from_millis(250);

/// Telemetry statistics for an audio source stream.
#[derive(Debug, Default)]
pub struct AudioSourceStats {
    pub total_frames: std::sync::atomic::AtomicU64,
    pub last_chunk_instant: Mutex<Option<Instant>>,
    pub max_interval_us: std::sync::atomic::AtomicU64,
}

/// An open input stream; drop to close. Not `Send` on every platform, so keep
/// it on the thread that created it.
pub struct AudioSource {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub queue: SharedQueue,
    pub reconnect: Arc<AtomicBool>,
    pub stats: Arc<AudioSourceStats>,
    _stream: Stream,
    _keepalive: Option<Stream>,
}

impl AudioSource {
    pub fn needs_reconnect(&self) -> bool {
        self.reconnect.load(Ordering::SeqCst)
    }

    pub fn flag_reconnect(&self) {
        self.reconnect.store(true, Ordering::SeqCst);
    }
}

/// Names of available microphone / input devices.
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names = Vec::new();
    if let Ok(devices) = host.input_devices() {
        for d in devices {
            if let Some(name) = device_name(&d) {
                names.push(name);
            }
        }
    }
    names
}

/// Names of available playback / output devices (for system audio loopback).
pub fn list_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names = Vec::new();
    if let Ok(devices) = host.output_devices() {
        for d in devices {
            if let Some(name) = device_name(&d) {
                names.push(name);
            }
        }
    }
    names
}

fn device_name(d: &Device) -> Option<String> {
    d.description().ok().map(|desc| desc.name().to_string())
}

fn is_default_device(name: Option<&str>) -> bool {
    match name {
        None => true,
        Some(s) => s.trim().is_empty() || s.eq_ignore_ascii_case("default"),
    }
}

/// Names of current default output and input devices.
pub fn default_output_device_name() -> Option<String> {
    let host = cpal::default_host();
    host.default_output_device().and_then(|d| device_name(&d))
}

pub fn default_input_device_name() -> Option<String> {
    let host = cpal::default_host();
    host.default_input_device().and_then(|d| device_name(&d))
}

/// Opens the microphone (`name`, or the default input device).
pub fn open_microphone(name: Option<&str>) -> Result<AudioSource> {
    let host = cpal::default_host();
    let device = if is_default_device(name) {
        host.default_input_device().context("no default input device")?
    } else {
        let n = name.unwrap();
        host.input_devices()?
            .find(|d| device_name(d).as_deref() == Some(n))
            .ok_or_else(|| anyhow!("input device '{n}' not found"))?
    };
    let config = device.default_input_config().context("microphone default config")?;
    build(device, config, "microphone", None, None)
}

/// Opens a loopback capture of the default output device (what the speakers
/// play). Supported on Windows (WASAPI) and macOS 14.6+; on Linux, set
/// `PULSE_SOURCE=<sink>.monitor` or similar for the default input instead.
pub fn open_system_loopback() -> Result<AudioSource> {
    open_system_loopback_device(None)
}

/// Opens a loopback capture of `name`, or the default output device when `None`.
pub fn open_system_loopback_device(name: Option<&str>) -> Result<AudioSource> {
    let host = cpal::default_host();
    let device = if is_default_device(name) {
        host.default_output_device().context("no default output device")?
    } else {
        let n = name.unwrap();
        host.output_devices()?
            .find(|d| device_name(d).as_deref() == Some(n))
            .ok_or_else(|| anyhow!("output device '{n}' not found"))?
    };
    let keepalive = start_loopback_keepalive(&device);
    let config = device.default_output_config().context("output device default config")?;
    log::info!(
        "system audio capture: standard loopback active ('{}')",
        device_name(&device).unwrap_or_else(|| "default".into())
    );
    // Explicit enlarged buffer: ~96 ms (e.g. 4608 frames at 48 kHz) to prevent driver underruns
    let target_buffer_frames = ((config.sample_rate() as f64) * 0.096).round() as u32;
    build(device, config, "system audio", keepalive, Some(target_buffer_frames))
}

/// Opens a shared-mode silent output stream on the loopback playback device.
/// On Windows WASAPI, loopback capture suspends callbacks and raises
/// discontinuity flags whenever the last active render stream closes; keeping
/// one silent client open prevents desktop silence dropouts and resume stutters.
fn start_loopback_keepalive(device: &Device) -> Option<Stream> {
    #[cfg(windows)]
    {
        let Ok(supported) = device.default_output_config() else {
            return None;
        };
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let err_fn = |e| log::debug!("loopback keepalive stream: {e}");
        let res = match format {
            SampleFormat::F32 => device.build_output_stream(
                config,
                |out: &mut [f32], _| out.fill(f32::EQUILIBRIUM),
                err_fn,
                None,
            ),
            SampleFormat::F64 => device.build_output_stream(
                config,
                |out: &mut [f64], _| out.fill(f64::EQUILIBRIUM),
                err_fn,
                None,
            ),
            SampleFormat::I16 => device.build_output_stream(
                config,
                |out: &mut [i16], _| out.fill(i16::EQUILIBRIUM),
                err_fn,
                None,
            ),
            SampleFormat::U16 => device.build_output_stream(
                config,
                |out: &mut [u16], _| out.fill(u16::EQUILIBRIUM),
                err_fn,
                None,
            ),
            SampleFormat::I24 => device.build_output_stream(
                config,
                |out: &mut [I24], _| out.fill(I24::EQUILIBRIUM),
                err_fn,
                None,
            ),
            SampleFormat::I32 => device.build_output_stream(
                config,
                |out: &mut [i32], _| out.fill(i32::EQUILIBRIUM),
                err_fn,
                None,
            ),
            other => {
                log::debug!("loopback keepalive unsupported format {other:?}");
                return None;
            }
        };
        match res {
            Ok(stream) => {
                if let Err(e) = stream.play() {
                    log::warn!("loopback keepalive play failed: {e}");
                    None
                } else {
                    log::info!(
                        "_keepalive: '{}' {} Hz, {} ch, {format:?}, buffer: Default",
                        device_name(device).unwrap_or_else(|| "default".into()),
                        config.sample_rate,
                        config.channels
                    );
                    Some(stream)
                }
            }
            Err(e) => {
                log::warn!("loopback keepalive unavailable: {e}");
                None
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = device;
        None
    }
}

fn build_input_stream_for_format<F, E>(
    device: &Device,
    sample_format: SampleFormat,
    config: StreamConfig,
    mut push: F,
    err_fn: E,
) -> Result<Stream>
where
    F: FnMut(Vec<f32>, &InputCallbackInfo) + Send + 'static,
    E: FnMut(cpal::Error) + Send + 'static,
{
    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            config,
            move |d: &[f32], info| push(d.to_vec(), info),
            err_fn,
            None,
        )?,
        SampleFormat::F64 => device.build_input_stream(
            config,
            move |d: &[f64], info| push(d.iter().map(|s| s.to_sample::<f32>()).collect(), info),
            err_fn,
            None,
        )?,
        SampleFormat::I16 => device.build_input_stream(
            config,
            move |d: &[i16], info| push(d.iter().map(|&s| s as f32 / 32768.0).collect(), info),
            err_fn,
            None,
        )?,
        SampleFormat::U16 => device.build_input_stream(
            config,
            move |d: &[u16], info| push(d.iter().map(|&s| (s as f32 - 32768.0) / 32768.0).collect(), info),
            err_fn,
            None,
        )?,
        SampleFormat::I24 => device.build_input_stream(
            config,
            move |d: &[I24], info| push(d.iter().map(|s| s.to_sample::<f32>()).collect(), info),
            err_fn,
            None,
        )?,
        SampleFormat::I32 => device.build_input_stream(
            config,
            move |d: &[i32], info| push(d.iter().map(|&s| s as f32 / 2147483648.0).collect(), info),
            err_fn,
            None,
        )?,
        other => return Err(anyhow!("unsupported sample format {other:?}")),
    };
    Ok(stream)
}

fn build(
    device: Device,
    supported: cpal::SupportedStreamConfig,
    what: &str,
    keepalive: Option<Stream>,
    target_buffer_frames: Option<u32>,
) -> Result<AudioSource> {
    let name = device_name(&device).unwrap_or_else(|| what.to_string());
    let sample_format = supported.sample_format();
    let sample_rate = supported.sample_rate();
    let channels = supported.channels();
    let queue: SharedQueue = Arc::new(Mutex::new(Queue::default()));
    let reconnect = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(AudioSourceStats::default());

    let err_fn_gen = {
        let what = what.to_string();
        let rec = reconnect.clone();
        move || {
            let what = what.clone();
            let rec = rec.clone();
            move |e: cpal::Error| {
                let err_str = e.to_string();
                let lower = err_str.to_lowercase();
                let is_transient = e.kind() == cpal::ErrorKind::Xrun
                    || e.kind() == cpal::ErrorKind::DeviceChanged
                    || lower.contains("underrun")
                    || lower.contains("overrun");
                if is_transient {
                    log::warn!("audio: {what} stream glitch (transient underrun/overrun, continuing): {err_str}");
                } else {
                    log::error!("audio: {what} fatal stream error: {err_str}, triggering reconnect");
                    rec.store(true, Ordering::SeqCst);
                }
            }
        }
    };

    let push_gen = {
        let q = queue.clone();
        let stats = stats.clone();
        let what = what.to_string();
        let channels = channels as usize;
        move || {
            let q = q.clone();
            let stats = stats.clone();
            let what = what.clone();
            let mut boosted = false;
            move |data: Vec<f32>, info: &InputCallbackInfo| {
                if !boosted {
                    boosted = true;
                    #[cfg(windows)]
                    promote_audio_thread();
                }
                let now = Instant::now();
                let frames = data.len() / channels.max(1);
                stats.total_frames.fetch_add(frames as u64, Ordering::Relaxed);
                {
                    let mut last = stats.last_chunk_instant.lock().unwrap();
                    if let Some(prev) = *last {
                        let interval_us = now.duration_since(prev).as_micros() as u64;
                        stats.max_interval_us.fetch_max(interval_us, Ordering::Relaxed);
                    }
                    *last = Some(now);
                }
                let ts = info.timestamp();
                let age = ts.callback.duration_since(ts.capture).min(MAX_CALLBACK_AGE);
                let at = now.checked_sub(age).unwrap_or(now);
                let mut q = q.lock().unwrap();
                if q.chunks.len() >= MAX_QUEUED_CHUNKS
                    && let Some(dropped) = q.chunks.pop_front()
                {
                    let dropped_samples = dropped.data.len();
                    let dropped_frames = dropped_samples / channels.max(1);
                    q.overflow += 1;
                    log::warn!(
                        "audio: [{what}] queue overflow! Dropped chunk with {dropped_frames} frames ({dropped_samples} samples), total overflows: {}",
                        q.overflow
                    );
                }
                q.chunks.push_back(Chunk { at, data });
            }
        }
    };

    let (stream, buffer_desc) = if let Some(target_frames) = target_buffer_frames {
        let mut fixed_config: StreamConfig = supported.into();
        fixed_config.buffer_size = cpal::BufferSize::Fixed(target_frames);
        match build_input_stream_for_format(&device, sample_format, fixed_config, push_gen(), err_fn_gen()) {
            Ok(s) => {
                let ms = target_frames as f64 * 1000.0 / fixed_config.sample_rate as f64;
                (s, format!("Fixed({target_frames} frames, ~{ms:.1} ms)"))
            }
            Err(e) => {
                log::warn!("audio: {what} failed with fixed buffer {target_frames} frames ({e:#}), falling back to default buffer");
                let default_config: StreamConfig = supported.into();
                let s = build_input_stream_for_format(&device, sample_format, default_config, push_gen(), err_fn_gen())?;
                (s, "Default (fallback)".to_string())
            }
        }
    } else {
        let default_config: StreamConfig = supported.into();
        let s = build_input_stream_for_format(&device, sample_format, default_config, push_gen(), err_fn_gen())?;
        (s, "Default".to_string())
    };

    stream.play().with_context(|| format!("starting {what} stream"))?;
    log::info!(
        "{what}: '{name}' {} Hz, {} ch, {sample_format:?}, buffer: {buffer_desc}",
        sample_rate,
        channels
    );
    Ok(AudioSource {
        name,
        sample_rate,
        channels,
        queue,
        reconnect,
        stats,
        _stream: stream,
        _keepalive: keepalive,
    })
}

/// Raises the audio capture callback thread to time-critical priority on Windows
/// so heavy encoding or 3D rendering does not preempt WASAPI packet reads.
#[cfg(windows)]
fn promote_audio_thread() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };
    // SAFETY: `GetCurrentThread()` returns a pseudo-handle for the calling thread.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }
}

#[cfg(windows)]
mod watcher {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use windows::core::{implement, PCWSTR};
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

    #[implement(IMMNotificationClient)]
    struct AudioWatcher {
        changed: Arc<AtomicBool>,
    }

    impl IMMNotificationClient_Impl for AudioWatcher_Impl {
        fn OnDeviceStateChanged(&self, _pwstrdeviceid: &PCWSTR, _dwnewstate: DEVICE_STATE) -> windows::core::Result<()> {
            log::debug!("audio device state changed");
            self.changed.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn OnDeviceAdded(&self, _pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
            log::debug!("audio device added");
            self.changed.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn OnDeviceRemoved(&self, _pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
            log::debug!("audio device removed");
            self.changed.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn OnDefaultDeviceChanged(&self, _flow: EDataFlow, _role: ERole, _pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
            log::debug!("audio default device changed");
            self.changed.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> windows::core::Result<()> {
            Ok(())
        }
    }

    pub struct DeviceNotificationListener {
        pub changed: Arc<AtomicBool>,
        enumerator: Option<IMMDeviceEnumerator>,
        client: Option<IMMNotificationClient>,
    }

    impl DeviceNotificationListener {
        pub fn new() -> Self {
            let changed = Arc::new(AtomicBool::new(false));
            // SAFETY: Initializes COM on the current thread and registers the endpoint notification callback.
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let enumerator: windows::core::Result<IMMDeviceEnumerator> =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL);
                match enumerator {
                    Ok(enumerator) => {
                        let watcher: IMMNotificationClient = AudioWatcher { changed: changed.clone() }.into();
                        if let Err(e) = enumerator.RegisterEndpointNotificationCallback(&watcher) {
                            log::warn!("failed to register audio endpoint notification callback: {e}");
                            Self { changed, enumerator: None, client: None }
                        } else {
                            log::debug!("audio endpoint notification callback registered");
                            Self { changed, enumerator: Some(enumerator), client: Some(watcher) }
                        }
                    }
                    Err(e) => {
                        log::warn!("failed to create MMDeviceEnumerator: {e}");
                        Self { changed, enumerator: None, client: None }
                    }
                }
            }
        }
    }

    impl Default for DeviceNotificationListener {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for DeviceNotificationListener {
        fn drop(&mut self) {
            if let (Some(enumerator), Some(client)) = (&self.enumerator, &self.client) {
                // SAFETY: Unregisters the client callback before releasing COM interfaces.
                unsafe {
                    let _ = enumerator.UnregisterEndpointNotificationCallback(client);
                }
            }
        }
    }
}

#[cfg(windows)]
pub use watcher::DeviceNotificationListener;

#[cfg(not(windows))]
#[derive(Default)]
pub struct DeviceNotificationListener {
    pub changed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(not(windows))]
impl DeviceNotificationListener {
    pub fn new() -> Self {
        Self {
            changed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}




