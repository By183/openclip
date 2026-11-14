//! Bandlimited windowed-sinc resampler for interleaved f32 audio.
//!
//! Uses a 16-tap Kaiser-windowed sinc filter (`beta = 6.0`) with an adaptive
//! low-pass cutoff when downsampling (`cutoff = 0.94 / ratio`) and a continuous
//! ring buffer across calls, preventing linear-interpolation aliasing/warble
//! (`44.1 kHz <-> 48 kHz`) and boundary clicks.

const RADIUS: usize = 8;
const LEFT_CTX: usize = RADIUS - 1;
const TAPS: usize = LEFT_CTX + 1 + RADIUS; // 16 taps: -7..=+8
const KAISER_BETA: f64 = 6.0;

/// 16-tap Kaiser-windowed sinc resampler maintaining filter history and fractional phase across calls.
pub struct KaiserSincResampler {
    from_rate: u32,
    channels: usize,
    ratio: f64, // input frames per output frame
    cutoff: f64,
    inv_i0_beta: f64,
    pos: f64, // fractional frame position inside `buf`
    buf: Vec<f32>,
    last_frame: Vec<f32>,
    primed: bool,
}

impl KaiserSincResampler {
    pub fn new(from_rate: u32, to_rate: u32, channels: usize) -> Self {
        let from = from_rate.max(1);
        let to = to_rate.max(1);
        let ch = channels.max(1);
        let ratio = from as f64 / to as f64;
        // Attenuate frequencies above target Nyquist before downsampling.
        let cutoff = if ratio > 1.0 { 0.94 / ratio } else { 1.0 };
        Self {
            from_rate: from,
            channels: ch,
            ratio,
            cutoff,
            inv_i0_beta: 1.0 / bessel_i0(KAISER_BETA),
            pos: LEFT_CTX as f64,
            buf: Vec::new(),
            last_frame: vec![0.0; ch],
            primed: false,
        }
    }

    pub fn input_rate(&self) -> u32 {
        self.from_rate
    }

    pub fn is_identity(&self) -> bool {
        (self.ratio - 1.0).abs() < 1e-9
    }

    /// Output frames currently held in the lookahead buffer (time lag between
    /// the last emitted output frame and the end of the most recent input chunk).
    pub fn pending_out_frames(&self) -> f64 {
        if self.is_identity() || !self.primed {
            return 0.0;
        }
        let total_frames = self.buf.len() / self.channels;
        ((total_frames as f64 - self.pos).max(0.0)) / self.ratio
    }

    /// Discards buffered filter history and resets fractional phase.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.last_frame.fill(0.0);
        self.pos = LEFT_CTX as f64;
        self.primed = false;
    }

    /// Resamples `input` (interleaved) and appends the result to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_identity() {
            out.extend_from_slice(input);
            return;
        }
        let ch = self.channels;
        let frames = input.len() / ch;
        if frames == 0 {
            return;
        }
        let valid = &input[..frames * ch];
        if !self.primed {
            for _ in 0..LEFT_CTX {
                self.buf.extend_from_slice(&valid[..ch]);
            }
            self.pos = LEFT_CTX as f64;
            self.primed = true;
        }
        self.last_frame.copy_from_slice(&valid[(frames - 1) * ch..]);
        self.buf.extend_from_slice(valid);
        let limit = usize::MAX;
        self.synthesize(out, limit);

        let drop_frames = (self.pos.floor() as isize - LEFT_CTX as isize).max(0) as usize;
        if drop_frames > 0 {
            self.buf.drain(..drop_frames * ch);
            self.pos -= drop_frames as f64;
        }
    }

    /// Drains any remaining buffered lookahead frames at end-of-stream and resets state.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        if self.is_identity() || !self.primed {
            self.reset();
            return;
        }
        let ch = self.channels;
        let real_end = self.buf.len() / ch;
        for _ in 0..=RADIUS {
            self.buf.extend_from_slice(&self.last_frame);
        }
        self.synthesize(out, real_end);
        self.reset();
    }

    fn synthesize(&mut self, out: &mut Vec<f32>, max_pos_frames: usize) {
        let ch = self.channels;
        let total_frames = self.buf.len() / ch;
        let max_pos = max_pos_frames as f64;
        let mut weights = [0.0f64; TAPS];

        while self.pos < max_pos {
            let center = self.pos.floor() as isize;
            if (center + RADIUS as isize) as usize >= total_frames {
                break;
            }
            let frac = self.pos - center as f64;
            let mut w_sum = 0.0f64;
            for (i, w_slot) in weights.iter_mut().enumerate() {
                let k = i as isize - LEFT_CTX as isize; // -LEFT_CTX ..= RADIUS
                let d = frac - k as f64;
                let u = d / RADIUS as f64;
                let w = if u.abs() >= 1.0 {
                    0.0
                } else {
                    let window = bessel_i0(KAISER_BETA * (1.0 - u * u).sqrt()) * self.inv_i0_beta;
                    sinc(self.cutoff * d) * window
                };
                *w_slot = w;
                w_sum += w;
            }
            if w_sum.abs() > 1e-12 {
                let inv_w = 1.0 / w_sum;
                let start_frame = (center - LEFT_CTX as isize) as usize;
                for c in 0..ch {
                    let mut acc = 0.0f64;
                    for (i, &w) in weights.iter().enumerate() {
                        acc += self.buf[(start_frame + i) * ch + c] as f64 * w;
                    }
                    out.push((acc * inv_w) as f32);
                }
            } else {
                let idx = center.max(0) as usize;
                out.extend_from_slice(&self.buf[idx * ch..(idx + 1) * ch]);
            }
            self.pos += self.ratio;
        }
    }
}

/// Backward-compatibility alias for [`KaiserSincResampler`].
pub type LinearResampler = KaiserSincResampler;

#[inline]
fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-8 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

#[inline]
fn bessel_i0(x: f64) -> f64 {
    let half = 0.5 * x;
    let mut sum = 1.0;
    let mut term = 1.0;
    for k in 1..=20 {
        let kf = k as f64;
        term *= (half / kf) * (half / kf);
        sum += term;
        if term < 1e-15 * sum {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halves_sample_count() {
        let mut r = KaiserSincResampler::new(96000, 48000, 1);
        let input: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let mut out = Vec::new();
        for chunk in input.chunks(100) {
            r.process(chunk, &mut out);
        }
        r.flush(&mut out);
        assert!((out.len() as i64 - 500).abs() <= 2, "len={}", out.len());
        for w in out.windows(2) {
            assert!(w[1] > w[0], "not monotonic: {:?}", w);
        }
    }

    #[test]
    fn upsamples_stereo() {
        let mut r = KaiserSincResampler::new(44100, 48000, 2);
        let input: Vec<f32> = (0..882).map(|i| (i / 2) as f32).collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        r.flush(&mut out);
        assert!((out.len() as i64 / 2 - 480).abs() <= 2, "frames={}", out.len() / 2);
    }

    #[test]
    fn chunked_44100_to_48000_sine_has_high_fidelity_and_no_glitches() {
        let in_rate = 44_100u32;
        let out_rate = 48_000u32;
        let freq = 1000.0f64;
        let n_in = 4410; // 100 ms
        let mut input = Vec::with_capacity(n_in * 2);
        for i in 0..n_in {
            let s = (i as f64 * freq * std::f64::consts::TAU / in_rate as f64).sin() as f32 * 0.8;
            input.push(s);
            input.push(-s);
        }
        let mut r = KaiserSincResampler::new(in_rate, out_rate, 2);
        let mut out = Vec::new();
        // Process in irregular chunks (441 frames = 10 ms WASAPI buffers) without flush in between.
        for chunk in input.chunks(441 * 2) {
            r.process(chunk, &mut out);
        }
        let out_frames = out.len() / 2;
        assert!(out_frames >= 4750);
        // Check interior [40 .. out_frames - 40] against ideal 1 kHz sine wave.
        let mut max_err = 0.0f32;
        for i in 40..out_frames.saturating_sub(40) {
            let expected = (i as f64 * freq * std::f64::consts::TAU / out_rate as f64).sin() as f32 * 0.8;
            max_err = max_err.max((out[i * 2] - expected).abs());
            max_err = max_err.max((out[i * 2 + 1] + expected).abs());
        }
        assert!(max_err < 0.005, "sine error across chunks too high: {max_err}");
    }

    #[test]
    fn downsamples_48000_to_44100_rejects_high_frequency_aliasing() {
        let in_rate = 48_000u32;
        let out_rate = 44_100u32;
        // 23 kHz tone is above 44.1 kHz Nyquist (22.05 kHz); anti-aliasing must attenuate it.
        let freq = 23_000.0f64;
        let n_in = 4800;
        let input: Vec<f32> = (0..n_in)
            .map(|i| (i as f64 * freq * std::f64::consts::TAU / in_rate as f64).sin() as f32)
            .collect();
        let mut r = KaiserSincResampler::new(in_rate, out_rate, 1);
        let mut out = Vec::new();
        for chunk in input.chunks(480) {
            r.process(chunk, &mut out);
        }
        let rms = (out[40..].iter().map(|&x| (x * x) as f64).sum::<f64>() / (out.len() - 40) as f64).sqrt();
        assert!(rms < 0.15, "23 kHz alias leaked too strongly: rms={rms}");
    }
}
