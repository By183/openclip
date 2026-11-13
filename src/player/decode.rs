//! Media Foundation decoding for the viewer.
//!
//! One [`IMFSourceReader`] with the first video and the first audio stream
//! selected, decoding to RGB32 and 16-bit PCM. It builds on the poster-frame
//! reader in [`crate::video::thumbnail`] and adds what playback needs: sample
//! timestamps, both streams at once, format-change handling and seeking.
//!
//! Everything here is thread-affine — the reader must be created, used and
//! dropped on one thread with a live `ComGuard`.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{GUID, HSTRING, Interface};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFDXGIBuffer, IMFMediaType, IMFSample, IMFSourceReader, MFAudioFormat_PCM, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Audio, MFMediaType_Video, MFVideoArea,
    MFVideoFormat_NV12, MFVideoFormat_RGB32, MFVideoTransferMatrix_BT601, MFVideoTransferMatrix_BT709,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE,
    MF_MT_YUV_MATRIX, MF_PD_DURATION, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR,
    MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED, MF_SOURCE_READERF_NEWSTREAM, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_D3D_MANAGER, MF_SOURCE_READER_DISABLE_DXVA, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_MEDIASOURCE,
};

use crate::capture::FramePool;
use crate::video::d3d11_vp::GpuPlaybackProcessor;
use crate::video::mf::{propvariant_i64, startup};
use crate::video::preview::{rows_to_rgba_into, PreviewImage};

use super::frame_interval_from_rate;

/// `GUID_NULL` — the default (100 ns) time format for a seek. The constant
/// itself lives behind the `Win32_Media_KernelStreaming` feature, which this
/// crate does not enable, so it is spelled out here.
const TIME_FORMAT_NULL: GUID = GUID::from_u128(0);

/// How many stream indices to probe when looking for video and audio.
const MAX_STREAMS: u32 = 16;

#[derive(Debug, Clone, Copy)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub interval: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct AudioInfo {
    pub rate: u32,
    pub channels: u16,
}

/// What one `ReadSample` produced.
pub enum Packet {
    Video { image: PreviewImage, pts: Duration },
    /// A video sample deliberately left unconverted — seek pre-roll. Skipping
    /// the pixel work here is what makes seeking feel immediate.
    VideoSkipped { pts: Duration },
    Audio { samples: Vec<f32>, channels: usize, pts: Duration },
    /// A gap, a format change or a stream that ended while others continue.
    Idle,
    /// Every selected stream has ended.
    End,
}

pub struct Reader {
    reader: IMFSourceReader,
    gpu: Option<GpuPlaybackProcessor>,
    video: Option<u32>,
    audio: Option<u32>,
    video_eos: bool,
    audio_eos: bool,
    /// Row pitch of the decoded frame (`RGB32` or fallback `NV12`); negative means bottom-up.
    stride: i32,
    /// Coded surface dimensions from `MF_MT_FRAME_SIZE` (GPU NV12 mode).
    coded_size: (u32, u32),
    /// Visible crop aperture within `coded_size` (GPU NV12 mode).
    crop_rect: RECT,
    /// Transfer matrix (`true` = BT.709, `false` = BT.601) for GPU NV12 -> RGBA.
    bt709: bool,
    /// Longest side asked of the decoder, re-applied after a format change.
    max_side: u32,
    pub duration: Option<Duration>,
    pub video_info: Option<VideoInfo>,
    pub audio_info: Option<AudioInfo>,
}

impl Reader {
    /// Opens `path`, asking for frames no larger than `max_side` on their
    /// longer side. Tries GPU DXVA hardware decoding + `ID3D11VideoProcessor`
    /// downscaling first, then falls back to CPU `RGB32` decoding.
    pub fn open(path: &Path, max_side: u32) -> Result<Reader> {
        match Reader::open_gpu(path, max_side) {
            Ok(r) => {
                if let Some(v) = r.video_info {
                    log::info!(
                        "player decoder: GPU DXVA NV12 -> RGBA ({}x{} -> {}x{}, bt709={})",
                        r.coded_size.0,
                        r.coded_size.1,
                        v.width,
                        v.height,
                        r.bt709
                    );
                }
                return Ok(r);
            }
            Err(e) => {
                log::info!("player decoder: GPU DXVA unavailable ({e:#}); falling back to CPU RGB32");
            }
        }
        match Reader::open_with(path, true, max_side) {
            Ok(r) => Ok(r),
            Err(e) => {
                log::debug!("advanced video processing unavailable ({e:#}); retrying without it");
                Reader::open_with(path, false, max_side)
            }
        }
    }

    fn open_gpu(path: &Path, max_side: u32) -> Result<Reader> {
        startup()?;
        let gpu = GpuPlaybackProcessor::new()?;
        let abs = std::fs::canonicalize(path).context("resolving the file path")?;
        let url = HSTRING::from(abs.to_string_lossy().trim_start_matches(r"\\?\"));
        let mut attrs: Option<IMFAttributes> = None;
        // SAFETY: `attrs` and `gpu.device_manager()` remain alive through reader creation.
        unsafe {
            MFCreateAttributes(&mut attrs, 3).context("MFCreateAttributes")?;
            let attrs = attrs.ok_or_else(|| anyhow!("MFCreateAttributes returned nothing"))?;
            attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, gpu.device_manager())?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUINT32(&MF_SOURCE_READER_DISABLE_DXVA, 0)?;
            let reader: IMFSourceReader =
                MFCreateSourceReaderFromURL(&url, &attrs).context("opening the file with D3D11 manager")?;
            Self::finish_open(reader, Some(gpu), false, max_side)
        }
    }

    fn open_with(path: &Path, advanced: bool, max_side: u32) -> Result<Reader> {
        startup()?;
        let abs = std::fs::canonicalize(path).context("resolving the file path")?;
        // Media Foundation rejects the extended-length prefix.
        let url = HSTRING::from(abs.to_string_lossy().trim_start_matches(r"\\?\"));
        let mut attrs: Option<IMFAttributes> = None;
        // SAFETY: every call gets live, valid arguments and each failure is
        // turned into an error without retaining anything.
        unsafe {
            MFCreateAttributes(&mut attrs, 1).context("MFCreateAttributes")?;
            let attrs = attrs.ok_or_else(|| anyhow!("MFCreateAttributes returned nothing"))?;
            // Lets the reader insert a converter so RGB32 can be requested
            // whatever the decoder natively produces.
            let key = if advanced {
                MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING
            } else {
                MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING
            };
            attrs.SetUINT32(&key, 1)?;
            let reader: IMFSourceReader =
                MFCreateSourceReaderFromURL(&url, &attrs).context("opening the file")?;
            Self::finish_open(reader, None, advanced, max_side)
        }
    }

    unsafe fn finish_open(
        reader: IMFSourceReader,
        mut gpu: Option<GpuPlaybackProcessor>,
        advanced: bool,
        max_side: u32,
    ) -> Result<Reader> {
        let duration = unsafe {
            reader
                .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
                .ok()
                .map(|v| Duration::from_nanos(v.Anonymous.Anonymous.Anonymous.uhVal * 100))
                .filter(|d| !d.is_zero())
        };

        // `ReadSample(ALL_STREAMS, ..)` reports the *real* stream index, not
        // the FIRST_VIDEO/FIRST_AUDIO sentinel, so resolve them up front.
        let (mut video, mut audio) = (None, None);
        for i in 0..MAX_STREAMS {
            let Ok(t) = (unsafe { reader.GetNativeMediaType(i, 0) }) else { break };
            let Ok(major) = (unsafe { t.GetGUID(&MF_MT_MAJOR_TYPE) }) else { continue };
            if major == MFMediaType_Video && video.is_none() {
                video = Some(i);
            } else if major == MFMediaType_Audio && audio.is_none() {
                audio = Some(i);
            }
        }
        if video.is_none() && audio.is_none() {
            bail!("no video or audio stream");
        }
        if video.is_none() {
            gpu = None;
        }

        unsafe {
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            if let Some(i) = video {
                reader.SetStreamSelection(i, true)?;
            }
            if let Some(i) = audio {
                reader.SetStreamSelection(i, true)?;
            }
        }

        let mut me = Reader {
            reader,
            gpu,
            video,
            audio,
            video_eos: video.is_none(),
            audio_eos: audio.is_none(),
            stride: 0,
            coded_size: (0, 0),
            crop_rect: RECT::default(),
            bt709: true,
            max_side,
            duration,
            video_info: None,
            audio_info: None,
        };
        if let Some(i) = video {
            me.setup_video(i, advanced)?;
        }
        // A file without usable sound still plays; only video is fatal.
        if let Some(i) = audio
            && let Err(e) = me.setup_audio(i)
        {
            log::warn!("audio track unusable ({e:#}); playing without sound");
            unsafe { me.reader.SetStreamSelection(i, false).ok() };
            me.audio = None;
            me.audio_eos = true;
        }
        Ok(me)
    }

    /// Negotiates GPU `NV12` (when `self.gpu.is_some()`) or CPU `RGB32`, then
    /// reads back the geometry and color matrix actually negotiated.
    fn setup_video(&mut self, stream: u32, advanced: bool) -> Result<()> {
        if self.gpu.is_some() {
            // SAFETY: MF calls on a live reader; failures propagate to CPU fallback.
            unsafe {
                let want = MFCreateMediaType()?;
                want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
                self.reader
                    .SetCurrentMediaType(stream, None, &want)
                    .context("negotiating NV12 DXVA video")?;
            }
            return self.refresh_video(stream);
        }

        // SAFETY: MF calls on a live reader; failures propagate as errors.
        unsafe {
            let native = self.reader.GetNativeMediaType(stream, 0).ok();
            let cap = native
                .as_ref()
                .and_then(|t| t.GetUINT64(&MF_MT_FRAME_SIZE).ok())
                .map(|s| ((s >> 32) as u32, (s & 0xFFFF_FFFF) as u32))
                .and_then(|(w, h)| capped_size(w, h, self.max_side));

            let make = |size: Option<(u32, u32)>| -> Result<IMFMediaType> {
                let want = MFCreateMediaType()?;
                want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
                if let Some((w, h)) = size {
                    want.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
                }
                Ok(want)
            };

            // Only the advanced processor can resize; if the sized request is
            // refused, fall back to the source's own dimensions.
            let sized = advanced.then_some(cap).flatten();
            if sized.is_none()
                || self.reader.SetCurrentMediaType(stream, None, &make(sized)?).is_err()
            {
                self.reader
                    .SetCurrentMediaType(stream, None, &make(None)?)
                    .context("negotiating RGB32 video")?;
            }
            self.refresh_video(stream)
        }
    }

    fn refresh_video(&mut self, stream: u32) -> Result<()> {
        // SAFETY: reading attributes off a media type we own.
        unsafe {
            let mt = self.reader.GetCurrentMediaType(stream)?;
            let size = mt.GetUINT64(&MF_MT_FRAME_SIZE).context("frame size")?;
            let (coded_w, coded_h) = ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32);
            if coded_w == 0 || coded_h == 0 {
                bail!("the video stream reports an empty frame size");
            }
            let interval = frame_interval_from_rate(mt.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0));

            if self.gpu.is_some() {
                self.coded_size = (coded_w, coded_h);
                self.stride = mt
                    .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                    .map(|s| s as i32)
                    .unwrap_or(coded_w as i32);
                if self.stride == 0 {
                    self.stride = coded_w as i32;
                }

                let mut area = MFVideoArea::default();
                let area_bytes = std::slice::from_raw_parts_mut(
                    (&mut area as *mut MFVideoArea).cast::<u8>(),
                    std::mem::size_of::<MFVideoArea>(),
                );
                let (crop_rect, disp_w, disp_h) = if mt.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, area_bytes, None).is_ok()
                    && area.Area.cx > 0
                    && area.Area.cy > 0
                {
                    let left = area.OffsetX.value as i32;
                    let top = area.OffsetY.value as i32;
                    (
                        RECT { left, top, right: left + area.Area.cx, bottom: top + area.Area.cy },
                        area.Area.cx as u32,
                        area.Area.cy as u32,
                    )
                } else {
                    (RECT { left: 0, top: 0, right: coded_w as i32, bottom: coded_h as i32 }, coded_w, coded_h)
                };
                self.crop_rect = crop_rect;
                self.bt709 = match mt.GetUINT32(&MF_MT_YUV_MATRIX) {
                    Ok(m) if m == MFVideoTransferMatrix_BT601.0 as u32 => false,
                    Ok(m) if m == MFVideoTransferMatrix_BT709.0 as u32 => true,
                    Ok(_) | Err(_) => disp_w >= 1280 || disp_h >= 720,
                };
                let (out_w, out_h) = capped_size(disp_w, disp_h, self.max_side).unwrap_or((disp_w, disp_h));
                self.video_info = Some(VideoInfo { width: out_w, height: out_h, interval });
            } else {
                // RGB32 is commonly bottom-up, which the negative stride announces.
                self.stride = mt
                    .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                    .map(|s| s as i32)
                    .unwrap_or((coded_w * 4) as i32);
                if self.stride == 0 {
                    self.stride = (coded_w * 4) as i32;
                }
                self.video_info = Some(VideoInfo { width: coded_w, height: coded_h, interval });
            }
            Ok(())
        }
    }

    /// Pins only PCM and 16 bits, then takes whatever rate and channel count MF
    /// settles on — demanding the device's exact layout here is what makes some
    /// decoders refuse outright.
    fn setup_audio(&mut self, stream: u32) -> Result<()> {
        // SAFETY: MF calls on a live reader.
        unsafe {
            let want = MFCreateMediaType()?;
            want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            want.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            want.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            self.reader.SetCurrentMediaType(stream, None, &want).context("negotiating PCM audio")?;
            self.refresh_audio(stream)
        }
    }

    fn refresh_audio(&mut self, stream: u32) -> Result<()> {
        // SAFETY: reading attributes off a media type we own.
        unsafe {
            let mt = self.reader.GetCurrentMediaType(stream)?;
            let bits = mt.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).unwrap_or(16);
            if bits != 16 {
                bail!("decoder produced {bits}-bit samples, expected 16");
            }
            let rate = mt.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).context("sample rate")?;
            let channels = mt.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(2).max(1) as u16;
            if rate == 0 {
                bail!("the audio stream reports a zero sample rate");
            }
            self.audio_info = Some(AudioInfo { rate, channels });
            Ok(())
        }
    }

    pub fn has_video(&self) -> bool {
        self.video.is_some()
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    pub fn has_live_audio(&self) -> bool {
        self.audio.is_some() && !self.audio_eos
    }

    fn finished(&self) -> bool {
        self.video_eos && self.audio_eos
    }

    /// Reads one sample. Video samples whose timestamp is before `pixels_from`
    /// come back as [`Packet::VideoSkipped`] without being converted.
    ///
    /// `audio_only` restricts the read to the audio stream. The caller uses it
    /// when video is buffered as far ahead as it may run but the output device
    /// still wants sound: reading every stream there would decode video frames
    /// that the backlog then has to throw away.
    pub fn read(
        &mut self,
        pixels_from: Option<Duration>,
        pool: &FramePool,
        audio_only: bool,
    ) -> Result<Packet> {
        let stream = match (audio_only, self.audio) {
            (true, Some(i)) => i,
            _ => MF_SOURCE_READER_ALL_STREAMS.0 as u32,
        };
        let mut index = 0u32;
        let mut flags = 0u32;
        let mut ts = 0i64;
        let mut sample: Option<IMFSample> = None;
        // SAFETY: all out-parameters are live locals for the duration of the call.
        unsafe {
            self.reader.ReadSample(
                stream,
                0,
                Some(&mut index),
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            )?;
        }
        let has = |f: i32| flags & f as u32 != 0;

        if has(MF_SOURCE_READERF_ERROR.0) {
            bail!("the decoder reported an error");
        }
        // The topology changed under us: re-apply both requested output types.
        if has(MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0) || has(MF_SOURCE_READERF_NEWSTREAM.0) {
            if let Some(i) = self.video {
                self.setup_video(i, true).ok();
            }
            if let Some(i) = self.audio {
                self.setup_audio(i).ok();
            }
            return Ok(Packet::Idle);
        }
        if has(MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0) {
            if Some(index) == self.video {
                self.refresh_video(index).ok();
            } else if Some(index) == self.audio {
                self.refresh_audio(index).ok();
            }
        }
        if has(MF_SOURCE_READERF_ENDOFSTREAM.0) {
            if Some(index) == self.video {
                self.video_eos = true;
            }
            if Some(index) == self.audio {
                self.audio_eos = true;
            }
            return Ok(if self.finished() { Packet::End } else { Packet::Idle });
        }
        // A stream tick, or simply nothing this time round.
        let Some(sample) = sample else { return Ok(Packet::Idle) };
        let pts = Duration::from_nanos(ts.max(0) as u64 * 100);

        if Some(index) == self.video {
            if pixels_from.is_some_and(|t| pts < t) {
                return Ok(Packet::VideoSkipped { pts });
            }
            let Some(image) = self.video_pixels(&sample, pool)? else { return Ok(Packet::Idle) };
            Ok(Packet::Video { image, pts })
        } else if Some(index) == self.audio {
            let Some(info) = self.audio_info else { return Ok(Packet::Idle) };
            let samples = pcm_to_f32(&sample)?;
            Ok(Packet::Audio { samples, channels: info.channels as usize, pts })
        } else {
            Ok(Packet::Idle)
        }
    }

    fn video_pixels(&mut self, sample: &IMFSample, pool: &FramePool) -> Result<Option<PreviewImage>> {
        let Some(info) = self.video_info else { return Ok(None) };
        if let Some(gpu) = self.gpu.as_mut() {
            // Try direct zero-copy DXVA texture first (`IMFDXGIBuffer`).
            if let Ok(buf) = unsafe { sample.GetBufferByIndex(0) }
                && let Ok(dxgi) = buf.cast::<IMFDXGIBuffer>()
            {
                let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
                unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw) }
                    .context("IMFDXGIBuffer::GetResource")?;
                if raw.is_null() {
                    bail!("IMFDXGIBuffer::GetResource returned null");
                }
                let tex = unsafe { ID3D11Texture2D::from_raw(raw) };
                let sub = unsafe { dxgi.GetSubresourceIndex() }.context("IMFDXGIBuffer::GetSubresourceIndex")?;
                let img = gpu.nv12_to_rgba(
                    &tex,
                    sub,
                    self.crop_rect,
                    (info.width, info.height),
                    self.bt709,
                    pool.take(),
                )?;
                return Ok(Some(img));
            }
            // Software decoder fallback inside MF: upload CPU NV12 to GPU VideoProcessor.
            let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            unsafe { buffer.Lock(&mut ptr, None, Some(&mut len)) }?;
            let src = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
            let img = gpu.nv12_cpu_to_rgba(
                src,
                self.stride.unsigned_abs() as usize,
                self.coded_size,
                self.crop_rect,
                (info.width, info.height),
                self.bt709,
                pool.take(),
            );
            unsafe {
                let _ = buffer.Unlock();
            }
            return Ok(Some(img?));
        }

        // SAFETY: the buffer is locked and unlocked around one bounded read.
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let src = std::slice::from_raw_parts(ptr, len as usize);
            // Straight from the locked buffer into a pooled RGBA image: the
            // reader already negotiated the size it wants, so there is nothing
            // to resample and no reason to stage the frame in a `RawFrame`
            // first. RGB32 is BGRA in memory, hence the red/blue swap.
            let image = rows_to_rgba_into(src, info.width, info.height, self.stride, true, pool.take());
            let _ = buffer.Unlock();
            Ok(image)
        }
    }

    /// Jumps to `to`. Media Foundation lands on the keyframe at or before the
    /// target, so the caller still has to decode forward to reach it.
    pub fn seek(&mut self, to: Duration) -> Result<()> {
        let pv = propvariant_i64((to.as_nanos() / 100) as i64);
        // SAFETY: `pv` is a VT_I8 variant that outlives the call and owns nothing.
        unsafe {
            self.reader.Flush(MF_SOURCE_READER_ALL_STREAMS.0 as u32).ok();
            self.reader.SetCurrentPosition(&TIME_FORMAT_NULL, &pv)?;
        }
        self.video_eos = self.video.is_none();
        self.audio_eos = self.audio.is_none();
        Ok(())
    }
}

/// Reads a 16-bit PCM sample into interleaved `f32`, the same scaling
/// [`crate::audio::capture`] applies in the opposite direction.
fn pcm_to_f32(sample: &IMFSample) -> Result<Vec<f32>> {
    // SAFETY: the buffer is locked and unlocked around one bounded read.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let bytes = std::slice::from_raw_parts(ptr, len as usize);
        let out = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0)
            .collect();
        let _ = buffer.Unlock();
        Ok(out)
    }
}

/// Output size for a source of `w`×`h`, shrunk to `max_side` on its longer
/// side. `None` when the source already fits and needs no scaling.
pub fn capped_size(w: u32, h: u32, max_side: u32) -> Option<(u32, u32)> {
    let max_side = max_side.max(2);
    let long = w.max(h);
    if w == 0 || h == 0 || long <= max_side {
        return None;
    }
    let scale = long as f64 / max_side as f64;
    // Even dimensions keep every chroma-subsampled decoder happy.
    let cw = (((w as f64 / scale).round() as u32).max(2)) & !1;
    let ch = (((h as f64 / scale).round() as u32).max(2)) & !1;
    Some((cw, ch))
}

#[cfg(test)]
mod tests {
    use super::super::{MAX_DECODE_SIDE, MAX_PLAYBACK_SIDE};
    use super::*;

    #[test]
    fn capped_size_only_shrinks_oversized_sources() {
        // At the snapshot ceiling, 1080p and 4K already fit.
        assert_eq!(capped_size(1920, 1080, MAX_DECODE_SIDE), None);
        assert_eq!(capped_size(3840, 2160, MAX_DECODE_SIDE), None);
        assert_eq!(capped_size(0, 0, MAX_DECODE_SIDE), None);

        // 8K halves, staying on even dimensions.
        assert_eq!(capped_size(7680, 4320, MAX_DECODE_SIDE), Some((3840, 2160)));
        let (w, h) = capped_size(5000, 3000, MAX_DECODE_SIDE).unwrap();
        assert_eq!(w, MAX_DECODE_SIDE);
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
        // Aspect ratio survives the round trip.
        assert!(((w as f64 / h as f64) - (5000.0 / 3000.0)).abs() < 0.01);

        // Portrait is capped on its own long side.
        assert_eq!(capped_size(2160, 7680, MAX_DECODE_SIDE), Some((1080, 3840)));
    }

    /// What playback actually asks for: 1440p and 4K come down to the viewer's
    /// own size instead of being carried at full resolution, and 1080p and
    /// below are left alone.
    #[test]
    fn playback_caps_everything_above_its_own_side() {

        assert_eq!(capped_size(3840, 2160, MAX_PLAYBACK_SIDE), Some((1920, 1080)));
        assert_eq!(capped_size(2560, 1440, MAX_PLAYBACK_SIDE), Some((1920, 1080)));
        assert_eq!(capped_size(1920, 1080, MAX_PLAYBACK_SIDE), None);
        assert_eq!(capped_size(1280, 720, MAX_PLAYBACK_SIDE), None);
        // A portrait phone clip is capped on its height, not its width.
        assert_eq!(capped_size(1080, 3840, MAX_PLAYBACK_SIDE), Some((540, 1920)));
    }

    #[test]
    fn gpu_reader_decodes_and_seeks_h264_mp4() {
        use crate::mux::{Muxer, VideoCodecConfig, VideoTrackConfig};
        use crate::settings::{Container, Profiles, RateControl, VideoCodec};
        use crate::video::{Converter, EncoderRequest, FrameInput, H264Encoder, PixelFormat, RawFrame, VideoEncoder};

        let (w, h) = (256u32, 256u32);
        let req = EncoderRequest {
            codec: VideoCodec::OpenH264,
            width: w,
            height: h,
            fps: 60,
            rate_control: RateControl::ConstantBitrate { kbps: 1500 },
            target_bitrate_bps: 1_500_000,
            keyframe_interval_frames: 30,
            profiles: Profiles::default(),
        };
        let mut enc = H264Encoder::new(&req).unwrap();
        let mut conv = Converter::new(w, h, enc.input_layout()).unwrap();

        let path = std::env::temp_dir().join(format!("openclip_decode_test_{}.mp4", std::process::id()));
        let mut mux = Muxer::create(
            Container::Mp4,
            &path,
            VideoTrackConfig { width: w, height: h, fps: 60.0, codec: VideoCodecConfig::H264 },
            None,
        )
        .unwrap();

        // Solid red BGRA frame repeated for 10 frames.
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for px in bgra.as_chunks_mut::<4>().0 {
            *px = [0, 0, 255, 255];
        }
        let raw = RawFrame {
            data: bgra,
            width: w,
            height: h,
            stride: w * 4,
            format: PixelFormat::Bgra,
            pts: Duration::ZERO,
            mouse: None,
        };
        conv.convert(&raw).unwrap();
        let mut packets = Vec::new();
        for i in 0..10u64 {
            let pts = Duration::from_millis(i * 16);
            match conv.frame() {
                FrameInput::I420 { y, u, v, strides, dims } => {
                    packets.extend(enc.encode(FrameInput::I420 { y, u, v, strides, dims }, pts).unwrap());
                }
                FrameInput::Nv12 { y, uv, strides, dims } => {
                    packets.extend(enc.encode(FrameInput::Nv12 { y, uv, strides, dims }, pts).unwrap());
                }
            }
        }
        packets.extend(enc.flush().unwrap());
        let p = enc.codec_params().expect("expected H264 codec params");
        mux.set_codec_params(p);
        assert!(!packets.is_empty());
        for pkt in packets {
            mux.push_video(&pkt.data, pkt.pts, pkt.keyframe).unwrap();
        }
        mux.finish_video(Duration::from_millis(160)).unwrap();
        mux.finalize().unwrap();

        let _com = crate::video::mf::ComGuard::new();
        let mut reader = Reader::open(&path, 128).unwrap();
        assert!(reader.gpu.is_some(), "expected GPU DXVA playback processor on Windows");
        let info = reader.video_info.unwrap();
        assert_eq!((info.width, info.height), (128, 128));

        let pool = FramePool::new(2);
        let mut decoded = 0u32;
        for _ in 0..32 {
            match reader.read(None, &pool, false).unwrap() {
                Packet::Video { image, .. } => {
                    assert_eq!((image.width, image.height), (128, 128));
                    let center = &image.rgba[(64 * 128 + 64) * 4..][..4];
                    assert!(center[0] > 210 && center[1] < 40 && center[2] < 40 && center[3] == 255, "center={center:?}");
                    pool.recycle(image.rgba);
                    decoded += 1;
                }
                Packet::End => break,
                _ => {}
            }
        }
        assert!(decoded >= 5, "decoded only {decoded} frames");
        reader.seek(Duration::from_millis(48)).unwrap();
        let mut landed = false;
        for _ in 0..32 {
            if let Packet::Video { image, .. } = reader.read(Some(Duration::from_millis(32)), &pool, false).unwrap() {
                assert_eq!((image.width, image.height), (128, 128));
                pool.recycle(image.rgba);
                landed = true;
                break;
            }
        }
        assert!(landed, "expected frame after seek");
        drop(reader);
        std::fs::remove_file(&path).ok();
    }
}
