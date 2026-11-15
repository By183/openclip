//! H.264 / HEVC encoding through a Media Foundation transform (hardware or
//! Microsoft's software encoder), fed either with GPU `ID3D11Texture2D`
//! (`DXGI_FORMAT_NV12`) surfaces via `IMFDXGIDeviceManager` (zero CPU readback)
//! or with NV12 frames from system memory.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFDXGIDeviceManager, IMFMediaType, IMFSample, MFCreateDXGISurfaceBuffer, MFCreateMediaType,
    MFCreateSample, MFMediaType_Video, MFNominalRange_16_235, MFSampleExtension_CleanPoint,
    MFT_MESSAGE_SET_D3D_MANAGER, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransferMatrix_BT709,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonQuality, CODECAPI_AVEncCommonQualityVsSpeed,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, MF_LOW_LATENCY, MF_MT_ALL_SAMPLES_INDEPENDENT,
    MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_SA_D3D11_AWARE,
    eAVEncCommonRateControlMode_CBR, eAVEncCommonRateControlMode_Quality, eAVEncH264VProfile_Base,
    eAVEncH264VProfile_High, eAVEncH264VProfile_Main, eAVEncH265VProfile_Main_420_8,
};

use super::transform::{make_sample_with, sample_bytes, MftSession};
use super::{activate_for, startup, ComGuard};
use crate::mux::{avc, hevc};
use crate::settings::{H264Profile, HevcProfile, RateControl};
use crate::video::d3d11_vp::{create_dxgi_device_manager, try_set_gpu_priority};
use crate::video::encoder::{
    CodecParams, EncodeStageTiming, EncodedFrame, EncoderInfo, EncoderRequest, FrameInput, InputLayout,
    VideoEncoder,
};

pub struct MfVideoEncoder {
    session: MftSession,
    dxgi_manager: Option<IMFDXGIDeviceManager>,
    params: Option<CodecParams>,
    /// Annex-B parameter sets from `MF_MT_MPEG_SEQUENCE_HEADER`, prepended to
    /// the first keyframe when the encoder does not emit them in-band.
    sequence_header: Option<Vec<u8>>,
    hevc: bool,
    frame_duration: i64,
    dims: (u32, u32),
    frames_in: u64,
    frames_out: u64,
    last_timing: EncodeStageTiming,
    description: String,
    /// Must be dropped last (declaration order) so COM objects go first.
    _com: ComGuard,
}

fn pack_u64(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn set_bt709_colorspace(t: &IMFMediaType) {
    unsafe {
        let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
        let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
        let _ = t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
        let _ = t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
    }
}

impl MfVideoEncoder {
    pub fn new(info: &EncoderInfo, req: &EncoderRequest) -> Result<Self> {
        Self::new_with_d3d(info, req, None)
    }

    pub fn new_with_d3d(info: &EncoderInfo, req: &EncoderRequest, d3d_device: Option<&ID3D11Device>) -> Result<Self> {
        if let Some(dev) = d3d_device {
            match Self::try_new(info, req, Some(dev)) {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    log::warn!("{} D3D11 zero-readback init failed ({e:#}); retrying CPU NV12 path", info.label);
                }
            }
        }
        Self::try_new(info, req, None)
    }

    fn try_new(info: &EncoderInfo, req: &EncoderRequest, d3d_device: Option<&ID3D11Device>) -> Result<Self> {
        let com = ComGuard::new();
        startup()?;
        let hevc = info.hevc;
        let activate = activate_for(info)?;
        let mut session = MftSession::from_activate(&activate)?;
        let fps = req.fps.max(1);
        let bitrate = req.target_bitrate_bps.max(200_000);

        let mut dxgi_manager = None;
        if let Some(dev) = d3d_device {
            let d3d11_aware = unsafe { session.transform().GetAttributes() }
                .and_then(|a| unsafe { a.GetUINT32(&MF_SA_D3D11_AWARE) })
                .unwrap_or(0)
                != 0;
            if d3d11_aware {
                let (manager, _reset_token) = create_dxgi_device_manager(dev)?;
                match try_set_gpu_priority(dev, 1) {
                    Ok(p) => log::info!("DXGI GPU thread priority set to {p} for {}", info.label),
                    Err((prev, e)) => {
                        log::info!("DXGI GPU thread priority unchanged (current={prev:?}): {e}");
                    }
                }
                if let Ok(attrs) = unsafe { session.transform().GetAttributes() } {
                    let _ = unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) };
                }
                unsafe {
                    session
                        .transform()
                        .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                }
                .with_context(|| format!("{} rejected MFT_MESSAGE_SET_D3D_MANAGER", info.label))?;
                dxgi_manager = Some(manager);
            } else {
                log::info!("{} is not MF_SA_D3D11_AWARE; using CPU NV12 path", info.label);
            }
        }

        let mut used_quality_mode = false;
        let apply_codec_api = |session: &MftSession, note_quality: &mut bool| {
            // Rate control first: some transforms only accept it before the types are set.
            match req.rate_control {
                RateControl::Quality(q) => {
                    if session.set_u32(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_Quality.0 as u32)
                        && session.set_u32(&CODECAPI_AVEncCommonQuality, q.clamp(1, 100) as u32)
                    {
                        *note_quality = true;
                    } else {
                        session.set_u32(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32);
                    }
                }
                RateControl::ConstantBitrate { .. } => {
                    session.set_u32(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32);
                }
            }
            session.set_u32(&CODECAPI_AVEncCommonMeanBitRate, bitrate);
            session.set_u32(&CODECAPI_AVEncMPVGOPSize, req.keyframe_interval_frames.max(1));
            // No B-frames: the muxers assume presentation order == decode order.
            session.set_u32(&CODECAPI_AVEncMPVDefaultBPictureCount, 0);
            session.set_bool(&CODECAPI_AVLowLatencyMode, true);
            // Bias hardware encoders toward encode speed (0) to keep 120+ fps slot times low.
            session.set_u32(&CODECAPI_AVEncCommonQualityVsSpeed, 0);
        };
        apply_codec_api(&session, &mut used_quality_mode);

        // Output type (always before the input type for encoders).
        let profile = if hevc {
            match req.profiles.hevc {
                HevcProfile::Auto => None,
                HevcProfile::Main => Some(eAVEncH265VProfile_Main_420_8.0 as u32),
            }
        } else {
            match req.profiles.h264 {
                H264Profile::Auto => None,
                H264Profile::Baseline => Some(eAVEncH264VProfile_Base.0 as u32),
                H264Profile::Main => Some(eAVEncH264VProfile_Main.0 as u32),
                H264Profile::High => Some(eAVEncH264VProfile_High.0 as u32),
            }
        };
        let out_type = output_media_type(hevc, req.width, req.height, fps, bitrate, profile)?;
        set_bt709_colorspace(&out_type);
        session.set_output_type(&out_type).with_context(|| format!("{} rejected the output format", info.label))?;
        apply_codec_api(&session, &mut used_quality_mode);

        // Input type: NV12 at the same size and rate.
        let in_type = unsafe { MFCreateMediaType() }?;
        unsafe {
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(req.width, req.height))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
            in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            in_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, req.width)?;
            in_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        }
        set_bt709_colorspace(&in_type);
        if let Err(e) = session.set_input_type(&in_type) {
            // Some transforms are picky: take their own NV12 proposal and adjust it.
            let proposed = session.input_available_types().into_iter().find(|t| {
                unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.map(|g| g == MFVideoFormat_NV12).unwrap_or(false)
            });
            let Some(t) = proposed else {
                return Err(e.context(format!("{} does not accept NV12 input", info.label)));
            };
            unsafe {
                t.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(req.width, req.height))?;
                t.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
                t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            }
            set_bt709_colorspace(&t);
            session.set_input_type(&t).with_context(|| format!("{} rejected the NV12 input format", info.label))?;
        }
        // Bitrate-related properties again now that both types are known (ignored if fixed).
        session.set_u32(&CODECAPI_AVEncCommonMeanBitRate, bitrate);
        session.set_u32(&CODECAPI_AVEncMPVGOPSize, req.keyframe_interval_frames.max(1));
        session.set_u32(&CODECAPI_AVEncCommonQualityVsSpeed, 0);

        // Encoded frames are far smaller than a raw one; use that as the buffer bound.
        let min_out = (req.width * req.height * 3 / 2).max(1 << 20);
        session.start(min_out)?;
        let sequence_header = sequence_header(&session.output_type().ok());

        let rc = match req.rate_control {
            RateControl::Quality(q) if used_quality_mode => format!("quality {q}"),
            RateControl::Quality(q) => format!("quality {q} → {} kbps CBR", bitrate / 1000),
            RateControl::ConstantBitrate { .. } => format!("{} kbps CBR", bitrate / 1000),
        };
        let mode = match (session.is_async(), dxgi_manager.is_some()) {
            (true, true) => "async, D3D11 GPU",
            (true, false) => "async, CPU NV12",
            (false, true) => "sync, D3D11 GPU",
            (false, false) => "sync, CPU NV12",
        };
        let description = format!(
            "{} via Media Foundation ({mode}), {}×{} @ {} fps, {rc}, {} profile{}",
            info.label,
            req.width,
            req.height,
            fps,
            if hevc { req.profiles.hevc.label() } else { req.profiles.h264.label() },
            if session.has_codec_api() { "" } else { ", no codec API" }
        );
        log::info!("{description}");
        Ok(Self {
            session,
            dxgi_manager,
            params: None,
            sequence_header,
            hevc,
            frame_duration: 10_000_000 / fps as i64,
            dims: (req.width, req.height),
            frames_in: 0,
            frames_out: 0,
            last_timing: EncodeStageTiming::default(),
            description,
            _com: com,
        })
    }

    fn process_sample(&mut self, sample: &IMFSample, t0: std::time::Instant, t1: std::time::Instant) -> Result<Vec<EncodedFrame>> {
        self.frames_in += 1;
        let outputs = self.session.process(sample)?;
        let t2 = std::time::Instant::now();
        let out_count = outputs.len();
        let mut frames = Vec::with_capacity(out_count);
        for s in &outputs {
            if let Some(f) = self.convert_output(s)? {
                frames.push(f);
            }
        }
        self.session.recycle_output(outputs);
        let t3 = std::time::Instant::now();
        let sample_prep_us = (t1 - t0).as_micros() as u64;
        let (wait_us, input_us, pump_us) = self.session.last_timing;
        let convert_out_us = (t3 - t2).as_micros() as u64;
        let mft_out_us = pump_us + convert_out_us;
        self.last_timing = EncodeStageTiming {
            mft_in_us: sample_prep_us + input_us,
            mft_wait_us: wait_us + mft_out_us,
            mft_need_in_us: wait_us,
            mft_process_in_us: input_us,
            mft_out_us,
        };
        if t3 - t0 > Duration::from_millis(8) {
            log::debug!(
                "slow MF encode ({}): sample {:.1} ms, process {:.1} ms (need_in {:.1}, input {:.1}, out {:.1}), {} out",
                if self.dxgi_manager.is_some() { "D3D11" } else { "CPU" },
                sample_prep_us as f64 / 1e3,
                (t2 - t1).as_secs_f64() * 1e3,
                wait_us as f64 / 1e3,
                input_us as f64 / 1e3,
                mft_out_us as f64 / 1e3,
                out_count
            );
        }
        Ok(frames)
    }

    fn convert_output(&mut self, sample: &windows::Win32::Media::MediaFoundation::IMFSample) -> Result<Option<EncodedFrame>> {
        let mut data = sample_bytes(sample)?;
        if data.is_empty() {
            return Ok(None);
        }
        let clean = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.map(|v| v == 1).ok();
        let nals = avc::split_annexb(&data);
        let has_params = nals.iter().any(|n| {
            if self.hevc { hevc::is_parameter_set(hevc::nal_type(n)) } else { avc::is_parameter_set(avc::nal_type(n)) }
        });
        let idr = nals.iter().any(|n| {
            if self.hevc { hevc::is_irap(hevc::nal_type(n)) } else { avc::nal_type(n) == avc::NAL_IDR }
        });
        let keyframe = clean.unwrap_or(idr) || idr || has_params;
        if keyframe
            && !has_params
            && let Some(h) = &self.sequence_header
        {
            let mut with = h.clone();
            with.extend_from_slice(&data);
            data = with;
        }
        if self.params.is_none() {
            self.params = CodecParams::from_annexb(&data, self.hevc);
        }
        let time = unsafe { sample.GetSampleTime() }.unwrap_or(0).max(0);
        self.frames_out += 1;
        Ok(Some(EncodedFrame { data, keyframe, pts: Duration::from_nanos(time as u64 * 100) }))
    }
}

/// Builds the compressed output media type for an H.264 / HEVC encoder.
pub(crate) fn output_media_type(
    hevc: bool,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
    profile: Option<u32>,
) -> Result<IMFMediaType> {
    let subtype = if hevc { MFVideoFormat_HEVC } else { MFVideoFormat_H264 };
    let t = unsafe { MFCreateMediaType() }.context("MFCreateMediaType")?;
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps.max(1), 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        if let Some(p) = profile {
            t.SetUINT32(&MF_MT_MPEG2_PROFILE, p)?;
        }
    }
    Ok(t)
}

/// Annex-B parameter sets advertised by the output type, if any.
fn sequence_header(t: &Option<IMFMediaType>) -> Option<Vec<u8>> {
    let t = t.as_ref()?;
    let size = unsafe { t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }.ok()?;
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    unsafe { t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None) }.ok()?;
    Some(buf)
}

impl VideoEncoder for MfVideoEncoder {
    fn input_layout(&self) -> InputLayout {
        InputLayout::Nv12
    }

    fn is_hevc(&self) -> bool {
        self.hevc
    }

    fn encode(&mut self, frame: FrameInput<'_>, pts: Duration) -> Result<Vec<EncodedFrame>> {
        let FrameInput::Nv12 { y, uv, strides, dims } = frame else {
            bail!("Media Foundation encoder needs NV12 input");
        };
        if dims != self.dims {
            bail!("frame size {}x{} does not match encoder {}x{}", dims.0, dims.1, self.dims.0, self.dims.1);
        }
        let (w, h) = (dims.0 as usize, dims.1 as usize);
        let uv_rows = h.div_ceil(2);
        let time = (pts.as_nanos() / 100) as i64;
        let t0 = std::time::Instant::now();
        // Repack the planes straight into the media buffer (tightly packed NV12).
        let sample = make_sample_with(w * h + w * uv_rows, time, self.frame_duration, |dst| {
            let (y_dst, uv_dst) = dst.split_at_mut(w * h);
            // The converter allocates tightly packed planes, which is the common
            // case: copy each one whole instead of a row at a time.
            if strides.0 == w && strides.1 == w {
                y_dst.copy_from_slice(&y[..w * h]);
                uv_dst.copy_from_slice(&uv[..w * uv_rows]);
                return;
            }
            for (row, out) in y_dst.chunks_exact_mut(w).enumerate() {
                out.copy_from_slice(&y[row * strides.0..row * strides.0 + w]);
            }
            for (row, out) in uv_dst.chunks_exact_mut(w).enumerate() {
                out.copy_from_slice(&uv[row * strides.1..row * strides.1 + w]);
            }
        })?;
        let t1 = std::time::Instant::now();
        self.process_sample(&sample, t0, t1)
    }

    fn supports_d3d11(&self) -> bool {
        self.dxgi_manager.is_some()
    }

    fn encode_d3d11(&mut self, tex: &ID3D11Texture2D, pts: Duration) -> Result<Vec<EncodedFrame>> {
        if self.dxgi_manager.is_none() {
            bail!("encoder does not have an active D3D11 device manager");
        }
        let time = (pts.as_nanos() / 100) as i64;
        let t0 = std::time::Instant::now();
        let sample = unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)
                .context("MFCreateDXGISurfaceBuffer")?;
            let contig_len = buffer
                .cast::<IMF2DBuffer>()
                .and_then(|b2d| b2d.GetContiguousLength())
                .unwrap_or(self.dims.0 * self.dims.1 * 3 / 2);
            buffer.SetCurrentLength(contig_len).context("DXGI buffer SetCurrentLength")?;
            let sample = MFCreateSample().context("MFCreateSample")?;
            sample.AddBuffer(&buffer).context("AddBuffer")?;
            sample.SetSampleTime(time).context("SetSampleTime")?;
            sample.SetSampleDuration(self.frame_duration).context("SetSampleDuration")?;
            sample
        };
        let t1 = std::time::Instant::now();
        self.process_sample(&sample, t0, t1)
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        let outputs = self.session.drain()?;
        let mut frames = Vec::with_capacity(outputs.len());
        for s in &outputs {
            if let Some(f) = self.convert_output(s)? {
                frames.push(f);
            }
        }
        self.session.recycle_output(outputs);
        log::info!("{}: {} frames in, {} out", self.description, self.frames_in, self.frames_out);
        Ok(frames)
    }

    fn codec_params(&self) -> Option<&CodecParams> {
        self.params.as_ref()
    }

    fn force_keyframe(&mut self) {
        self.session.set_u32(&CODECAPI_AVEncVideoForceKeyFrame, 1);
    }

    fn last_timing(&self) -> Option<EncodeStageTiming> {
        Some(self.last_timing)
    }

    fn describe(&self) -> String {
        self.description.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Profiles, VideoCodec};
    use crate::video::d3d11_vp::GpuCaptureProcessor;
    use crate::video::encoder::{available_encoders, create_video_encoder_with_d3d};
    use windows::Win32::Foundation::{HMODULE, RECT};
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

    #[test]
    fn mf_d3d11_encodes_gpu_nv12_surfaces_on_hardware_encoder() {
        let Some(hw) = available_encoders().into_iter().find(|e| e.hardware) else {
            eprintln!("no hardware MF encoder enumerated; skipping D3D11 MFT encode test");
            return;
        };
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut dev = None;
        let mut ctx = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            )
            .expect("D3D11 hardware device");
        }
        let dev = dev.unwrap();
        let ctx = ctx.unwrap();
        let (w, h) = (256u32, 256u32);
        let bgra = vec![0x40u8; (w * h * 4) as usize];
        let src_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: bgra.as_ptr().cast(), SysMemPitch: w * 4, SysMemSlicePitch: 0 };
        let mut src_tex = None;
        unsafe { dev.CreateTexture2D(&src_desc, Some(&init), Some(&mut src_tex)) }.unwrap();
        let src_tex = src_tex.unwrap();

        let mut vp = GpuCaptureProcessor::new(dev.clone(), ctx, (w, h), (w, h), (w, h)).unwrap();
        let req = EncoderRequest {
            codec: VideoCodec::Mf { hevc: hw.hevc, clsid: hw.clsid.clone() },
            width: w,
            height: h,
            fps: 60,
            rate_control: RateControl::ConstantBitrate { kbps: 4000 },
            target_bitrate_bps: 4_000_000,
            keyframe_interval_frames: 60,
            profiles: Profiles::default(),
        };
        let (mut enc, note) = create_video_encoder_with_d3d(&req, Some(&dev)).unwrap();
        assert!(note.is_none(), "unexpected fallback note: {note:?}");
        eprintln!("Encoder description: {}", enc.describe());
        assert!(enc.supports_d3d11(), "hardware encoder {} should accept D3D11 NV12 surfaces", hw.label);

        let mut out_frames = Vec::new();
        for i in 0..15u64 {
            let slot = vp.convert(&src_tex, (w, h), RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }).unwrap();
            let pts = Duration::from_nanos(i * 1_000_000_000 / 60);
            out_frames.extend(enc.encode_d3d11(&slot.texture, pts).unwrap());
        }
        out_frames.extend(enc.flush().unwrap());
        assert!(!out_frames.is_empty(), "expected encoded packets from D3D11 hardware encoder");
        assert!(out_frames.iter().any(|f| f.keyframe), "expected keyframe");
        assert!(enc.codec_params().is_some(), "expected SPS/PPS codec params");
        eprintln!("Hardware D3D11 MFT test produced {} encoded frames", out_frames.len());
    }

    #[test]
    fn mf_stage_timings_are_populated_for_d3d11_and_cpu() {
        let Some(hw) = available_encoders().into_iter().find(|e| e.hardware) else {
            eprintln!("no hardware MF encoder enumerated; skipping stage timing test");
            return;
        };
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut dev = None;
        let mut ctx = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            )
            .expect("D3D11 hardware device");
        }
        let dev = dev.unwrap();
        let ctx = ctx.unwrap();
        let (w, h) = (1280u32, 720u32);
        let bgra = vec![0x55u8; (w * h * 4) as usize];
        let src_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: bgra.as_ptr().cast(), SysMemPitch: w * 4, SysMemSlicePitch: 0 };
        let mut src_tex = None;
        unsafe { dev.CreateTexture2D(&src_desc, Some(&init), Some(&mut src_tex)) }.unwrap();
        let src_tex = src_tex.unwrap();

        let req = EncoderRequest {
            codec: VideoCodec::Mf { hevc: hw.hevc, clsid: hw.clsid.clone() },
            width: w,
            height: h,
            fps: 120,
            rate_control: RateControl::ConstantBitrate { kbps: 20_000 },
            target_bitrate_bps: 20_000_000,
            keyframe_interval_frames: 240,
            profiles: Profiles::default(),
        };

        // 1) D3D11 GPU path with convert_timed + wait_complete + encode_d3d11.
        let mut vp = GpuCaptureProcessor::new(dev.clone(), ctx.clone(), (w, h), (w, h), (w, h)).unwrap();
        let (mut gpu_enc, note) = create_video_encoder_with_d3d(&req, Some(&dev)).unwrap();
        assert!(note.is_none(), "unexpected fallback note: {note:?}");
        assert!(gpu_enc.supports_d3d11());
        let mut gpu_out = 0usize;
        for i in 0..10u64 {
            let (slot, _vp_us) = vp.convert_timed(&src_tex, (w, h), RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }, None).unwrap();
            let _gw_us = slot.wait_complete(&ctx);
            let pts = Duration::from_nanos(i * 1_000_000_000 / 120);
            gpu_out += gpu_enc.encode_d3d11(&slot.texture, pts).unwrap().len();
        }
        gpu_out += gpu_enc.flush().unwrap().len();
        assert!(gpu_out > 0);
        assert!(gpu_enc.last_timing().is_some());

        // 2) CPU NV12 fallback path without D3D device.
        let (mut cpu_enc, _) = create_video_encoder_with_d3d(&req, None).unwrap();
        assert!(!cpu_enc.supports_d3d11());
        let y = vec![0x80u8; (w * h) as usize];
        let uv = vec![0x80u8; (w * h / 2) as usize];
        let mut cpu_out = 0usize;
        for i in 0..10u64 {
            let pts = Duration::from_nanos(i * 1_000_000_000 / 120);
            cpu_out += cpu_enc
                .encode(FrameInput::Nv12 { y: &y, uv: &uv, strides: (w as usize, w as usize), dims: (w, h) }, pts)
                .unwrap()
                .len();
        }
        cpu_out += cpu_enc.flush().unwrap().len();
        assert!(cpu_out > 0);
        assert!(cpu_enc.last_timing().is_some());
    }
}
