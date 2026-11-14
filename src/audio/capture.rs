//! Audio capture through `cpal`: microphone input and system-audio loopback.
//!
//! Streams deliver interleaved f32 chunks (at the device's native rate and
//! channel count) into a shared queue together with their arrival time, which
//! the mixer uses to place them on the recording timeline.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
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

#[allow(dead_code, clippy::large_enum_variant)]
enum AudioBackendStream {
    Cpal {
        _stream: Stream,
        _keepalive: Option<Stream>,
    },
    #[cfg(windows)]
    Wasapi(wasapi::WasapiProcessLoopback),
}

/// An open input stream; drop to close. Not `Send` on every platform, so keep
/// it on the thread that created it.
pub struct AudioSource {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub queue: SharedQueue,
    _backend: AudioBackendStream,
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

/// Opens the microphone (`name`, or the default input device).
pub fn open_microphone(name: Option<&str>) -> Result<AudioSource> {
    let host = cpal::default_host();
    let device = match name {
        Some(n) => host
            .input_devices()?
            .find(|d| device_name(d).as_deref() == Some(n))
            .ok_or_else(|| anyhow!("input device '{n}' not found"))?,
        None => host.default_input_device().context("no default input device")?,
    };
    let config = device.default_input_config().context("microphone default config")?;
    build(device, config, "microphone", None)
}

/// Opens a loopback capture of the default output device (what the speakers
/// play). Supported on Windows (WASAPI) and macOS 14.6+; on Linux, set
/// `PULSE_SOURCE=<sink>.monitor` or similar for the default input instead.
pub fn open_system_loopback() -> Result<AudioSource> {
    open_system_loopback_device(None)
}

/// Opens a loopback capture of `name`, or the default output device when `None`.
pub fn open_system_loopback_device(name: Option<&str>) -> Result<AudioSource> {
    #[cfg(windows)]
    {
        if name.is_none_or(|s| s.is_empty()) {
            match wasapi::try_open_process_loopback() {
                Ok(src) => {
                    log::info!(
                        "system audio capture: WASAPI process loopback active (excluding openclip pid {})",
                        std::process::id()
                    );
                    return Ok(src);
                }
                Err(e) => {
                    log::warn!(
                        "system audio capture: WASAPI process loopback unavailable ({e:#}); falling back to standard loopback"
                    );
                }
            }
        }
    }

    let host = cpal::default_host();
    let device = match name.filter(|s| !s.is_empty()) {
        Some(n) => host
            .output_devices()?
            .find(|d| device_name(d).as_deref() == Some(n))
            .ok_or_else(|| anyhow!("output device '{n}' not found"))?,
        None => host.default_output_device().context("no default output device")?,
    };
    let keepalive = start_loopback_keepalive(&device);
    let config = device.default_output_config().context("output device default config")?;
    log::info!(
        "system audio capture: standard loopback active ('{}')",
        device_name(&device).unwrap_or_else(|| "default".into())
    );
    build(device, config, "system audio", keepalive)
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
                    log::debug!("loopback keepalive play failed: {e}");
                    None
                } else {
                    log::debug!("loopback silent keepalive active ({format:?}, {} Hz)", config.sample_rate);
                    Some(stream)
                }
            }
            Err(e) => {
                log::debug!("loopback keepalive unavailable: {e}");
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

fn build(
    device: Device,
    supported: cpal::SupportedStreamConfig,
    what: &str,
    keepalive: Option<Stream>,
) -> Result<AudioSource> {
    let name = device_name(&device).unwrap_or_else(|| what.to_string());
    let sample_format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let queue: SharedQueue = Arc::new(Mutex::new(Queue::default()));
    let err_fn = {
        let what = what.to_string();
        move |e| log::warn!("{what} stream: {e}")
    };
    let q = queue.clone();
    let mut boosted = false;
    let mut push = move |data: Vec<f32>, info: &InputCallbackInfo| {
        if !boosted {
            boosted = true;
            #[cfg(windows)]
            promote_audio_thread();
        }
        let ts = info.timestamp();
        let age = ts.callback.duration_since(ts.capture).min(MAX_CALLBACK_AGE);
        let at = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        let mut q = q.lock().unwrap();
        if q.chunks.len() >= MAX_QUEUED_CHUNKS {
            q.chunks.pop_front();
            q.overflow += 1;
        }
        q.chunks.push_back(Chunk { at, data });
    };
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
        other => return Err(anyhow!("unsupported sample format {other:?} for {what}")),
    };
    stream.play().with_context(|| format!("starting {what} stream"))?;
    log::info!(
        "{what}: '{name}' {} Hz, {} ch, {sample_format:?}",
        config.sample_rate,
        config.channels
    );
    Ok(AudioSource {
        name,
        sample_rate: config.sample_rate,
        channels: config.channels,
        queue,
        _backend: AudioBackendStream::Cpal {
            _stream: stream,
            _keepalive: keepalive,
        },
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
mod wasapi {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use windows::core::{implement, Interface, HRESULT};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::*;
    use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

    struct SendUnknown(windows::core::IUnknown);
    unsafe impl Send for SendUnknown {}
    unsafe impl Sync for SendUnknown {}

    struct SendAudioWorker {
        event: usize,
        capture: usize,
    }

    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct ActivationHandler {
        event: HANDLE,
        result: Arc<Mutex<Option<windows::core::Result<SendUnknown>>>>,
    }

    impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
        fn ActivateCompleted(
            &self,
            operation: windows_core::Ref<'_, IActivateAudioInterfaceAsyncOperation>,
        ) -> windows::core::Result<()> {
            let op = operation.ok()?;
            let mut hr = HRESULT(0);
            let mut unk = None;
            unsafe {
                op.GetActivateResult(&mut hr, &mut unk)?;
            }
            let res = if hr.is_ok() {
                unk.map(SendUnknown).ok_or(windows::core::Error::from(windows::Win32::Foundation::E_FAIL))
            } else {
                Err(windows::core::Error::from(hr))
            };
            *self.result.lock().unwrap() = Some(res);
            unsafe {
                let _ = SetEvent(self.event);
            }
            Ok(())
        }
    }

    #[repr(C)]
    struct WaveFormatExtensible {
        format: WAVEFORMATEX,
        samples: u16,
        channel_mask: u32,
        sub_format: windows::core::GUID,
    }

    pub struct WasapiProcessLoopback {
        stop: Arc<AtomicBool>,
        event: HANDLE,
        worker: Option<JoinHandle<()>>,
        client: IAudioClient,
    }

    unsafe impl Send for WasapiProcessLoopback {}

    impl Drop for WasapiProcessLoopback {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            unsafe {
                let _ = SetEvent(self.event);
            }
            if let Some(w) = self.worker.take() {
                let _ = w.join();
            }
            unsafe {
                let _ = self.client.Stop();
                let _ = CloseHandle(self.event);
            }
        }
    }

    pub fn try_open_process_loopback() -> Result<AudioSource> {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

        let event = unsafe { CreateEventW(None, false, false, None)? };
        let result = Arc::new(Mutex::new(None));
        let handler = ActivationHandler {
            event,
            result: result.clone(),
        };
        let handler_interface: IActivateAudioInterfaceCompletionHandler = handler.into();

        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: std::process::id(),
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };

        let prop = PROPVARIANT {
            Anonymous: windows::Win32::System::Com::StructuredStorage::PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(
                    windows::Win32::System::Com::StructuredStorage::PROPVARIANT_0_0 {
                        vt: windows::Win32::System::Variant::VT_BLOB,
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: windows::Win32::System::Com::StructuredStorage::PROPVARIANT_0_0_0 {
                            blob: windows::Win32::System::Com::BLOB {
                                cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                                pBlobData: &mut params as *mut _ as *mut u8,
                            },
                        },
                    },
                ),
            },
        };

        let _op = unsafe {
            ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&prop as *const _),
                &handler_interface,
            )
        }
        .map_err(|e| {
            unsafe { let _ = CloseHandle(event); }
            anyhow!("ActivateAudioInterfaceAsync failed: {e}")
        })?;

        // Forget prop so its Drop doesn't call PropVariantClear on non-CoTaskMemAlloc stack pointer
        std::mem::forget(prop);

        let wait_res = unsafe { WaitForSingleObject(event, 2000) };
        if wait_res != WAIT_OBJECT_0 {
            unsafe { let _ = CloseHandle(event); }
            bail!("timed out waiting for process loopback audio interface activation");
        }

        let send_unknown = result
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| {
                unsafe { let _ = CloseHandle(event); }
                anyhow!("activation completed without result")
            })?
            .map_err(|e| {
                unsafe { let _ = CloseHandle(event); }
                anyhow!("activation result error: {e}")
            })?;

        let client: IAudioClient = send_unknown.0.cast().map_err(|e| {
            unsafe { let _ = CloseHandle(event); }
            anyhow!("cast to IAudioClient failed: {e}")
        })?;

        let sample_rate = 48000u32;
        let channels = 2u16;
        let ksd_subtype_ieee_float = windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
        let wf = WaveFormatExtensible {
            format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: channels,
                nSamplesPerSec: sample_rate,
                nAvgBytesPerSec: sample_rate * (channels as u32) * 4,
                nBlockAlign: channels * 4,
                wBitsPerSample: 32,
                cbSize: 22,
            },
            samples: 32,
            channel_mask: 0x3,
            sub_format: ksd_subtype_ieee_float,
        };

        let sample_event = unsafe { CreateEventW(None, false, false, None)? };
        let buffer_duration = 10_000_000i64; // 1 second buffer
        let init_res = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                buffer_duration,
                0,
                &wf as *const _ as *const _,
                None,
            )
        };
        if let Err(e) = init_res {
            unsafe {
                let _ = CloseHandle(sample_event);
                let _ = CloseHandle(event);
            }
            bail!("IAudioClient::Initialize failed: {e}");
        }

        if let Err(e) = unsafe { client.SetEventHandle(sample_event) } {
            unsafe {
                let _ = CloseHandle(sample_event);
                let _ = CloseHandle(event);
            }
            bail!("IAudioClient::SetEventHandle failed: {e}");
        }

        let capture_client: IAudioCaptureClient = match unsafe { client.GetService() } {
            Ok(c) => c,
            Err(e) => {
                unsafe {
                    let _ = CloseHandle(sample_event);
                    let _ = CloseHandle(event);
                }
                bail!("IAudioClient::GetService<IAudioCaptureClient> failed: {e}");
            }
        };

        if let Err(e) = unsafe { client.Start() } {
            unsafe {
                let _ = CloseHandle(sample_event);
                let _ = CloseHandle(event);
            }
            bail!("IAudioClient::Start failed: {e}");
        }

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let queue: SharedQueue = Arc::new(Mutex::new(Queue::default()));
        let queue_clone = queue.clone();
        let worker_args = SendAudioWorker {
            event: sample_event.0 as usize,
            capture: unsafe { std::mem::transmute_copy(&capture_client) },
        };
        // Suppress Drop of capture_client here since ownership was transferred to worker thread
        std::mem::forget(capture_client);

        let worker = std::thread::Builder::new()
            .name("wasapi-process-loopback".into())
            .spawn(move || {
                let sample_event = HANDLE(worker_args.event as *mut std::ffi::c_void);
                let capture_client: IAudioCaptureClient = unsafe { std::mem::transmute_copy(&worker_args.capture) };
                promote_audio_thread();
                while !stop_clone.load(Ordering::Relaxed) {
                    let wr = unsafe { WaitForSingleObject(sample_event, 50) };
                    if stop_clone.load(Ordering::Relaxed) {
                        break;
                    }
                    if wr == WAIT_OBJECT_0 || wr == WAIT_TIMEOUT {
                        loop {
                            let mut p_data = std::ptr::null_mut();
                            let mut num_frames = 0u32;
                            let mut flags = 0u32;
                            let mut dev_pos = 0u64;
                            let mut qpc_pos = 0u64;
                            let hr = unsafe {
                                capture_client.GetBuffer(
                                    &mut p_data,
                                    &mut num_frames,
                                    &mut flags,
                                    Some(&mut dev_pos),
                                    Some(&mut qpc_pos),
                                )
                            };
                            if hr.is_err() || num_frames == 0 {
                                break;
                            }
                            let is_silent = (flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)) != 0;
                            let total_floats = (num_frames as usize) * 2;
                            let data = if is_silent {
                                vec![0.0f32; total_floats]
                            } else {
                                let slice = unsafe { std::slice::from_raw_parts(p_data as *const f32, total_floats) };
                                slice.to_vec()
                            };
                            unsafe {
                                let _ = capture_client.ReleaseBuffer(num_frames);
                            }
                            let at = Instant::now();
                            let mut q = queue_clone.lock().unwrap();
                            if q.chunks.len() >= MAX_QUEUED_CHUNKS {
                                q.chunks.pop_front();
                                q.overflow += 1;
                            }
                            q.chunks.push_back(Chunk { at, data });
                        }
                    }
                }
            })
            .ok();

        unsafe {
            let _ = CloseHandle(event);
        }

        let loopback = WasapiProcessLoopback {
            stop,
            event: sample_event,
            worker,
            client,
        };

        Ok(AudioSource {
            name: "WASAPI Process Loopback".into(),
            sample_rate,
            channels,
            queue,
            _backend: AudioBackendStream::Wasapi(loopback),
        })
    }
}
