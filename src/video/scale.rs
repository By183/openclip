//! Arbitrary-ratio frame scaling (presets / percentages) with `fast_image_resize`.

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

use super::convert::RawFrame;

/// Minimum destination pixel count before horizontal strip parallelism kicks in
/// (`1280×720`). Smaller frames stay single-threaded so thread-spawn overhead never dominates.
const PARALLEL_MIN_PIXELS: usize = 1280 * 720;
/// Cap worker strips at 4 so video prep stays fast without saturating all cores a game needs.
const MAX_THREADS: usize = 4;

pub struct Scaler {
    resizers: Vec<Resizer>,
    filter: FilterType,
    options: ResizeOptions,
    dst: (u32, u32),
    tight: Vec<u8>,
    /// The fitted picture, before it is centred into a letterboxed frame.
    scratch: Vec<u8>,
}

fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, MAX_THREADS)
}

/// Bilinear for mild downscaling keeps screen text crisp; a box filter avoids
/// aliasing beyond 2×.
fn filter_for(src: (u32, u32), dst: (u32, u32)) -> FilterType {
    let ratio = (src.0 as f32 / dst.0.max(1) as f32).max(src.1 as f32 / dst.1.max(1) as f32);
    if ratio > 2.0 { FilterType::Box } else { FilterType::Bilinear }
}

fn alg(filter: FilterType) -> ResizeOptions {
    ResizeOptions::new().resize_alg(ResizeAlg::Convolution(filter)).use_alpha(false)
}

/// The frame's pixels with no row padding, borrowed straight from `frame` when
/// it is already tightly packed and repacked into `tight` when it is not.
fn pack<'a>(tight: &'a mut Vec<u8>, frame: &'a RawFrame) -> &'a [u8] {
    let (w, h) = (frame.width, frame.height);
    let row = (w * 4) as usize;
    if frame.stride as usize == row && frame.data.len() >= row * h as usize {
        return &frame.data[..row * h as usize];
    }
    tight.clear();
    tight.reserve(row * h as usize);
    for y in 0..h as usize {
        let start = y * frame.stride as usize;
        tight.extend_from_slice(&frame.data[start..start + row]);
    }
    tight
}

/// Largest size with `src`'s aspect ratio that fits inside `dst`. Both axes are
/// rounded down to even numbers so the centred picture lands on the chroma grid
/// of the 4:2:0 formats every encoder here takes.
pub(crate) fn fit_inside(src: (u32, u32), dst: (u32, u32)) -> (u32, u32) {
    let (sw, sh) = (src.0.max(1) as f64, src.1.max(1) as f64);
    let (dw, dh) = (dst.0.max(2), dst.1.max(2));
    let scale = (dw as f64 / sw).min(dh as f64 / sh);
    let even = |v: f64, max: u32| (((v).round() as u32) & !1).clamp(2, max & !1);
    (even(sw * scale, dw), even(sh * scale, dh))
}

/// Paints the bars around an `fw`×`fh` picture placed at (`ox`, `oy`) black.
/// Only the bars are touched: an aspect-preserving fit leaves them on one axis
/// at most, so re-blacking the whole frame every slot would be wasted work.
fn fill_bars(data: &mut [u8], stride: usize, dh: u32, ox: u32, oy: u32, fw: u32, fh: u32) {
    const BLACK: [u8; 4] = [0, 0, 0, 255];
    let paint = |row: &mut [u8]| {
        for px in row.as_chunks_mut::<4>().0 {
            *px = BLACK;
        }
    };
    for y in (0..oy as usize).chain((oy + fh) as usize..dh as usize) {
        paint(&mut data[y * stride..(y + 1) * stride]);
    }
    let (left, right) = (ox as usize * 4, (ox + fw) as usize * 4);
    for y in oy as usize..(oy + fh) as usize {
        let row = &mut data[y * stride..(y + 1) * stride];
        paint(&mut row[..left]);
        paint(&mut row[right..]);
    }
}

impl Scaler {
    /// Scales `src`-sized frames to `dst`.
    pub fn new(src: (u32, u32), dst: (u32, u32)) -> Self {
        let filter = filter_for(src, dst);
        let threads = default_threads();
        Self {
            resizers: (0..threads).map(|_| Resizer::new()).collect(),
            filter,
            options: alg(filter),
            dst,
            tight: Vec::new(),
            scratch: Vec::new(),
        }
    }

    pub fn dst(&self) -> (u32, u32) {
        self.dst
    }

    pub fn scale(&mut self, frame: &RawFrame) -> RawFrame {
        let mut out = RawFrame::empty(frame.format);
        self.scale_into(frame, &mut out);
        out
    }

    /// Scales `frame` into `dst`, reusing `dst`'s buffer.
    pub fn scale_into(&mut self, frame: &RawFrame, dst: &mut RawFrame) {
        let max_threads = if (self.dst.0 as usize).saturating_mul(self.dst.1 as usize) >= PARALLEL_MIN_PIXELS
            && frame.height >= 16
            && self.dst.1 >= 16
        {
            self.resizers.len()
        } else {
            1
        };
        self.scale_into_with_threads(frame, dst, max_threads);
    }

    fn scale_into_with_threads(&mut self, frame: &RawFrame, dst: &mut RawFrame, max_threads: usize) {
        let (w, h) = (frame.width, frame.height);
        dst.format = frame.format;
        dst.pts = frame.pts;
        dst.mouse = frame.mouse.clone();
        let src_bytes = pack(&mut self.tight, frame);
        let copy_source = |dst: &mut RawFrame| {
            dst.data.clear();
            dst.data.extend_from_slice(src_bytes);
            dst.width = w;
            dst.height = h;
            dst.stride = w * 4;
        };
        if (w, h) == self.dst {
            copy_source(dst);
            return;
        }
        let (dw, dh) = self.dst;
        let needed = (dw * dh * 4) as usize;
        // The resizer writes every destination byte, so only size the buffer —
        // re-zeroing it would touch several MB per frame for nothing.
        if dst.data.len() < needed {
            dst.data.resize(needed, 0);
        } else {
            dst.data.truncate(needed);
        }

        let threads = max_threads.min(self.resizers.len()).max(1);
        let dst_slice_rows = (dh as usize / threads).max(1);
        if threads > 1 && h >= 16 && dh >= 16 && dst_slice_rows < dh as usize {
            let filter = self.filter;
            let row_bytes = dw as usize * 4;
            let src_row_bytes = w as usize * 4;
            let scale_y = h as f64 / dh as f64;
            // Generous vertical convolution support overlap so border coefficients match
            // a single full-frame pass for both Bilinear and Box downscaling.
            let r_src = 1.5 * scale_y.max(1.0) + 2.0;

            let err = std::thread::scope(|s| -> Result<(), String> {
                let mut handles = Vec::with_capacity(threads);
                let mut rem_dst = dst.data.as_mut_slice();
                let mut rem_resizers = self.resizers.as_mut_slice();
                let mut start_dy = 0usize;
                for ti in 0..threads {
                    let end_dy = if ti + 1 == threads { dh as usize } else { (start_dy + dst_slice_rows).min(dh as usize) };
                    let strip_h = end_dy - start_dy;
                    let (dst_part, dst_next) = rem_dst.split_at_mut(strip_h * row_bytes);
                    rem_dst = dst_next;
                    let (r_head, r_tail) = rem_resizers.split_first_mut().unwrap();
                    rem_resizers = r_tail;

                    let src_top = ((start_dy as f64 + 0.5) * scale_y - r_src).floor().max(0.0) as u32;
                    let src_bot = (((end_dy as f64 - 0.5) * scale_y + r_src).ceil() as u32).min(h);
                    let src_strip_h = src_bot - src_top;
                    let src_strip = &src_bytes[src_top as usize * src_row_bytes..src_bot as usize * src_row_bytes];
                    let top_crop = start_dy as f64 * scale_y - src_top as f64;
                    let h_crop = strip_h as f64 * scale_y;
                    let slice_opts = alg(filter).crop(0.0, top_crop, w as f64, h_crop);

                    handles.push(s.spawn(move || -> Result<(), String> {
                        let src_img = ImageRef::new(w, src_strip_h, src_strip, PixelType::U8x4)
                            .map_err(|e| format!("bad source buffer: {e}"))?;
                        let mut tgt_img = Image::from_slice_u8(dw, strip_h as u32, dst_part, PixelType::U8x4)
                            .map_err(|e| format!("bad dest buffer: {e}"))?;
                        r_head
                            .resize(&src_img, &mut tgt_img, &slice_opts)
                            .map_err(|e| format!("resize failed: {e}"))?;
                        Ok(())
                    }));
                    start_dy = end_dy;
                }
                for h in handles {
                    h.join().map_err(|_| "scale worker panicked".to_string())??;
                }
                Ok(())
            });
            match err {
                Ok(()) => {
                    dst.width = dw;
                    dst.height = dh;
                    dst.stride = dw * 4;
                    return;
                }
                Err(e) => {
                    log::warn!("parallel resize fallback: {e}");
                }
            }
        }

        let src = match ImageRef::new(w, h, src_bytes, PixelType::U8x4) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("resize: bad source buffer: {e}");
                copy_source(dst);
                return;
            }
        };
        let mut target = Image::from_slice_u8(dw, dh, &mut dst.data, PixelType::U8x4).expect("sized above");
        if let Err(e) = self.resizers[0].resize(&src, &mut target, &self.options) {
            log::warn!("resize failed: {e}");
            copy_source(dst);
            return;
        }
        dst.width = dw;
        dst.height = dh;
        dst.stride = dw * 4;
    }

    /// Fits `frame` into the fixed output size, preserving its aspect ratio and
    /// filling the leftover edges with black.
    ///
    /// The encoder, converter and container header are all built from the first
    /// frame, so a source that changes size mid-recording — a game toggling
    /// fullscreen or changing resolution — cannot be re-negotiated. Fitting the
    /// new picture into the original frame keeps the recording going and keeps
    /// the aspect ratio honest, where stretching it to fill would not.
    pub fn letterbox_into(&mut self, frame: &RawFrame, dst: &mut RawFrame) {
        let (w, h) = (frame.width, frame.height);
        let (dw, dh) = self.dst;
        if (w, h) == (dw, dh) {
            // Same size after all: the plain path already copies straight through.
            self.scale_into(frame, dst);
            return;
        }
        dst.format = frame.format;
        dst.pts = frame.pts;
        dst.mouse = frame.mouse.clone();
        let stride = dw as usize * 4;
        let needed = stride * dh as usize;
        if dst.data.len() < needed {
            dst.data.resize(needed, 0);
        } else {
            dst.data.truncate(needed);
        }
        dst.width = dw;
        dst.height = dh;
        dst.stride = dw * 4;

        let (fw, fh) = fit_inside((w, h), (dw, dh));
        let opts = alg(filter_for((w, h), (fw, fh)));
        let src_bytes = pack(&mut self.tight, frame);
        self.scratch.resize(fw as usize * fh as usize * 4, 0);
        let mut fitted = false;
        if let Ok(src) = ImageRef::new(w, h, src_bytes, PixelType::U8x4)
            && let Ok(mut target) = Image::from_slice_u8(fw, fh, &mut self.scratch, PixelType::U8x4)
        {
            fitted = self.resizers[0].resize(&src, &mut target, &opts).is_ok();
        }
        if !fitted {
            // Nothing usable to show; a black frame still keeps the timeline going.
            log::warn!("letterbox: could not fit {w}×{h} into {fw}×{fh}");
            dst.data.fill(0);
            return;
        }
        let (ox, oy) = (((dw - fw) / 2) & !1, ((dh - fh) / 2) & !1);
        fill_bars(&mut dst.data, stride, dh, ox, oy, fw, fh);
        let row = fw as usize * 4;
        for y in 0..fh as usize {
            let s = y * row;
            let d = (oy as usize + y) * stride + ox as usize * 4;
            dst.data[d..d + row].copy_from_slice(&self.scratch[s..s + row]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::PixelFormat;
    use std::time::Duration;

    fn solid(w: u32, h: u32, px: [u8; 4]) -> RawFrame {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for p in data.as_chunks_mut::<4>().0 {
            *p = px;
        }
        RawFrame { data, width: w, height: h, stride: w * 4, format: PixelFormat::Bgra, pts: Duration::ZERO, mouse: None }
    }

    #[test]
    fn fits_inside_preserving_aspect() {
        // Wider than the target: the width binds, bars go top and bottom.
        assert_eq!(fit_inside((1920, 1080), (1280, 1280)), (1280, 720));
        // Taller: the height binds, bars go left and right.
        assert_eq!(fit_inside((1080, 1920), (1280, 1280)), (720, 1280));
        // Matching aspect fills the frame exactly.
        assert_eq!(fit_inside((3840, 2160), (1920, 1080)), (1920, 1080));
        // Upscaling a smaller source is allowed; it still fits.
        assert_eq!(fit_inside((640, 360), (1920, 1080)), (1920, 1080));
        // Awkward ratios round down to even and never overflow the frame.
        let (w, h) = fit_inside((1001, 999), (640, 480));
        assert_eq!((w % 2, h % 2), (0, 0));
        assert!(w <= 640 && h <= 480);
    }

    #[test]
    fn letterboxes_a_source_that_changed_size() {
        // Output stays locked to the size the encoder was built for.
        let mut s = Scaler::new((640, 480), (640, 480));
        let src = solid(800, 400, [10, 20, 30, 255]);
        let mut out = RawFrame::empty(PixelFormat::Bgra);
        s.letterbox_into(&src, &mut out);
        assert_eq!((out.width, out.height, out.stride), (640, 480, 640 * 4));
        assert_eq!(out.data.len(), 640 * 480 * 4);

        let (fw, fh) = fit_inside((800, 400), (640, 480));
        assert_eq!((fw, fh), (640, 320));
        let oy = ((480 - fh) / 2) & !1;
        // The bars are black...
        assert_eq!(&out.data[..4], &[0, 0, 0, 255]);
        let last = out.data.len() - 4;
        assert_eq!(&out.data[last..], &[0, 0, 0, 255]);
        // ...and the picture is centred between them.
        let mid = (oy as usize + 10) * out.stride as usize + 320 * 4;
        assert_eq!(&out.data[mid..mid + 4], &[10, 20, 30, 255]);
    }

    #[test]
    fn letterbox_passes_a_matching_size_straight_through() {
        let mut s = Scaler::new((320, 240), (320, 240));
        let src = solid(320, 240, [1, 2, 3, 255]);
        let mut out = RawFrame::empty(PixelFormat::Bgra);
        s.letterbox_into(&src, &mut out);
        assert_eq!((out.width, out.height), (320, 240));
        assert_eq!(&out.data[..4], &[1, 2, 3, 255]);
    }

    #[test]
    fn scales_to_target_size() {
        let (w, h) = (64, 48);
        let mut data = vec![0u8; (w * h * 4) as usize];
        for px in data.as_chunks_mut::<4>().0 {
            *px = [10, 20, 30, 255];
        }
        let frame = RawFrame { data, width: w, height: h, stride: w * 4, format: PixelFormat::Bgra, pts: Duration::ZERO, mouse: None };
        let mut s = Scaler::new((w, h), (32, 24));
        let out = s.scale(&frame);
        assert_eq!((out.width, out.height, out.stride), (32, 24, 128));
        assert_eq!(out.data.len(), 32 * 24 * 4);
        assert_eq!(&out.data[..4], &[10, 20, 30, 255]);
        // Padded stride path.
        let mut padded = frame.clone();
        padded.stride = w * 4 + 16;
        let mut d = Vec::new();
        for y in 0..h as usize {
            d.extend_from_slice(&frame.data[y * (w * 4) as usize..(y + 1) * (w * 4) as usize]);
            d.extend_from_slice(&[0; 16]);
        }
        padded.data = d;
        let out2 = Scaler::new((w, h), (20, 10)).scale(&padded);
        assert_eq!((out2.width, out2.height), (20, 10));
        assert_eq!(&out2.data[..4], &[10, 20, 30, 255]);
    }

    fn pattern_frame(w: u32, h: u32) -> RawFrame {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                data[i] = ((x * 7 + y * 13) % 256) as u8;
                data[i + 1] = (((x * 3) ^ (y * 19)) % 256) as u8;
                data[i + 2] = ((x * 11 + y * 5) % 256) as u8;
                data[i + 3] = 255;
            }
        }
        RawFrame { data, width: w, height: h, stride: w * 4, format: PixelFormat::Bgra, pts: Duration::ZERO, mouse: None }
    }

    #[test]
    fn parallel_strips_match_single_thread_bilinear_and_box() {
        for (src, dst) in [
            ((320, 180), (240, 135)),  // Bilinear (4:3 downscale like 1440p->1080p)
            ((257, 145), (192, 108)),  // Bilinear non-multiple
            ((320, 180), (80, 45)),    // Box 4x (like 1440p->360p)
            ((257, 145), (80, 44)),    // Box non-integer >2x
            ((160, 90), (240, 135)),   // Bilinear upscale
        ] {
            let frame = pattern_frame(src.0, src.1);
            let mut single = Scaler::new(src, dst);
            let mut parallel = Scaler::new(src, dst);
            let mut ref_out = RawFrame::empty(PixelFormat::Bgra);
            let mut par_out = RawFrame::empty(PixelFormat::Bgra);
            single.scale_into_with_threads(&frame, &mut ref_out, 1);
            parallel.scale_into_with_threads(&frame, &mut par_out, 4);
            assert_eq!(ref_out.width, par_out.width);
            assert_eq!(ref_out.height, par_out.height);
            let max_diff = ref_out
                .data
                .iter()
                .zip(par_out.data.iter())
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            assert!(
                max_diff <= 1,
                "src={src:?} dst={dst:?} diverged by max_diff={max_diff}"
            );
        }
    }
}
