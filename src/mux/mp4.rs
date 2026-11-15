//! Streaming progressive ISO BMFF fragmented MP4 (`fMP4`) writer.
//!
//! Layout:
//!   `ftyp` | `moov` (with `mvex` and empty `stbl` sample tables) |
//!   [ `moof` + `mdat` ]* | (optional `mfra` at finalize).
//!
//! Video is `avc1` (H.264) or `hvc1` (HEVC) with length-prefixed samples;
//! audio is `mp4a` with MPEG-1 Layer III (`objectTypeIndication` 0x6B) or
//! AAC (0x40) `esds`.
//!
//! Because `avcC` / `hvcC` requires parameter sets (SPS/PPS, VPS), samples
//! are buffered in memory until the first keyframe (or `set_codec_params`),
//! at which point the initialization segment (`ftyp` + `moov`) is flushed
//! immediately to disk.
//!
//! Subsequent samples are grouped into self-contained fragments (`moof` + `mdat`)
//! and flushed to disk on keyframes (>= 1.5 s) or emergency timeouts (>= 3.0 s).
//! Each fragment uses `default-base-is-moof` (flag `0x020000`) and 64-bit `tfdt`
//! decode timestamps, allowing valid playback even if the process crashes or loses
//! power without calling [`Mp4Writer::finalize`].

use std::io::{Seek, Write};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::avc;
use super::boxes::{mp4_now, unity_matrix, Buf};
use super::hevc;
use crate::audio::encoder::AudioCodecConfig;
use crate::video::encoder::CodecParams;

/// Timescale of the video track (ticks per second).
pub const VIDEO_TIMESCALE: u32 = 90_000;
/// Timescale of the movie header.
const MOVIE_TIMESCALE: u32 = 1_000;
/// Target minimum duration of a fragment before splitting on a keyframe.
const FRAGMENT_TARGET_DURATION: Duration = Duration::from_millis(1500);
/// Maximum duration of a fragment before forcing an emergency flush.
const FRAGMENT_MAX_DURATION: Duration = Duration::from_millis(3000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodecConfig {
    H264,
    Hevc,
}

#[derive(Debug, Clone)]
pub struct VideoTrackConfig {
    pub width: u32,
    pub height: u32,
    /// Used for default fallback sample duration when calculating frame pacing.
    pub fps: f64,
    pub codec: VideoCodecConfig,
}

#[derive(Debug, Clone)]
pub struct AudioTrackConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub bitrate_bps: u32,
    /// PCM samples per encoded frame (1152 for MPEG-1 Layer III, 1024 for AAC).
    pub samples_per_frame: u32,
    pub codec: AudioCodecConfig,
}

#[derive(Debug, Clone)]
struct VideoSample {
    data: Vec<u8>,
    pts_ticks: u64,
    keyframe: bool,
}

#[derive(Debug, Clone)]
struct AudioSample {
    data: Vec<u8>,
    pts_ticks: u64,
}

#[derive(Debug, Clone, Copy)]
struct SeekEntry {
    time: u64,
    moof_offset: u64,
}

pub struct Mp4Writer<W: Write + Seek> {
    out: W,
    pos: u64,
    video: Option<VideoTrackConfig>,
    video_end_pts: Option<Duration>,
    audio: Vec<AudioTrackConfig>,
    params: Option<CodecParams>,
    header_written: bool,
    sequence_number: u32,
    first_video_pts: Option<Duration>,
    last_video_pts_ticks: Option<u64>,
    fragment_start_video_pts: Option<Duration>,
    fragment_start_audio_pts: Option<u64>,
    buffered_video: Vec<VideoSample>,
    buffered_audio: Vec<Vec<AudioSample>>,
    video_samples_total: usize,
    audio_samples_total: Vec<usize>,
    seek_entries: Vec<SeekEntry>,
    scratch: Vec<u8>,
    finalized: bool,
}

impl<W: Write + Seek> Mp4Writer<W> {
    /// Starts a new file with the given tracks. At least one track is required.
    pub fn new(
        out: W,
        video: Option<VideoTrackConfig>,
        audio: Option<AudioTrackConfig>,
    ) -> Result<Self> {
        Self::with_audio_tracks(out, video, audio.into_iter().collect())
    }

    /// Starts a new file with an optional video track and zero or more audio tracks.
    pub fn with_audio_tracks(
        out: W,
        video: Option<VideoTrackConfig>,
        audio_tracks: Vec<AudioTrackConfig>,
    ) -> Result<Self> {
        if video.is_none() && audio_tracks.is_empty() {
            bail!("MP4 needs at least one track");
        }
        for a in &audio_tracks {
            if matches!(a.codec, AudioCodecConfig::Pcm { .. }) {
                bail!("PCM audio is not supported in MP4; use AVI");
            }
        }
        let audio_count = audio_tracks.len();
        let mut writer = Self {
            out,
            pos: 0,
            video,
            video_end_pts: None,
            audio: audio_tracks,
            params: None,
            header_written: false,
            sequence_number: 1,
            first_video_pts: None,
            last_video_pts_ticks: None,
            fragment_start_video_pts: None,
            fragment_start_audio_pts: None,
            buffered_video: Vec::new(),
            buffered_audio: vec![Vec::new(); audio_count],
            video_samples_total: 0,
            audio_samples_total: vec![0; audio_count],
            seek_entries: Vec::new(),
            scratch: Vec::new(),
            finalized: false,
        };

        // If audio-only, we already have everything needed to write the initialization header.
        if writer.video.is_none() {
            writer.write_header()?;
        }

        Ok(writer)
    }

    /// Extends the final video sample's presentation duration up to `end_pts`
    /// so variable-frame-rate recordings that end on a static screen match the
    /// full wall-clock/audio duration without writing duplicate frames.
    pub fn finish_video(&mut self, end_pts: Duration) {
        self.video_end_pts = Some(end_pts);
    }

    pub fn has_video(&self) -> bool {
        self.video.is_some()
    }

    pub fn has_audio(&self) -> bool {
        !self.audio.is_empty()
    }

    pub fn audio_track_count(&self) -> usize {
        self.audio.len()
    }

    /// Registers the codec parameter sets (start-code-free NALs). When none is
    /// registered, `push_video` harvests them from the first keyframe.
    pub fn set_codec_params(&mut self, params: &CodecParams) {
        if self.params.is_none() {
            self.params = Some(params.clone());
            if !self.header_written {
                let _ = self.write_header();
            }
        }
    }

    /// Appends one video access unit in Annex-B form. Parameter sets and
    /// access-unit delimiters are stripped; the rest is length-prefixed.
    pub fn push_video(&mut self, annexb: &[u8], pts: Duration, keyframe: bool) -> Result<()> {
        let cfg = self.video.as_ref().context("no video track")?;
        let is_hevc = cfg.codec == VideoCodecConfig::Hevc;
        if self.params.is_none() && keyframe {
            self.params = CodecParams::from_annexb(annexb, is_hevc);
            if self.params.is_some() && !self.header_written {
                self.write_header()?;
            }
        }

        self.scratch.clear();
        for nal in avc::split_annexb(annexb) {
            let skip = if is_hevc {
                let t = hevc::nal_type(nal);
                hevc::is_parameter_set(t) || t == hevc::NAL_AUD
            } else {
                let t = avc::nal_type(nal);
                avc::is_parameter_set(t) || t == avc::NAL_AUD
            };
            if !skip {
                avc::push_avcc(&mut self.scratch, nal);
            }
        }
        if self.scratch.is_empty() {
            return Ok(());
        }

        let first = *self.first_video_pts.get_or_insert(pts);
        let rel = pts.saturating_sub(first);
        let mut ticks = duration_to_ticks(rel, VIDEO_TIMESCALE);
        if let Some(last) = self.last_video_pts_ticks
            && ticks <= last
        {
            ticks = last + 1;
        }
        self.last_video_pts_ticks = Some(ticks);

        // Check if current fragment duration threshold reached
        let fragment_dur = if let Some(start) = self.fragment_start_video_pts {
            pts.saturating_sub(start)
        } else {
            Duration::ZERO
        };

        let should_flush = (!self.buffered_video.is_empty()
            && keyframe
            && fragment_dur >= FRAGMENT_TARGET_DURATION)
            || (!self.buffered_video.is_empty() && fragment_dur >= FRAGMENT_MAX_DURATION);

        if should_flush {
            self.flush_fragment(Some(ticks))?;
        }

        if self.fragment_start_video_pts.is_none() {
            self.fragment_start_video_pts = Some(pts);
        }

        self.buffered_video.push(VideoSample {
            data: self.scratch.clone(),
            pts_ticks: ticks,
            keyframe,
        });
        self.video_samples_total += 1;

        Ok(())
    }

    /// Appends one audio frame (MP3 frame / AAC access unit) to track 0. Audio samples are assumed contiguous.
    pub fn push_audio(&mut self, frame: &[u8]) -> Result<()> {
        self.push_audio_track(0, frame)
    }

    /// Appends one audio frame (MP3 frame / AAC access unit) to audio track `index`.
    pub fn push_audio_track(&mut self, index: usize, frame: &[u8]) -> Result<()> {
        let cfg = self.audio.get(index).with_context(|| format!("no audio track {index}"))?;
        let current_sample_count = self.audio_samples_total[index];
        let ticks = current_sample_count as u64 * cfg.samples_per_frame as u64;

        if self.buffered_audio[index].is_empty() && self.fragment_start_audio_pts.is_none() {
            self.fragment_start_audio_pts = Some(ticks);
        }

        self.buffered_audio[index].push(AudioSample {
            data: frame.to_vec(),
            pts_ticks: ticks,
        });
        self.audio_samples_total[index] += 1;

        // For audio-only streams, flush on fragment target duration
        if self.video.is_none() && index == 0 {
            let samples = self.buffered_audio[0].len() as u64;
            let dur_ticks = samples * cfg.samples_per_frame as u64;
            let dur = Duration::from_secs_f64(dur_ticks as f64 / cfg.sample_rate as f64);
            if dur >= FRAGMENT_TARGET_DURATION {
                self.flush_fragment(None)?;
            }
        }

        Ok(())
    }

    fn write_header(&mut self) -> Result<()> {
        if self.header_written {
            return Ok(());
        }
        let is_hevc = self.video.as_ref().map(|v| v.codec == VideoCodecConfig::Hevc).unwrap_or(false);
        let mut ftyp = Buf::new();
        ftyp.atom(b"ftyp", |b| {
            b.fourcc(b"isom").u32(0x200);
            for brand in [b"isom", b"iso5", b"iso6", b"mp41", if is_hevc { b"hvc1" } else { b"avc1" }] {
                b.fourcc(brand);
            }
        });

        let moov = self.build_init_moov()?;

        self.out.write_all(&ftyp.0)?;
        self.out.write_all(&moov)?;
        self.pos += (ftyp.0.len() + moov.len()) as u64;
        self.out.flush()?;
        self.header_written = true;
        Ok(())
    }

    fn build_init_moov(&self) -> Result<Vec<u8>> {
        let now = mp4_now();
        let mut tracks: Vec<Vec<u8>> = Vec::new();
        let mut track_ids: Vec<u32> = Vec::new();
        let mut next_track_id = 1u32;

        if let Some(cfg) = &self.video {
            let params = self.params.as_ref().context("no parameter sets recorded for video track")?;
            let stsd = match (cfg.codec, params) {
                (VideoCodecConfig::H264, CodecParams::H264 { sps, pps }) => build_avc1(cfg, sps, pps),
                (VideoCodecConfig::Hevc, CodecParams::Hevc { vps, sps, pps }) => build_hvc1(cfg, vps, sps, pps),
                _ => bail!("codec parameter sets do not match the video track codec"),
            };
            let trak = build_init_trak(&InitTrakParams {
                track_id: next_track_id,
                now,
                timescale: VIDEO_TIMESCALE,
                handler: b"vide",
                handler_name: "VideoHandler",
                dims: Some((cfg.width, cfg.height)),
                stsd_entry: stsd,
                is_video: true,
            });
            tracks.push(trak);
            track_ids.push(next_track_id);
            next_track_id += 1;
        }

        for cfg in &self.audio {
            let stsd = build_mp4a(cfg);
            let trak = build_init_trak(&InitTrakParams {
                track_id: next_track_id,
                now,
                timescale: cfg.sample_rate,
                handler: b"soun",
                handler_name: "SoundHandler",
                dims: None,
                stsd_entry: stsd,
                is_video: false,
            });
            tracks.push(trak);
            track_ids.push(next_track_id);
            next_track_id += 1;
        }

        if tracks.is_empty() {
            bail!("no tracks configured");
        }

        let mut moov = Buf::new();
        moov.atom(b"moov", |b| {
            b.full_atom(b"mvhd", 0, 0, |b| {
                b.u32(now as u32).u32(now as u32);
                b.u32(MOVIE_TIMESCALE).u32(0); // duration = 0 in progressive fMP4
                b.u32(0x0001_0000).u16(0x0100).zeros(10);
                unity_matrix(b);
                b.zeros(24);
                b.u32(next_track_id);
            });
            for trak in &tracks {
                b.bytes(trak);
            }
            b.atom(b"mvex", |b| {
                for &tid in &track_ids {
                    b.full_atom(b"trex", 0, 0, |b| {
                        b.u32(tid); // track_ID
                        b.u32(1);   // default_sample_description_index = 1
                        b.u32(0);   // default_sample_duration = 0
                        b.u32(0);   // default_sample_size = 0
                        b.u32(0);   // default_sample_flags = 0
                    });
                }
            });
        });

        Ok(moov.into_vec())
    }

    fn flush_fragment(&mut self, next_video_pts: Option<u64>) -> Result<()> {
        if !self.header_written {
            self.write_header()?;
        }

        let has_video = !self.buffered_video.is_empty();
        let has_audio = self.buffered_audio.iter().any(|a| !a.is_empty());
        if !has_video && !has_audio {
            return Ok(());
        }

        let moof_offset = self.pos;
        let mut moof = Buf::new();

        // (track_index_in_mdat_payload, patch_offset_in_moof)
        let mut patches: Vec<(usize, usize)> = Vec::new();
        let mut payload_lengths: Vec<usize> = Vec::new();

        let seq = self.sequence_number;
        self.sequence_number += 1;

        moof.atom(b"moof", |b| {
            b.full_atom(b"mfhd", 0, 0, |b| {
                b.u32(seq);
            });

            if has_video {
                let vid_track_id = 1u32;
                let base_time = self.buffered_video[0].pts_ticks;
                let sample_count = self.buffered_video.len() as u32;

                b.atom(b"traf", |b| {
                    // tfhd: flag 0x020000 = default-base-is-moof
                    b.full_atom(b"tfhd", 0, 0x020000, |b| {
                        b.u32(vid_track_id);
                    });
                    // tfdt: version 1, 64-bit baseMediaDecodeTime
                    b.full_atom(b"tfdt", 1, 0, |b| {
                        b.u64(base_time);
                    });
                    // trun: flags 0x000701 = data-offset | sample-duration | sample-size | sample-flags
                    b.full_atom(b"trun", 0, 0x000701, |b| {
                        b.u32(sample_count);
                        let patch_offset = b.0.len();
                        b.i32(0); // placeholder for data_offset
                        patches.push((payload_lengths.len(), patch_offset));

                        let n = self.buffered_video.len();
                        let fallback_delta = (VIDEO_TIMESCALE as f64 / self.video.as_ref().unwrap().fps.max(1.0)).round() as u32;

                        for i in 0..n {
                            let s = &self.buffered_video[i];
                            let dur = if i + 1 < n {
                                self.buffered_video[i + 1].pts_ticks.saturating_sub(s.pts_ticks)
                            } else if let Some(next_pts) = next_video_pts {
                                next_pts.saturating_sub(s.pts_ticks)
                            } else if let (Some(end_pts), Some(first)) = (self.video_end_pts, self.first_video_pts) {
                                let end_ticks = duration_to_ticks(end_pts.saturating_sub(first), VIDEO_TIMESCALE);
                                end_ticks.saturating_sub(s.pts_ticks).max(1)
                            } else {
                                fallback_delta as u64
                            };
                            let duration = dur.clamp(1, u32::MAX as u64) as u32;
                            let size = s.data.len() as u32;
                            // 0x0200_0000: I-frame / keyframe (depends on none)
                            // 0x0101_0000: P-frame / non-keyframe (depends on others, non-sync)
                            let flags = if s.keyframe { 0x0200_0000 } else { 0x0101_0000 };

                            b.u32(duration);
                            b.u32(size);
                            b.u32(flags);
                        }
                    });
                });

                let vid_bytes: usize = self.buffered_video.iter().map(|s| s.data.len()).sum();
                payload_lengths.push(vid_bytes);
            }

            for (idx, samples) in self.buffered_audio.iter().enumerate() {
                if samples.is_empty() {
                    continue;
                }
                let audio_track_id = (if self.video.is_some() { 2 } else { 1 }) + idx as u32;
                let base_time = samples[0].pts_ticks;
                let sample_count = samples.len() as u32;
                let sample_duration = self.audio[idx].samples_per_frame;

                b.atom(b"traf", |b| {
                    b.full_atom(b"tfhd", 0, 0x020000, |b| {
                        b.u32(audio_track_id);
                    });
                    b.full_atom(b"tfdt", 1, 0, |b| {
                        b.u64(base_time);
                    });
                    // trun: flags 0x000301 = data-offset | sample-duration | sample-size
                    b.full_atom(b"trun", 0, 0x000301, |b| {
                        b.u32(sample_count);
                        let patch_offset = b.0.len();
                        b.i32(0); // placeholder for data_offset
                        patches.push((payload_lengths.len(), patch_offset));

                        for s in samples {
                            b.u32(sample_duration);
                            b.u32(s.data.len() as u32);
                        }
                    });
                });

                let aud_bytes: usize = samples.iter().map(|s| s.data.len()).sum();
                payload_lengths.push(aud_bytes);
            }
        });

        // Patch each track's data_offset relative to start of moof
        let moof_len = moof.0.len() as u32;
        let mdat_hdr_len = 8u32;
        let mut track_offset = moof_len + mdat_hdr_len;

        for (track_idx, patch_offset) in patches {
            let offset_bytes = (track_offset as i32).to_be_bytes();
            moof.0[patch_offset..patch_offset + 4].copy_from_slice(&offset_bytes);
            track_offset += payload_lengths[track_idx] as u32;
        }

        let mdat_payload_len = track_offset - (moof_len + mdat_hdr_len);
        let mut mdat_hdr = Buf::new();
        mdat_hdr.u32(mdat_hdr_len + mdat_payload_len).fourcc(b"mdat");

        // Write fragment to disk
        self.out.write_all(&moof.0)?;
        self.out.write_all(&mdat_hdr.0)?;

        if has_video {
            for s in &self.buffered_video {
                self.out.write_all(&s.data)?;
            }
        }
        for samples in &self.buffered_audio {
            for s in samples {
                self.out.write_all(&s.data)?;
            }
        }

        self.pos += (moof.0.len() + mdat_hdr.0.len() + mdat_payload_len as usize) as u64;
        self.out.flush()?;

        // Record seek entry for keyframe in video track
        if has_video && let Some(key_sample) = self.buffered_video.iter().find(|s| s.keyframe) {
            self.seek_entries.push(SeekEntry {
                time: key_sample.pts_ticks,
                moof_offset,
            });
        }

        self.buffered_video.clear();
        for a in &mut self.buffered_audio {
            a.clear();
        }
        self.fragment_start_video_pts = None;
        self.fragment_start_audio_pts = None;

        Ok(())
    }

    /// Bytes written to the file so far.
    pub fn bytes_written(&self) -> u64 {
        self.pos
    }

    /// Number of video samples pushed so far.
    pub fn video_samples(&self) -> usize {
        self.video_samples_total
    }

    /// Number of audio samples (MP3 frames / AAC frames) pushed so far on track 0.
    pub fn audio_samples(&self) -> usize {
        self.audio_track_samples(0)
    }

    /// Number of audio samples pushed so far on audio track `index`.
    pub fn audio_track_samples(&self, index: usize) -> usize {
        self.audio_samples_total.get(index).copied().unwrap_or(0)
    }

    /// Writes remaining buffered samples, optional `mfra` index, and flushes the writer.
    pub fn finalize(mut self) -> Result<()> {
        if !self.header_written {
            if self.video.is_some() && self.params.is_none() {
                bail!("no samples were written");
            }
            self.write_header()?;
        }

        self.flush_fragment(None)?;

        // Build mfra random access box if we have indexed video fragments
        if !self.seek_entries.is_empty() {
            let mfra = self.build_mfra();
            self.out.write_all(&mfra)?;
            self.pos += mfra.len() as u64;
        }

        self.out.flush()?;
        self.finalized = true;
        Ok(())
    }

    fn build_mfra(&self) -> Vec<u8> {
        let mut b = Buf::new();
        b.atom(b"mfra", |b| {
            b.full_atom(b"tfra", 1, 0, |b| {
                b.u32(1); // track_id = 1 (video)
                b.u32(0); // lengths (sample_num, trun_num, traf_num all 0 = 1 byte each)
                b.u32(self.seek_entries.len() as u32);
                for e in &self.seek_entries {
                    b.u64(e.time);
                    b.u64(e.moof_offset);
                    b.u8(1); // traf_number
                    b.u8(1); // trun_number
                    b.u8(1); // sample_delta / sample_number
                }
            });
            let mfra_len = b.0.len() + 16; // 16 bytes for mfro box
            b.full_atom(b"mfro", 0, 0, |b| {
                b.u32(mfra_len as u32);
            });
        });
        b.into_vec()
    }
}

impl<W: Write + Seek> Drop for Mp4Writer<W> {
    fn drop(&mut self) {
        if !self.finalized {
            log::info!("Mp4Writer dropped without finalize(); fMP4 fragments on disk remain valid and playable");
        }
    }
}

fn duration_to_ticks(d: Duration, timescale: u32) -> u64 {
    (d.as_nanos() * timescale as u128 / 1_000_000_000) as u64
}

struct InitTrakParams<'a> {
    track_id: u32,
    now: u64,
    timescale: u32,
    handler: &'a [u8; 4],
    handler_name: &'a str,
    dims: Option<(u32, u32)>,
    stsd_entry: Vec<u8>,
    is_video: bool,
}

fn build_init_trak(p: &InitTrakParams<'_>) -> Vec<u8> {
    let mut b = Buf::new();
    b.atom(b"trak", |b| {
        b.full_atom(b"tkhd", 0, 0x3, |b| {
            b.u32(p.now as u32).u32(p.now as u32).u32(p.track_id).u32(0);
            b.u32(0); // duration = 0 in fMP4 init
            b.zeros(8);
            b.u16(0).u16(0);
            b.u16(if p.is_video { 0 } else { 0x0100 }).u16(0);
            unity_matrix(b);
            let (w, h) = p.dims.unwrap_or((0, 0));
            b.u32(w << 16).u32(h << 16);
        });
        b.atom(b"mdia", |b| {
            b.full_atom(b"mdhd", 0, 0, |b| {
                b.u32(p.now as u32).u32(p.now as u32);
                b.u32(p.timescale).u32(0); // duration = 0 in fMP4 init
                b.u16(0x55C4).u16(0);      // language 'und'
            });
            b.full_atom(b"hdlr", 0, 0, |b| {
                b.u32(0).fourcc(p.handler).zeros(12);
                b.bytes(p.handler_name.as_bytes()).u8(0);
            });
            b.atom(b"minf", |b| {
                if p.is_video {
                    b.full_atom(b"vmhd", 0, 1, |b| {
                        b.u16(0).u16(0).u16(0).u16(0);
                    });
                } else {
                    b.full_atom(b"smhd", 0, 0, |b| {
                        b.u16(0).u16(0);
                    });
                }
                b.atom(b"dinf", |b| {
                    b.full_atom(b"dref", 0, 0, |b| {
                        b.u32(1);
                        b.full_atom(b"url ", 0, 1, |_| {});
                    });
                });
                b.atom(b"stbl", |b| {
                    b.full_atom(b"stsd", 0, 0, |b| {
                        b.u32(1).bytes(&p.stsd_entry);
                    });
                    // Empty sample tables for fMP4
                    b.full_atom(b"stts", 0, 0, |b| {
                        b.u32(0);
                    });
                    b.full_atom(b"stsc", 0, 0, |b| {
                        b.u32(0);
                    });
                    b.full_atom(b"stsz", 0, 0, |b| {
                        b.u32(0).u32(0);
                    });
                    b.full_atom(b"stco", 0, 0, |b| {
                        b.u32(0);
                    });
                });
            });
        });
    });
    b.into_vec()
}

/// The VisualSampleEntry fields shared by `avc1` and `hvc1`.
fn visual_sample_entry(b: &mut Buf, cfg: &VideoTrackConfig) {
    b.zeros(6).u16(1); // reserved, data_reference_index
    b.u16(0).u16(0).zeros(12); // pre_defined, reserved, pre_defined
    b.u16(cfg.width as u16).u16(cfg.height as u16);
    b.u32(0x0048_0000).u32(0x0048_0000); // 72 dpi
    b.u32(0);
    b.u16(1); // frame_count
    let name = b"openclip";
    b.u8(name.len() as u8).bytes(name).zeros(31 - name.len());
    b.u16(0x0018); // depth
    b.u16(0xFFFF); // pre_defined = -1
}

fn build_hvc1(cfg: &VideoTrackConfig, vps: &[u8], sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let info = hevc::parse_sps_profile(sps).unwrap_or_default();
    let mut b = Buf::new();
    b.atom(b"hvc1", |b| {
        visual_sample_entry(b, cfg);
        b.atom(b"hvcC", |b| {
            b.u8(1); // configurationVersion
            b.u8((info.profile_space << 6) | (info.tier << 5) | (info.profile_idc & 0x1F));
            b.u32(info.compat_flags);
            b.bytes(&info.constraint_flags);
            b.u8(info.level_idc);
            b.u16(0xF000); // min_spatial_segmentation_idc = 0
            b.u8(0xFC); // parallelismType = 0
            b.u8(0xFC | (info.chroma_format_idc & 3));
            b.u8(0xF8 | (info.bit_depth_luma.saturating_sub(8) & 7));
            b.u8(0xF8 | (info.bit_depth_chroma.saturating_sub(8) & 7));
            b.u16(0); // avgFrameRate
            // constantFrameRate 0, numTemporalLayers 1, temporalIdNested 0, lengthSizeMinusOne 3
            b.u8((1 << 3) | 3);
            b.u8(3); // numOfArrays
            for (t, nal) in [(hevc::NAL_VPS, vps), (hevc::NAL_SPS, sps), (hevc::NAL_PPS, pps)] {
                b.u8(0x80 | t); // array_completeness + NAL type
                b.u16(1);
                b.u16(nal.len() as u16).bytes(nal);
            }
        });
    });
    b.into_vec()
}

fn build_avc1(cfg: &VideoTrackConfig, sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let (profile, compat, level) = avc::sps_profile_info(sps).unwrap_or((66, 0xC0, 31));
    let mut b = Buf::new();
    b.atom(b"avc1", |b| {
        visual_sample_entry(b, cfg);
        b.atom(b"avcC", |b| {
            b.u8(1).u8(profile).u8(compat).u8(level);
            b.u8(0xFC | 3); // lengthSizeMinusOne = 3
            b.u8(0xE0 | 1);
            b.u16(sps.len() as u16).bytes(sps);
            b.u8(1);
            b.u16(pps.len() as u16).bytes(pps);
            if matches!(profile, 100 | 110 | 122 | 144) {
                b.u8(0xFC | 1).u8(0xF8).u8(0xF8).u8(0);
            }
        });
    });
    b.into_vec()
}

fn build_mp4a(cfg: &AudioTrackConfig) -> Vec<u8> {
    // objectTypeIndication: 0x6B = MPEG-1 audio (Layer III), 0x40 = MPEG-4 audio (AAC).
    let (oti, dsi, buffer_size) = match &cfg.codec {
        AudioCodecConfig::Aac { asc } => (0x40u8, Some(asc.as_slice()), 0x1800u32),
        AudioCodecConfig::Mp3 | AudioCodecConfig::Pcm { .. } => (0x6B, None, 0),
    };
    let mut b = Buf::new();
    b.atom(b"mp4a", |b| {
        b.zeros(6).u16(1);
        b.u16(0).u16(0).u32(0); // version, revision, vendor
        b.u16(cfg.channels).u16(16);
        b.u16(0).u16(0);
        b.u32(cfg.sample_rate << 16);
        b.full_atom(b"esds", 0, 0, |b| {
            b.descriptor(0x03, |b| {
                b.u16(0).u8(0); // ES_ID, flags
                b.descriptor(0x04, |b| {
                    b.u8(oti);
                    b.u8(0x15); // streamType audio (5) << 2 | reserved 1
                    b.u24(buffer_size); // bufferSizeDB
                    b.u32(cfg.bitrate_bps).u32(cfg.bitrate_bps);
                    if let Some(asc) = dsi {
                        b.descriptor(0x05, |b| {
                            b.bytes(asc);
                        });
                    }
                });
                b.descriptor(0x06, |b| {
                    b.u8(0x02);
                });
            });
        });
    });
    b.into_vec()
}
