//! Places captured audio on the recording timeline and mixes sources into a
//! single stereo stream at the master sample rate.
//!
//! Each source's chunks are mapped to timeline positions using their arrival
//! time; gaps (e.g. WASAPI loopback delivering nothing while the system is
//! silent) are filled with silence so audio never drifts against video.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::capture::SharedQueue;
use super::resample::KaiserSincResampler;

/// If a source's sample count lags its wall-clock position by more than this,
/// the gap is filled with silence.
const GAP_THRESHOLD: Duration = Duration::from_millis(60);

/// If a source accumulates more than this much buffered audio ahead of its
/// wall-clock position (e.g. a fast hardware clock over a long session),
/// trim excess lead so audio never drifts permanently late relative to video.
const MAX_LEAD: Duration = Duration::from_millis(400);

/// Short click-free ramp applied after silence gaps.
const FADE_DURATION: Duration = Duration::from_millis(2);

/// Smooth overlap-add crossfade when trimming accumulated hardware clock drift.
const CROSSFADE_DURATION: Duration = Duration::from_millis(5);

struct SourceState {
    queue: SharedQueue,
    channels: usize,
    resampler: KaiserSincResampler,
    /// Stereo interleaved samples at the master rate, starting at `buffer_start`.
    buffer: VecDeque<f32>,
    /// Timeline frame index of `buffer[0]` (or where the drained end stands).
    buffer_start: i64,
    /// True once at least one chunk has anchored this source's timeline position.
    initialized: bool,
    /// Remaining excess frames to trim after crossing `MAX_LEAD`.
    trim_frames: usize,
    /// Pending crossfade across the trim seam into the queued buffer tail.
    crossfade: bool,
    scratch: Vec<f32>,
    stereo: Vec<f32>,
    gain: f32,
}

pub struct Mixer {
    rate: u32,
    epoch: Instant,
    /// Timeline origin (first video frame) relative to `epoch`.
    origin: Duration,
    sources: Vec<SourceState>,
    /// Next output frame index.
    out_pos: i64,
    pub silence_inserted: u64,
}

impl Mixer {
    pub fn new(rate: u32, epoch: Instant, origin: Duration) -> Self {
        Self { rate, epoch, origin, sources: Vec::new(), out_pos: 0, silence_inserted: 0 }
    }

    pub fn add_source(&mut self, queue: SharedQueue, sample_rate: u32, channels: u16, gain: f32) -> usize {
        let id = self.sources.len();
        self.sources.push(SourceState {
            queue,
            channels: channels.max(1) as usize,
            resampler: KaiserSincResampler::new(sample_rate, self.rate, 2),
            buffer: VecDeque::new(),
            buffer_start: 0,
            initialized: false,
            trim_frames: 0,
            crossfade: false,
            scratch: Vec::new(),
            stereo: Vec::new(),
            gain,
        });
        id
    }

    /// Replaces the source queue and updates format parameters (sample rate,
    /// channels, gain) after a device reconnect or hot-swap, preserving
    /// timeline continuity and buffered audio.
    pub fn update_source(
        &mut self,
        id: usize,
        queue: SharedQueue,
        sample_rate: u32,
        channels: u16,
        gain: Option<f32>,
    ) {
        if let Some(src) = self.sources.get_mut(id) {
            src.queue = queue;
            src.channels = channels.max(1) as usize;
            src.resampler = KaiserSincResampler::new(sample_rate, self.rate, 2);
            if let Some(g) = gain {
                src.gain = g;
            }
            if src.buffer.is_empty() {
                src.buffer_start = src.buffer_start.max(self.out_pos);
            }
            src.trim_frames = 0;
            src.crossfade = false;
        }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Output frames produced so far.
    pub fn position(&self) -> i64 {
        self.out_pos
    }

    /// Removes `delta` of wall-clock time from the timeline (used after a
    /// pause): anything captured meanwhile is discarded and every source
    /// re-aligns by arrival time on its next chunk.
    pub fn shift_origin(&mut self, delta: Duration) {
        self.origin += delta;
        for src in &mut self.sources {
            src.queue.lock().unwrap().chunks.clear();
            src.buffer.clear();
            src.buffer_start = self.out_pos;
            src.initialized = false;
            src.trim_frames = 0;
            src.crossfade = false;
            src.resampler.reset();
        }
    }

    fn timeline_frames(&self, at: Instant) -> i64 {
        let since_epoch = match at.checked_duration_since(self.epoch) {
            Some(d) => d.as_secs_f64(),
            None => -(self.epoch.duration_since(at).as_secs_f64()),
        };
        let t = since_epoch - self.origin.as_secs_f64();
        (t * self.rate as f64).round() as i64
    }

    /// Pulls queued chunks from every source onto the timeline.
    fn ingest(&mut self) {
        let rate = self.rate as f64;
        let gap_frames = (GAP_THRESHOLD.as_secs_f64() * rate) as i64;
        let max_lead_frames = (MAX_LEAD.as_secs_f64() * rate) as i64;
        let fade_frames = ((FADE_DURATION.as_secs_f64() * rate).round() as usize).max(1);
        let crossfade_frames = ((CROSSFADE_DURATION.as_secs_f64() * rate).round() as usize).max(1);

        for src in &mut self.sources {
            let chunks: Vec<_> = {
                let mut q = src.queue.lock().unwrap();
                q.chunks.drain(..).collect()
            };
            for chunk in chunks {
                let frames_in = chunk.data.len() / src.channels;
                if frames_in == 0 {
                    continue;
                }
                let arrival = {
                    let since_epoch = match chunk.at.checked_duration_since(self.epoch) {
                        Some(d) => d.as_secs_f64(),
                        None => -(self.epoch.duration_since(chunk.at).as_secs_f64()),
                    };
                    let t = since_epoch - self.origin.as_secs_f64();
                    (t * rate).round() as i64
                };

                // If the stream just resumed after a silent gap (> gap_frames),
                // drop any stale pre-gap resampler lookahead before processing.
                if src.initialized {
                    let buffer_end = src.buffer_start + (src.buffer.len() / 2) as i64;
                    let approx_out = (frames_in as f64 * rate / src.resampler_in_rate()).round() as i64;
                    let approx_start = arrival
                        - src.resampler.pending_out_frames().round() as i64
                        - approx_out;
                    if approx_start - buffer_end > gap_frames {
                        src.resampler.reset();
                    }
                }

                // Downmix / upmix to stereo and resample.
                src.stereo.clear();
                push_stereo_downmix(&chunk.data, src.channels, src.gain, &mut src.stereo);
                src.scratch.clear();
                src.resampler.process(&src.stereo, &mut src.scratch);
                let frames_out = src.scratch.len() / 2;
                if frames_out == 0 {
                    continue;
                }

                let emit_end = arrival - src.resampler.pending_out_frames().round() as i64;
                let expected_start = emit_end - frames_out as i64;
                let buffer_end = src.buffer_start + (src.buffer.len() / 2) as i64;

                if !src.initialized {
                    src.buffer_start = expected_start;
                    src.initialized = true;
                    src.trim_frames = 0;
                    src.crossfade = false;
                    fade_in(&mut src.scratch, fade_frames);
                    src.buffer.extend(src.scratch.iter().copied());
                } else if expected_start - buffer_end > gap_frames {
                    // Source stalled (e.g. loopback silence): keep buffer_end and
                    // expected_start synchronized via the single buffer cursor.
                    src.trim_frames = 0;
                    src.crossfade = false;
                    let pad = (expected_start - buffer_end) as usize;
                    if src.buffer.is_empty() {
                        src.buffer_start = expected_start;
                    } else {
                        src.buffer.extend(std::iter::repeat_n(0.0, pad * 2));
                    }
                    self.silence_inserted += pad as u64;
                    fade_in(&mut src.scratch, fade_frames);
                    src.buffer.extend(src.scratch.iter().copied());
                } else {
                    if src.trim_frames == 0 && buffer_end - expected_start > max_lead_frames {
                        // Hardware clock ran noticeably fast ahead of wall clock:
                        // schedule a single trim back toward `max_lead_frames / 2`
                        // followed by an overlap-add crossfade at the seam.
                        src.trim_frames = (buffer_end - expected_start - max_lead_frames / 2).max(1) as usize;
                        src.crossfade = true;
                    }
                    if src.trim_frames >= frames_out {
                        src.trim_frames -= frames_out;
                    } else {
                        let drop = src.trim_frames;
                        src.trim_frames = 0;
                        let tail = &mut src.scratch[drop * 2..];
                        if src.crossfade {
                            src.crossfade = false;
                            let tail_frames = tail.len() / 2;
                            let buf_frames = src.buffer.len() / 2;
                            let k = crossfade_frames.min(buf_frames).min(tail_frames);
                            if k > 0 {
                                let base = src.buffer.len() - k * 2;
                                for f in 0..k {
                                    let alpha = (f + 1) as f32 / (k + 1) as f32;
                                    let keep = 1.0 - alpha;
                                    src.buffer[base + f * 2] =
                                        src.buffer[base + f * 2] * keep + tail[f * 2] * alpha;
                                    src.buffer[base + f * 2 + 1] =
                                        src.buffer[base + f * 2 + 1] * keep + tail[f * 2 + 1] * alpha;
                                }
                                src.buffer.extend(tail[k * 2..].iter().copied());
                            } else {
                                fade_in(tail, fade_frames);
                                src.buffer.extend(tail.iter().copied());
                            }
                        } else {
                            // Normal continuous audio (including minor callback jitter):
                            // append seamlessly right onto `buffer_end`.
                            src.buffer.extend(tail.iter().copied());
                        }
                    }
                }
            }
        }
    }

    /// Mixes all sources up to timeline frame `target`, appending interleaved
    /// stereo samples to `out`.
    pub fn mix_until(&mut self, target_time: Instant, out: &mut Vec<f32>) {
        self.ingest();
        let target = self.timeline_frames(target_time);
        if target <= self.out_pos {
            return;
        }
        let n = (target - self.out_pos) as usize;
        let start = out.len();
        out.resize(start + n * 2, 0.0);
        let dst = &mut out[start..];
        for src in &mut self.sources {
            // Discard anything older than the output position.
            while src.buffer_start < self.out_pos && !src.buffer.is_empty() {
                src.buffer.pop_front();
                src.buffer.pop_front();
                src.buffer_start += 1;
            }
            if src.buffer.is_empty() {
                src.buffer_start = src.buffer_start.max(self.out_pos);
            }
            let offset = (src.buffer_start - self.out_pos).max(0) as usize; // frames of silence first
            let avail = src.buffer.len() / 2;
            let take = avail.min(n.saturating_sub(offset));
            for i in 0..take {
                let d = (offset + i) * 2;
                dst[d] += src.buffer[i * 2];
                dst[d + 1] += src.buffer[i * 2 + 1];
            }
            src.buffer.drain(..take * 2);
            src.buffer_start += take as i64;
        }
        for s in dst.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
        self.out_pos = target;
    }
}

impl SourceState {
    fn resampler_in_rate(&self) -> f64 {
        self.resampler.input_rate() as f64
    }
}

/// Folds interleaved `channels` into stereo while preserving center/surround energy.
fn push_stereo_downmix(input: &[f32], channels: usize, gain: f32, out: &mut Vec<f32>) {
    const SURROUND_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;
    let frames = input.len() / channels;
    out.reserve(frames * 2);
    match channels {
        1 => {
            for &m in &input[..frames] {
                let v = m * gain;
                out.push(v);
                out.push(v);
            }
        }
        2 => {
            for f in 0..frames {
                let b = f * 2;
                out.push(input[b] * gain);
                out.push(input[b + 1] * gain);
            }
        }
        // Quad (FL, FR, BL, BR)
        4 => {
            for f in 0..frames {
                let b = f * 4;
                out.push((input[b] + input[b + 2] * SURROUND_GAIN) * gain);
                out.push((input[b + 1] + input[b + 3] * SURROUND_GAIN) * gain);
            }
        }
        // 5.1 (FL, FR, FC, LFE, BL, BR)
        6 => {
            for f in 0..frames {
                let b = f * 6;
                let center = input[b + 2] * SURROUND_GAIN;
                out.push((input[b] + center + input[b + 4] * SURROUND_GAIN) * gain);
                out.push((input[b + 1] + center + input[b + 5] * SURROUND_GAIN) * gain);
            }
        }
        // 7.1 (FL, FR, FC, LFE, BL, BR, SL, SR)
        8 => {
            for f in 0..frames {
                let b = f * 8;
                let center = input[b + 2] * SURROUND_GAIN;
                out.push((input[b] + center + (input[b + 4] + input[b + 6]) * SURROUND_GAIN) * gain);
                out.push((input[b + 1] + center + (input[b + 5] + input[b + 7]) * SURROUND_GAIN) * gain);
            }
        }
        // Arbitrary layouts (3ch / >2ch): preserve FL/FR plus center/ambient contribution.
        _ => {
            for f in 0..frames {
                let b = f * channels;
                let mut l = input[b];
                let mut r = input[b + 1];
                let extra = channels - 2;
                let weight = SURROUND_GAIN / extra as f32;
                for &s in &input[b + 2..b + channels] {
                    l += s * weight;
                    r += s * weight;
                }
                out.push(l * gain);
                out.push(r * gain);
            }
        }
    }
}

fn fade_in(stereo: &mut [f32], fade_frames: usize) {
    let frames = (stereo.len() / 2).min(fade_frames);
    for f in 0..frames {
        let g = f as f32 / frames as f32;
        stereo[f * 2] *= g;
        stereo[f * 2 + 1] *= g;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::capture::{Chunk, Queue};
    use std::sync::{Arc, Mutex};

    fn make_queue() -> SharedQueue {
        Arc::new(Mutex::new(Queue::default()))
    }

    #[test]
    fn empty_drain_then_gap_does_not_shift_audio() {
        let epoch = Instant::now();
        let q = make_queue();
        let mut mixer = Mixer::new(48_000, epoch, Duration::ZERO);
        mixer.add_source(q.clone(), 48_000, 1, 1.0);

        // Chunk 1: 0..20 ms at amplitude +0.5
        q.lock().unwrap().chunks.push_back(Chunk {
            at: epoch + Duration::from_millis(20),
            data: vec![0.5; 960],
        });
        // Drain up to 100 ms -> buffer empties at 20 ms, `out_pos` advances to 100 ms.
        let mut out1 = Vec::new();
        mixer.mix_until(epoch + Duration::from_millis(100), &mut out1);
        assert_eq!(out1.len(), 4800 * 2);

        // Chunk 2 arrives after an 80 ms gap (> GAP_THRESHOLD = 60 ms): covers 100..120 ms!
        q.lock().unwrap().chunks.push_back(Chunk {
            at: epoch + Duration::from_millis(120),
            data: vec![0.8; 960],
        });
        let mut out2 = Vec::new();
        mixer.mix_until(epoch + Duration::from_millis(120), &mut out2);
        assert_eq!(out2.len(), 960 * 2);

        // Previously `buffer_start` was 100ms while `next_pos` remained 20ms, shifting
        // chunk 2 late by +80ms into silence! Now samples after the 2ms click fade
        // must immediately be +0.8 inside this 100..120ms window.
        let steady_idx = 200 * 2;
        assert!((out2[steady_idx] - 0.8).abs() < 1e-4, "got {} (shifted gap bug!)", out2[steady_idx]);
        assert!((out2[out2.len() - 2] - 0.8).abs() < 1e-4);
    }

    #[test]
    fn continuous_stream_with_jitter_has_zero_tears() {
        let rate = 48_000u32;
        let epoch = Instant::now();
        let q = make_queue();
        let mut mixer = Mixer::new(rate, epoch, Duration::ZERO);
        mixer.add_source(q.clone(), rate, 1, 1.0);

        let chunk_frames = 480usize; // 10 ms chunks
        let total_chunks = 40usize; // 400 ms
        let jitter_ms: [i64; 8] = [0, 8, -4, 15, 2, -6, 12, 3];
        let mut all = Vec::new();
        for k in 0..total_chunks {
            let mut data = Vec::with_capacity(chunk_frames);
            for i in 0..chunk_frames {
                let n = k * chunk_frames + i;
                data.push((n as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.6);
            }
            let base_ms = ((k + 1) * 10) as i64;
            let at_ms = (base_ms + jitter_ms[k % jitter_ms.len()]).max(1) as u64;
            q.lock().unwrap().chunks.push_back(Chunk {
                at: epoch + Duration::from_millis(at_ms),
                data,
            });
            // Intermittently mix while streaming, keeping a small jitter lead.
            if k >= 5 && k % 3 == 2 {
                mixer.mix_until(epoch + Duration::from_millis(((k - 3) * 10) as u64), &mut all);
            }
        }
        mixer.mix_until(epoch + Duration::from_millis(360), &mut all);
        assert_eq!(mixer.silence_inserted, 0);
        // Check RMS energy across every 20 ms block past fade-in: none may drop to zero.
        let start_frame = 200;
        let frames = all.len() / 2;
        for w in (start_frame..frames.saturating_sub(960)).step_by(960) {
            let rms = (all[w * 2..(w + 960) * 2].iter().map(|x| x * x).sum::<f32>() / (960 * 2) as f32).sqrt();
            assert!(rms > 0.35, "torn silence window at frame {w}: rms={rms}");
        }
    }

    #[test]
    fn surround_5_1_downmix_preserves_center_and_rear() {
        let mut out = Vec::new();
        // 5.1 layout: [FL=0, FR=0, FC=0.5, LFE=0.9, BL=0.2, BR=0.0]
        push_stereo_downmix(&[0.0, 0.0, 0.5, 0.9, 0.2, 0.0], 6, 1.0, &mut out);
        assert_eq!(out.len(), 2);
        let expected_l = (0.5 + 0.2) * std::f32::consts::FRAC_1_SQRT_2;
        let expected_r = 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        assert!((out[0] - expected_l).abs() < 1e-5, "l={}", out[0]);
        assert!((out[1] - expected_r).abs() < 1e-5, "r={}", out[1]);
    }

    #[test]
    fn shift_origin_resets_timeline_and_resampler() {
        let epoch = Instant::now();
        let q = make_queue();
        let mut mixer = Mixer::new(48_000, epoch, Duration::ZERO);
        mixer.add_source(q.clone(), 44_100, 1, 1.0);

        q.lock().unwrap().chunks.push_back(Chunk {
            at: epoch + Duration::from_millis(20),
            data: vec![0.4; 882],
        });
        let mut out = Vec::new();
        mixer.mix_until(epoch + Duration::from_millis(20), &mut out);

        // Simulate 500 ms pause.
        mixer.shift_origin(Duration::from_millis(500));
        q.lock().unwrap().chunks.push_back(Chunk {
            at: epoch + Duration::from_millis(540),
            data: vec![0.7; 882],
        });
        out.clear();
        mixer.mix_until(epoch + Duration::from_millis(540), &mut out);
        assert_eq!(out.len(), 960 * 2);
        // After pause + fade-in, signal matches post-pause value (+0.7).
        assert!((out[300 * 2] - 0.7).abs() < 0.01, "got {}", out[300 * 2]);
    }

    #[test]
    fn fast_clock_drift_trim_is_click_free_without_silence_dip() {
        let rate = 48_000u32;
        let epoch = Instant::now();
        let q = make_queue();
        let mut mixer = Mixer::new(rate, epoch, Duration::ZERO);
        mixer.add_source(q.clone(), rate, 1, 1.0);

        let chunk_frames = 480usize; // 10 ms
        let mut all = Vec::new();
        // Feed 150 chunks slightly faster than wall clock (+15% synthetic drift)
        // so buffered lead reliably exceeds MAX_LEAD (400 ms) and triggers trimming.
        for k in 0..150usize {
            let mut data = Vec::with_capacity(chunk_frames);
            for i in 0..chunk_frames {
                let n = k * chunk_frames + i;
                data.push((n as f32 * 220.0 * std::f32::consts::TAU / rate as f32).sin() * 0.6);
            }
            let at = epoch + Duration::from_secs_f64((k + 1) as f64 * 0.0085);
            q.lock().unwrap().chunks.push_back(Chunk { at, data });
            if k >= 25 && k % 2 == 0 {
                mixer.mix_until(epoch + Duration::from_secs_f64(k as f64 * 0.0085 - 0.05), &mut all);
            }
        }
        mixer.mix_until(epoch + Duration::from_secs_f64(1.25), &mut all);
        assert_eq!(mixer.silence_inserted, 0);
        // Past the initial 2ms fade-in, every 2ms window must keep strong signal energy
        // (no dip toward zero during the drift trim seam).
        let frames = all.len() / 2;
        for w in (200..frames.saturating_sub(96)).step_by(96) {
            let rms = (all[w * 2..(w + 96) * 2].iter().map(|x| x * x).sum::<f32>() / (96 * 2) as f32).sqrt();
            assert!(rms > 0.20, "audible dip around frame {w}: rms={rms}");
        }
    }

    #[test]
    fn reconnect_replaces_resampler_and_preserves_timeline() {
        let master_rate = 48_000u32;
        let epoch = Instant::now();
        let q1 = make_queue();
        let mut mixer = Mixer::new(master_rate, epoch, Duration::ZERO);

        // Initial device: 48 kHz stereo (2 ch)
        let src_id = mixer.add_source(q1.clone(), 48_000, 2, 1.0);

        // Deliver 200 ms of audio (20 chunks of 10 ms = 480 frames * 2 ch = 960 floats)
        let chunk_frames_48k = 480usize;
        for k in 0..20usize {
            let data = vec![0.5f32; chunk_frames_48k * 2];
            q1.lock().unwrap().chunks.push_back(Chunk {
                at: epoch + Duration::from_millis(((k + 1) * 10) as u64),
                data,
            });
        }

        let mut out = Vec::new();
        mixer.mix_until(epoch + Duration::from_millis(200), &mut out);
        assert_eq!(out.len(), 480 * 20 * 2); // 9600 frames * 2 ch = 19200 floats
        assert!((out[1000 * 2] - 0.5).abs() < 1e-4);

        // Disconnect: 500 ms of silence (mix until 700 ms without new chunks).
        mixer.mix_until(epoch + Duration::from_millis(700), &mut out);
        assert_eq!(out.len(), (48_000 * 700 / 1000) * 2); // 33600 frames * 2 ch = 67200 floats
        // Check that the silence gap contains 0.0
        let gap_sample = out[15000 * 2];
        assert_eq!(gap_sample, 0.0);

        // Reconnect with a different device: 44.1 kHz mono (1 ch)
        let q2 = make_queue();
        mixer.update_source(src_id, q2.clone(), 44_100, 1, None);

        // Deliver 300 ms of audio at 44.1 kHz (30 chunks of 10 ms = 441 floats each)
        // from 700 ms to 1000 ms.
        let chunk_frames_44k = 441usize;
        for k in 0..30usize {
            let data = vec![0.8f32; chunk_frames_44k];
            q2.lock().unwrap().chunks.push_back(Chunk {
                at: epoch + Duration::from_millis(700 + ((k + 1) * 10) as u64),
                data,
            });
        }

        mixer.mix_until(epoch + Duration::from_millis(1000), &mut out);
        assert_eq!(out.len(), 48_000 * 2); // Exactly 1.0 second of audio!

        // Verify audio after reconnect is present and resampled cleanly to ~0.8
        let reconnected_sample = out[43200 * 2]; // Around 900 ms
        assert!((reconnected_sample - 0.8).abs() < 0.05, "got {}", reconnected_sample);
    }
}
