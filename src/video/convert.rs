//! Raw frame representation and BGRA/RGBA → I420 / NV12 conversion.

use std::time::Duration;

use anyhow::{bail, Result};
use openh264::formats::YUVSlices;

use super::encoder::{FrameInput, InputLayout};
use super::mouse_fx::MouseSnapshot;
use yuv::{
    bgra_to_yuv420, bgra_to_yuv_nv12, rgba_to_yuv420, rgba_to_yuv_nv12, BufferStoreMut, YuvBiPlanarImageMut,
    YuvChromaSubsampling, YuvConversionMode, YuvPlanarImageMut, YuvRange, YuvStandardMatrix,
};

/// Minimum pixel count before horizontal strip parallelism kicks in (`1280×720`).
const PARALLEL_MIN_PIXELS: usize = 1280 * 720;
/// Half-size downscale destination pixel threshold (`960×540`, i.e. half of 1080p).
const HALF_PARALLEL_MIN_PIXELS: usize = 960 * 540;
/// Cap worker strips at 4 so prep finishes in ~2 ms without starving games.
const MAX_THREADS: usize = 4;

fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, MAX_THREADS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 4 bytes per pixel: B, G, R, A (Windows.Graphics.Capture, DXGI).
    Bgra,
    /// 4 bytes per pixel: R, G, B, A (xcap, image crate).
    Rgba,
}

/// A captured frame in a packed 32-bit pixel format.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Bytes per row (>= width * 4).
    pub stride: u32,
    pub format: PixelFormat,
    /// Capture time relative to the recording epoch.
    pub pts: Duration,
    /// Pointer state sampled when the frame was captured (global pixels).
    pub mouse: Option<MouseSnapshot>,
}

impl RawFrame {
    /// An empty frame whose buffer is reused by the `*_into` scalers.
    pub fn empty(format: PixelFormat) -> RawFrame {
        RawFrame { data: Vec::new(), width: 0, height: 0, stride: 0, format, pts: Duration::ZERO, mouse: None }
    }

    /// Copies a padded, possibly bottom-up 32-bit source into a top-down,
    /// tightly packed BGRA frame.
    ///
    /// Media Foundation hands back RGB32 (which is BGRA on little-endian
    /// Windows) with a `MF_MT_DEFAULT_STRIDE` that is **negative** when the
    /// rows run bottom-up, so both the padding and the flip are handled here.
    /// `None` when the buffer is too small for the stated geometry.
    pub fn from_bgra_rows(src: &[u8], width: u32, height: u32, stride: i32) -> Option<RawFrame> {
        let pitch = stride.unsigned_abs() as usize;
        let row = width as usize * 4;
        if width == 0 || height == 0 || pitch < row || src.len() < pitch * height as usize {
            return None;
        }
        let mut data = vec![0u8; row * height as usize];
        for y in 0..height as usize {
            let sy = if stride < 0 { height as usize - 1 - y } else { y };
            data[y * row..(y + 1) * row].copy_from_slice(&src[sy * pitch..sy * pitch + row]);
        }
        Some(RawFrame {
            data,
            width,
            height,
            stride: width * 4,
            format: PixelFormat::Bgra,
            pts: Duration::ZERO,
            mouse: None,
        })
    }

    /// Returns a copy at half resolution using a 2×2 box filter.
    pub fn downscale_half(&self) -> RawFrame {
        let mut out = RawFrame::empty(self.format);
        self.downscale_half_into(&mut out);
        out
    }

    /// Half-resolution 2×2 box filter into `dst`, reusing its buffer.
    pub fn downscale_half_into(&self, dst: &mut RawFrame) {
        let w = (self.width / 2).max(1);
        let h = (self.height / 2).max(1);
        let threads = if (w as usize).saturating_mul(h as usize) >= HALF_PARALLEL_MIN_PIXELS && h >= 16 {
            default_threads()
        } else {
            1
        };
        self.downscale_half_into_with_threads(dst, threads);
    }

    fn downscale_half_into_with_threads(&self, dst: &mut RawFrame, max_threads: usize) {
        let w = (self.width / 2).max(1);
        let h = (self.height / 2).max(1);
        let s = self.stride as usize;
        let sw = self.width as usize;
        let sh = self.height as usize;
        let dw = w as usize;
        let dh = h as usize;
        let needed = dw * dh * 4;
        // Size the buffer once and write in place: pushing two million bytes one
        // at a time was the most expensive step of the half-size path.
        if dst.data.len() < needed {
            dst.data.resize(needed, 0);
        } else {
            dst.data.truncate(needed);
        }
        let downscale_rows = |start_y: usize, dslice: &mut [u8]| {
            for (off, drow) in dslice.chunks_exact_mut(dw * 4).enumerate() {
                let y = start_y + off;
                let r0 = &self.data[y * 2 * s..y * 2 * s + sw * 4];
                let y1 = (y * 2 + 1).min(sh - 1);
                let r1 = &self.data[y1 * s..y1 * s + sw * 4];
                let (p0, rest0) = r0.as_chunks::<8>();
                let (p1, _) = r1.as_chunks::<8>();
                let (out, _) = drow.as_chunks_mut::<4>();
                // Pairs of source pixels (8 bytes) → one destination pixel.
                for ((a, b), o) in p0.iter().zip(p1).zip(out.iter_mut()) {
                    for ch in 0..4 {
                        let sum = a[ch] as u32 + a[ch + 4] as u32 + b[ch] as u32 + b[ch + 4] as u32;
                        o[ch] = ((sum + 2) / 4) as u8;
                    }
                }
                if p0.len() < dw {
                    // Odd source width: the last destination pixel repeats the last column.
                    let a = &rest0[..4];
                    let b = &r1[r1.len() - 4..];
                    let o = &mut out[p0.len()];
                    for ch in 0..4 {
                        o[ch] = (a[ch] as u32 + b[ch] as u32).div_ceil(2) as u8;
                    }
                }
            }
        };

        let threads = max_threads.clamp(1, MAX_THREADS);
        let slice_rows = (dh / threads).max(1);
        if threads > 1 && h >= 16 && slice_rows < dh {
            std::thread::scope(|scope| {
                let mut rem_dst = dst.data.as_mut_slice();
                let mut start_y = 0usize;
                for ti in 0..threads {
                    let end_y = if ti + 1 == threads { dh } else { (start_y + slice_rows).min(dh) };
                    let strip_h = end_y - start_y;
                    let part = if ti + 1 == threads {
                        std::mem::take(&mut rem_dst)
                    } else {
                        let (p, next) = rem_dst.split_at_mut(strip_h * dw * 4);
                        rem_dst = next;
                        p
                    };
                    scope.spawn(move || downscale_rows(start_y, part));
                    start_y = end_y;
                }
            });
        } else {
            downscale_rows(0, &mut dst.data);
        }
        dst.width = w;
        dst.height = h;
        dst.stride = w * 4;
        dst.format = self.format;
        dst.pts = self.pts;
        dst.mouse = self.mouse.clone();
    }

    /// Returns a copy cropped to the given rectangle (clamped to the frame bounds).
    pub fn crop(&self, x: u32, y: u32, width: u32, height: u32) -> RawFrame {
        let x = x.min(self.width.saturating_sub(1));
        let y = y.min(self.height.saturating_sub(1));
        let w = width.min(self.width - x).max(1);
        let h = height.min(self.height - y).max(1);
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for row in y..y + h {
            let start = (row * self.stride + x * 4) as usize;
            data.extend_from_slice(&self.data[start..start + (w * 4) as usize]);
        }
        RawFrame { data, width: w, height: h, stride: w * 4, format: self.format, pts: self.pts, mouse: self.mouse.clone() }
    }
}

/// Reusable BGRA/RGBA → I420 or NV12 converter. Output dimensions are forced
/// even (4:2:0 requires it); odd source frames lose their last column/row.
pub struct Converter {
    layout: InputLayout,
    i420: Option<YuvPlanarImageMut<'static, u8>>,
    nv12: Option<YuvBiPlanarImageMut<'static, u8>>,
    dims: (u32, u32),
}

impl Converter {
    pub fn new(width: u32, height: u32, layout: InputLayout) -> Result<Self> {
        let (w, h) = even_dims(width, height);
        if w == 0 || h == 0 {
            bail!("frame too small to encode: {width}x{height}");
        }
        let (i420, nv12) = match layout {
            InputLayout::I420 => (Some(YuvPlanarImageMut::alloc(w, h, YuvChromaSubsampling::Yuv420)), None),
            InputLayout::Nv12 => (None, Some(YuvBiPlanarImageMut::alloc(w, h, YuvChromaSubsampling::Yuv420))),
        };
        Ok(Self { layout, i420, nv12, dims: (w, h) })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        self.dims
    }

    pub fn layout(&self) -> InputLayout {
        self.layout
    }

    /// Converts `frame` into the internal buffers. The frame must match the
    /// converter dimensions after even rounding.
    pub fn convert(&mut self, frame: &RawFrame) -> Result<()> {
        let (w, h) = even_dims(frame.width, frame.height);
        let threads = if (w as usize).saturating_mul(h as usize) >= PARALLEL_MIN_PIXELS && h >= 16 {
            default_threads()
        } else {
            1
        };
        self.convert_with_threads(frame, threads)
    }

    fn convert_with_threads(&mut self, frame: &RawFrame, max_threads: usize) -> Result<()> {
        let (w, h) = even_dims(frame.width, frame.height);
        if (w, h) != self.dims {
            bail!("frame size {}x{} does not match converter {}x{}", frame.width, frame.height, self.dims.0, self.dims.1);
        }
        let needed = frame.stride as usize * (h as usize - 1) + w as usize * 4;
        if frame.data.len() < needed {
            bail!("frame buffer too small: {} < {}", frame.data.len(), needed);
        }
        let data = &frame.data[..needed];
        let (range, matrix, mode) = (YuvRange::Limited, YuvStandardMatrix::Bt709, YuvConversionMode::Balanced);

        let threads = max_threads.clamp(1, MAX_THREADS);
        // Chroma rows are 1 per 2 luma rows in 4:2:0, so each horizontal strip
        // must have an even height to keep UV planes aligned cleanly.
        let slice_rows = ((h as usize / threads) & !1).max(2);
        if threads > 1 && h >= 16 && slice_rows < h as usize {
            let src_stride = frame.stride as usize;
            let total_h = h as usize;
            match self.layout {
                InputLayout::I420 => {
                    let p = self.i420.as_mut().unwrap();
                    let (y_stride, u_stride, v_stride) = (p.y_stride as usize, p.u_stride as usize, p.v_stride as usize);
                    let y_plane: &mut [u8] = p.y_plane.borrow_mut();
                    let u_plane: &mut [u8] = p.u_plane.borrow_mut();
                    let v_plane: &mut [u8] = p.v_plane.borrow_mut();
                    return std::thread::scope(|scope| -> Result<()> {
                        let mut handles = Vec::with_capacity(threads);
                        let mut rem_src = data;
                        let mut rem_y = y_plane;
                        let mut rem_u = u_plane;
                        let mut rem_v = v_plane;
                        let mut start_y = 0usize;
                        for ti in 0..threads {
                            let last = ti + 1 == threads;
                            let end_y = if last { total_h } else { (start_y + slice_rows).min(total_h) };
                            let strip_h = end_y - start_y;
                            let src_part = if last {
                                rem_src
                            } else {
                                let (part, next) = rem_src.split_at(strip_h * src_stride);
                                rem_src = next;
                                part
                            };
                            let (y_part, u_part, v_part) = if last {
                                (
                                    std::mem::take(&mut rem_y),
                                    std::mem::take(&mut rem_u),
                                    std::mem::take(&mut rem_v),
                                )
                            } else {
                                let (yp, yn) = rem_y.split_at_mut(strip_h * y_stride);
                                let (up, un) = rem_u.split_at_mut((strip_h / 2) * u_stride);
                                let (vp, vn) = rem_v.split_at_mut((strip_h / 2) * v_stride);
                                rem_y = yn;
                                rem_u = un;
                                rem_v = vn;
                                (yp, up, vp)
                            };
                            let fmt = frame.format;
                            let stride = frame.stride;
                            handles.push(scope.spawn(move || -> Result<()> {
                                let mut sub = YuvPlanarImageMut {
                                    y_plane: BufferStoreMut::Borrowed(y_part),
                                    y_stride: y_stride as u32,
                                    u_plane: BufferStoreMut::Borrowed(u_part),
                                    u_stride: u_stride as u32,
                                    v_plane: BufferStoreMut::Borrowed(v_part),
                                    v_stride: v_stride as u32,
                                    width: w,
                                    height: strip_h as u32,
                                };
                                let r = match fmt {
                                    PixelFormat::Bgra => bgra_to_yuv420(&mut sub, src_part, stride, range, matrix, mode),
                                    PixelFormat::Rgba => rgba_to_yuv420(&mut sub, src_part, stride, range, matrix, mode),
                                };
                                r.map_err(|e| anyhow::anyhow!("yuv conversion failed: {e}"))
                            }));
                            start_y = end_y;
                            if last || start_y >= total_h {
                                break;
                            }
                        }
                        for h in handles {
                            h.join().map_err(|_| anyhow::anyhow!("yuv worker panicked"))??;
                        }
                        Ok(())
                    });
                }
                InputLayout::Nv12 => {
                    let p = self.nv12.as_mut().unwrap();
                    let (y_stride, uv_stride) = (p.y_stride as usize, p.uv_stride as usize);
                    let y_plane: &mut [u8] = p.y_plane.borrow_mut();
                    let uv_plane: &mut [u8] = p.uv_plane.borrow_mut();
                    return std::thread::scope(|scope| -> Result<()> {
                        let mut handles = Vec::with_capacity(threads);
                        let mut rem_src = data;
                        let mut rem_y = y_plane;
                        let mut rem_uv = uv_plane;
                        let mut start_y = 0usize;
                        for ti in 0..threads {
                            let last = ti + 1 == threads;
                            let end_y = if last { total_h } else { (start_y + slice_rows).min(total_h) };
                            let strip_h = end_y - start_y;
                            let src_part = if last {
                                rem_src
                            } else {
                                let (part, next) = rem_src.split_at(strip_h * src_stride);
                                rem_src = next;
                                part
                            };
                            let (y_part, uv_part) = if last {
                                (std::mem::take(&mut rem_y), std::mem::take(&mut rem_uv))
                            } else {
                                let (yp, yn) = rem_y.split_at_mut(strip_h * y_stride);
                                let (uvp, uvn) = rem_uv.split_at_mut((strip_h / 2) * uv_stride);
                                rem_y = yn;
                                rem_uv = uvn;
                                (yp, uvp)
                            };
                            let fmt = frame.format;
                            let stride = frame.stride;
                            handles.push(scope.spawn(move || -> Result<()> {
                                let mut sub = YuvBiPlanarImageMut {
                                    y_plane: BufferStoreMut::Borrowed(y_part),
                                    y_stride: y_stride as u32,
                                    uv_plane: BufferStoreMut::Borrowed(uv_part),
                                    uv_stride: uv_stride as u32,
                                    width: w,
                                    height: strip_h as u32,
                                };
                                let r = match fmt {
                                    PixelFormat::Bgra => bgra_to_yuv_nv12(&mut sub, src_part, stride, range, matrix, mode),
                                    PixelFormat::Rgba => rgba_to_yuv_nv12(&mut sub, src_part, stride, range, matrix, mode),
                                };
                                r.map_err(|e| anyhow::anyhow!("yuv conversion failed: {e}"))
                            }));
                            start_y = end_y;
                            if last || start_y >= total_h {
                                break;
                            }
                        }
                        for h in handles {
                            h.join().map_err(|_| anyhow::anyhow!("yuv worker panicked"))??;
                        }
                        Ok(())
                    });
                }
            }
        }

        let res = match (self.layout, frame.format) {
            (InputLayout::I420, PixelFormat::Bgra) => {
                bgra_to_yuv420(self.i420.as_mut().unwrap(), data, frame.stride, range, matrix, mode)
            }
            (InputLayout::I420, PixelFormat::Rgba) => {
                rgba_to_yuv420(self.i420.as_mut().unwrap(), data, frame.stride, range, matrix, mode)
            }
            (InputLayout::Nv12, PixelFormat::Bgra) => {
                bgra_to_yuv_nv12(self.nv12.as_mut().unwrap(), data, frame.stride, range, matrix, mode)
            }
            (InputLayout::Nv12, PixelFormat::Rgba) => {
                rgba_to_yuv_nv12(self.nv12.as_mut().unwrap(), data, frame.stride, range, matrix, mode)
            }
        };
        res.map_err(|e| anyhow::anyhow!("yuv conversion failed: {e}"))?;
        Ok(())
    }

    /// Zero-copy view of the converted planes for the encoder.
    pub fn frame(&self) -> FrameInput<'_> {
        match self.layout {
            InputLayout::I420 => {
                let p = self.i420.as_ref().unwrap();
                FrameInput::I420 {
                    y: plane(&p.y_plane),
                    u: plane(&p.u_plane),
                    v: plane(&p.v_plane),
                    strides: (p.y_stride as usize, p.u_stride as usize, p.v_stride as usize),
                    dims: self.dims,
                }
            }
            InputLayout::Nv12 => {
                let p = self.nv12.as_ref().unwrap();
                FrameInput::Nv12 {
                    y: plane(&p.y_plane),
                    uv: plane(&p.uv_plane),
                    strides: (p.y_stride as usize, p.uv_stride as usize),
                    dims: self.dims,
                }
            }
        }
    }

    /// I420 planes as OpenH264 slices (I420 converters only).
    pub fn yuv(&self) -> YUVSlices<'_> {
        match self.frame() {
            FrameInput::I420 { y, u, v, strides, dims } => {
                YUVSlices::new((y, u, v), (dims.0 as usize, dims.1 as usize), strides)
            }
            FrameInput::Nv12 { .. } => panic!("yuv() called on an NV12 converter"),
        }
    }
}

fn plane<'a>(store: &'a BufferStoreMut<'_, u8>) -> &'a [u8] {
    store.borrow()
}

/// Rounds dimensions down to even values.
pub fn even_dims(width: u32, height: u32) -> (u32, u32) {
    (width & !1, height & !1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::formats::YUVSource;

    fn solid(w: u32, h: u32, bgra: [u8; 4]) -> RawFrame {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            data.extend_from_slice(&bgra);
        }
        RawFrame { data, width: w, height: h, stride: w * 4, format: PixelFormat::Bgra, pts: Duration::ZERO, mouse: None }
    }

    #[test]
    fn converts_solid_white_to_limited_range_y() {
        let frame = solid(16, 8, [255, 255, 255, 255]);
        let mut c = Converter::new(16, 8, InputLayout::I420).unwrap();
        c.convert(&frame).unwrap();
        let yuv = c.yuv();
        assert_eq!(yuv.dimensions(), (16, 8));
        assert!(yuv.y().iter().all(|&v| (230..=240).contains(&v)), "y={}", yuv.y()[0]);
        assert!(yuv.u().iter().all(|&v| (126..=130).contains(&v)));
    }

    #[test]
    fn converts_to_nv12() {
        let frame = solid(16, 8, [0, 0, 255, 255]); // pure red (BGRA)
        let mut c = Converter::new(16, 8, InputLayout::Nv12).unwrap();
        c.convert(&frame).unwrap();
        let FrameInput::Nv12 { y, uv, strides, dims } = c.frame() else { panic!("nv12") };
        assert_eq!(dims, (16, 8));
        assert_eq!(strides, (16, 16));
        assert_eq!(y.len(), 16 * 8);
        assert_eq!(uv.len(), 16 * 4);
        // Red: low luma, low Cb, high Cr.
        assert!(y[0] < 90, "y={}", y[0]);
        assert!(uv[0] < 128 && uv[1] > 200, "uv={:?}", &uv[..2]);
    }

    #[test]
    fn odd_sizes_round_down() {
        let frame = solid(15, 9, [0, 0, 0, 255]);
        let mut c = Converter::new(15, 9, InputLayout::I420).unwrap();
        assert_eq!(c.dimensions(), (14, 8));
        c.convert(&frame).unwrap();
    }

    #[test]
    fn downscale_and_crop() {
        let frame = solid(8, 6, [10, 20, 30, 255]);
        let half = frame.downscale_half();
        assert_eq!((half.width, half.height), (4, 3));
        assert_eq!(&half.data[..4], &[10, 20, 30, 255]);
        assert_eq!(half.data.len(), 4 * 3 * 4);
        // Odd sizes: 7×5 → 3×2, averaging 2×2 blocks.
        let mut odd = solid(7, 5, [0, 0, 0, 255]);
        odd.data[0] = 100; // B of pixel (0,0)
        odd.data[4] = 100; // B of pixel (1,0)
        let half = odd.downscale_half();
        assert_eq!((half.width, half.height), (3, 2));
        assert_eq!(half.data[0], 50);
        assert_eq!(half.data.len(), 3 * 2 * 4);
        let cropped = frame.crop(2, 2, 4, 2);
        assert_eq!((cropped.width, cropped.height), (4, 2));
        assert_eq!(cropped.data.len(), 32);
    }

    fn pattern(w: u32, h: u32, extra_stride: u32, format: PixelFormat) -> RawFrame {
        let row = (w * 4) as usize;
        let stride = row + extra_stride as usize;
        let needed = stride * (h as usize - 1) + row;
        let mut data = vec![0xA5u8; needed];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = y * stride + x * 4;
                data[i] = ((x * 9 + y * 13) % 256) as u8;
                data[i + 1] = (((x * 5) ^ (y * 17)) % 256) as u8;
                data[i + 2] = ((x * 3 + y * 23) % 256) as u8;
                data[i + 3] = 255;
            }
        }
        RawFrame { data, width: w, height: h, stride: stride as u32, format, pts: Duration::ZERO, mouse: None }
    }

    #[test]
    fn parallel_yuv_matches_single_thread_exactly() {
        for (w, h, pad) in [(64, 18, 0), (66, 20, 16), (128, 34, 8)] {
            for fmt in [PixelFormat::Bgra, PixelFormat::Rgba] {
                let frame = pattern(w, h, pad, fmt);
                for layout in [InputLayout::Nv12, InputLayout::I420] {
                    let mut c1 = Converter::new(w, h, layout).unwrap();
                    let mut c4 = Converter::new(w, h, layout).unwrap();
                    c1.convert_with_threads(&frame, 1).unwrap();
                    c4.convert_with_threads(&frame, 4).unwrap();
                    match (c1.frame(), c4.frame()) {
                        (
                            FrameInput::Nv12 { y: y1, uv: uv1, .. },
                            FrameInput::Nv12 { y: y4, uv: uv4, .. },
                        ) => {
                            assert_eq!(y1, y4, "Nv12 Y mismatch {w}x{h} pad={pad} {fmt:?}");
                            assert_eq!(uv1, uv4, "Nv12 UV mismatch {w}x{h} pad={pad} {fmt:?}");
                        }
                        (
                            FrameInput::I420 { y: y1, u: u1, v: v1, .. },
                            FrameInput::I420 { y: y4, u: u4, v: v4, .. },
                        ) => {
                            assert_eq!(y1, y4, "I420 Y mismatch {w}x{h} pad={pad} {fmt:?}");
                            assert_eq!(u1, u4, "I420 U mismatch {w}x{h} pad={pad} {fmt:?}");
                            assert_eq!(v1, v4, "I420 V mismatch {w}x{h} pad={pad} {fmt:?}");
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }
    }

    #[test]
    fn parallel_downscale_half_matches_single_thread_exactly() {
        let frame = pattern(65, 37, 12, PixelFormat::Bgra);
        let mut r1 = RawFrame::empty(PixelFormat::Bgra);
        let mut r4 = RawFrame::empty(PixelFormat::Bgra);
        frame.downscale_half_into_with_threads(&mut r1, 1);
        frame.downscale_half_into_with_threads(&mut r4, 4);
        assert_eq!((r1.width, r1.height, r1.stride), (r4.width, r4.height, r4.stride));
        assert_eq!(r1.data, r4.data);
    }
}
