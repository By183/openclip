//! Direct3D 11 `VideoProcessor` helpers for zero-readback capture encoding
//! (`BGRA`/`RGBA` -> pooled `NV12`) and hardware-accelerated viewer decoding
//! (`NV12` -> `R8G8B8A8` at display resolution).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11BlendState, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext,
    ID3D11Multithread, ID3D11PixelShader, ID3D11Query, ID3D11RenderTargetView, ID3D11Resource,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
    ID3D11VideoContext, ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_DECODER,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VIDEO_ENCODER,
    D3D11_BLEND_DESC, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD,
    D3D11_BLEND_SRC_ALPHA, D3D11_BOX, D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL,
    D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAP_READ,
    D3D11_MAP_WRITE, D3D11_MAP_WRITE_DISCARD, D3D11_MAPPED_SUBRESOURCE, D3D11_QUERY_DESC,
    D3D11_QUERY_EVENT, D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SAMPLER_DESC, D3D11_SDK_VERSION,
    D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE_ADDRESS_CLAMP,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_USAGE_IMMUTABLE,
    D3D11_USAGE_STAGING, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_RGBA, D3D11_VIDEO_COLOR_YCbCrA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_ALPHA_FILL_MODE_OPAQUE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VIEWPORT, D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};

use super::mouse_fx::{
    FrameClick, MouseFx, ARROW, HIGHLIGHT_RADIUS, RIPPLE_END, RIPPLE_START, RIPPLE_THICKNESS,
};
use super::preview::PreviewImage;
use super::scale::fit_inside;
use super::watermark::{Watermark, WatermarkRenderer};

const OVERLAY_VS_DXBC: &[u8] = include_bytes!("shaders/overlay_vs.dxbc");
const OVERLAY_SPRITE_PS_DXBC: &[u8] = include_bytes!("shaders/overlay_sprite_ps.dxbc");
const OVERLAY_SHAPE_PS_DXBC: &[u8] = include_bytes!("shaders/overlay_shape_ps.dxbc");

/// Frame-level overlay dynamic input (cursor position, click ripples, scale).
#[derive(Debug, Clone, Default)]
pub struct GpuOverlayInput {
    pub cursor: (i32, i32),
    pub clicks: Vec<FrameClick>,
    pub scale: f32,
}

fn create_cursor_arrow_texture(
    device: &ID3D11Device,
) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
    let mut pixels = [0u8; 12 * 19 * 4];
    for (y, row_str) in ARROW.iter().enumerate() {
        let row = row_str.as_bytes();
        for (x, &b) in row.iter().enumerate().take(12) {
            let idx = (y * 12 + x) * 4;
            match b {
                b'X' => {
                    pixels[idx] = 0;
                    pixels[idx + 1] = 0;
                    pixels[idx + 2] = 0;
                    pixels[idx + 3] = 255;
                }
                b'W' => {
                    pixels[idx] = 255;
                    pixels[idx + 1] = 255;
                    pixels[idx + 2] = 255;
                    pixels[idx + 3] = 255;
                }
                _ => {}
            }
        }
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: 12,
        Height: 19,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let subresource = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: 12 * 4,
        SysMemSlicePitch: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, Some(&subresource), Some(&mut tex)) }
        .context("CreateTexture2D(cursor arrow)")?;
    let tex = tex.ok_or_else(|| anyhow!("CreateTexture2D returned None"))?;
    let mut srv = None;
    unsafe { device.CreateShaderResourceView(&tex, None, Some(&mut srv)) }
        .context("CreateShaderResourceView(cursor arrow)")?;
    let srv = srv.ok_or_else(|| anyhow!("CreateShaderResourceView returned None"))?;
    Ok((tex, srv))
}

/// GPU-side compositor for cursor highlight, click ripple rings, custom cursor,
/// and watermark badge directly in VRAM via Direct3D 11 shaders.
pub struct GpuOverlayCompositor {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps_sprite: ID3D11PixelShader,
    ps_shape: ID3D11PixelShader,
    blend: ID3D11BlendState,
    sampler: ID3D11SamplerState,
    cb: ID3D11Buffer,
    scratch: Option<(u32, u32, DXGI_FORMAT, ID3D11Texture2D, ID3D11RenderTargetView)>,
    watermark_tex: Option<(String, u32, u32, ID3D11Texture2D, ID3D11ShaderResourceView)>,
    cursor_tex: Option<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
    watermark_renderer: Option<WatermarkRenderer>,
    pub mouse_fx: MouseFx,
    pub watermark: Watermark,
}

impl GpuOverlayCompositor {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        mouse_fx: MouseFx,
        watermark: Watermark,
    ) -> Result<Self> {
        let mut vs = None;
        unsafe { device.CreateVertexShader(OVERLAY_VS_DXBC, None, Some(&mut vs)) }
            .context("CreateVertexShader(overlay_vs)")?;
        let vs = vs.ok_or_else(|| anyhow!("CreateVertexShader returned None"))?;

        let mut ps_sprite = None;
        unsafe { device.CreatePixelShader(OVERLAY_SPRITE_PS_DXBC, None, Some(&mut ps_sprite)) }
            .context("CreatePixelShader(overlay_sprite_ps)")?;
        let ps_sprite = ps_sprite.ok_or_else(|| anyhow!("CreatePixelShader returned None"))?;

        let mut ps_shape = None;
        unsafe { device.CreatePixelShader(OVERLAY_SHAPE_PS_DXBC, None, Some(&mut ps_shape)) }
            .context("CreatePixelShader(overlay_shape_ps)")?;
        let ps_shape = ps_shape.ok_or_else(|| anyhow!("CreatePixelShader returned None"))?;

        let mut blend_desc = D3D11_BLEND_DESC::default();
        blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D11_BLEND_SRC_ALPHA,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE,
            DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let mut blend = None;
        unsafe { device.CreateBlendState(&blend_desc, Some(&mut blend)) }
            .context("CreateBlendState(overlay)")?;
        let blend = blend.ok_or_else(|| anyhow!("CreateBlendState returned None"))?;

        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler = None;
        unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler)) }
            .context("CreateSamplerState(overlay)")?;
        let sampler = sampler.ok_or_else(|| anyhow!("CreateSamplerState returned None"))?;

        let cb_desc = D3D11_BUFFER_DESC {
            ByteWidth: 64, // 4 * float4
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut cb = None;
        unsafe { device.CreateBuffer(&cb_desc, None, Some(&mut cb)) }
            .context("CreateBuffer(overlay constants)")?;
        let cb = cb.ok_or_else(|| anyhow!("CreateBuffer returned None"))?;

        let watermark_renderer = watermark.any_overlay().then(WatermarkRenderer::new).flatten();

        Ok(Self {
            device: device.clone(),
            context: context.clone(),
            vs,
            ps_sprite,
            ps_shape,
            blend,
            sampler,
            cb,
            scratch: None,
            watermark_tex: None,
            cursor_tex: None,
            watermark_renderer,
            mouse_fx,
            watermark,
        })
    }

    pub fn is_active(&self) -> bool {
        self.mouse_fx.any_overlay() || self.watermark.any_overlay()
    }

    pub fn scratch_texture(&self) -> Option<&ID3D11Texture2D> {
        self.scratch.as_ref().map(|(_, _, _, tex, _)| tex)
    }

    fn ensure_scratch(&mut self, w: u32, h: u32, format: DXGI_FORMAT) -> Result<()> {
        if let Some((sw, sh, sfmt, ..)) = &self.scratch
            && *sw == w && *sh == h && *sfmt == format
        {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .context("CreateTexture2D(scratch)")?;
        let tex = tex.ok_or_else(|| anyhow!("CreateTexture2D returned None"))?;
        let mut rtv = None;
        unsafe { self.device.CreateRenderTargetView(&tex, None, Some(&mut rtv)) }
            .context("CreateRenderTargetView(scratch)")?;
        let rtv = rtv.ok_or_else(|| anyhow!("CreateRenderTargetView returned None"))?;
        self.scratch = Some((w, h, format, tex, rtv));
        Ok(())
    }

    fn ensure_watermark_tex(&mut self, height: u32) -> Result<()> {
        let height = height.max(1);
        if let Some((text, _, h, ..)) = &self.watermark_tex
            && text == &self.watermark.text
            && *h == height
        {
            return Ok(());
        }
        if self.watermark_renderer.is_none() {
            self.watermark_renderer = WatermarkRenderer::new();
        }
        let Some(renderer) = &mut self.watermark_renderer else {
            bail!("WatermarkRenderer unavailable");
        };
        let sprite = renderer.sprite_for(&self.watermark, height);
        let (w, h) = (sprite.width, sprite.height);
        if w == 0 || h == 0 {
            self.watermark_tex = None;
            return Ok(());
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let subresource = D3D11_SUBRESOURCE_DATA {
            pSysMem: sprite.rgba.as_ptr().cast(),
            SysMemPitch: w * 4,
            SysMemSlicePitch: 0,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, Some(&subresource), Some(&mut tex)) }
            .context("CreateTexture2D(watermark)")?;
        let tex = tex.ok_or_else(|| anyhow!("CreateTexture2D returned None"))?;
        let mut srv = None;
        unsafe { self.device.CreateShaderResourceView(&tex, None, Some(&mut srv)) }
            .context("CreateShaderResourceView(watermark)")?;
        let srv = srv.ok_or_else(|| anyhow!("CreateShaderResourceView returned None"))?;
        self.watermark_tex = Some((self.watermark.text.clone(), w, h, tex, srv));
        Ok(())
    }

    fn ensure_cursor_tex(&mut self) -> Result<()> {
        if self.cursor_tex.is_some() {
            return Ok(());
        }
        let (tex, srv) = create_cursor_arrow_texture(&self.device)?;
        self.cursor_tex = Some((tex, srv));
        Ok(())
    }

    // SAFETY: Maps dynamic 64-byte CB0 with WRITE_DISCARD and copies shader parameters.
    unsafe fn update_cb(
        &self,
        rect: [f32; 4],
        center_rad: [f32; 4],
        color: [f32; 4],
        target_size: [f32; 4],
    ) -> Result<()> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: Caller guarantees valid context and constant buffer.
        unsafe {
            self.context
                .Map(&self.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .context("Map(overlay cb)")?;
            let ptr = mapped.pData as *mut f32;
            std::ptr::copy_nonoverlapping(rect.as_ptr(), ptr, 4);
            std::ptr::copy_nonoverlapping(center_rad.as_ptr(), ptr.add(4), 4);
            std::ptr::copy_nonoverlapping(color.as_ptr(), ptr.add(8), 4);
            std::ptr::copy_nonoverlapping(target_size.as_ptr(), ptr.add(12), 4);
            self.context.Unmap(&self.cb, 0);
        }
        Ok(())
    }

    pub fn draw(
        &mut self,
        src: &ID3D11Texture2D,
        src_rect: RECT,
        crop_w: u32,
        crop_h: u32,
        overlay: Option<&GpuOverlayInput>,
    ) -> Result<&ID3D11Texture2D> {
        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: Queries source texture description.
        unsafe { src.GetDesc(&mut src_desc) };
        self.ensure_scratch(crop_w, crop_h, src_desc.Format)?;
        let (_, _, _, scratch_tex, scratch_rtv) = self.scratch.as_ref().unwrap();

        // Copy cropped source region into scratch texture at (0, 0)
        let src_box = D3D11_BOX {
            left: src_rect.left as u32,
            top: src_rect.top as u32,
            front: 0,
            right: src_rect.right as u32,
            bottom: src_rect.bottom as u32,
            back: 1,
        };
        // SAFETY: Copies subresource within valid texture bounds.
        unsafe {
            self.context.CopySubresourceRegion(
                scratch_tex,
                0,
                0,
                0,
                0,
                src,
                0,
                Some(&src_box),
            );
        }

        // Configure pipeline state
        // SAFETY: Sets viewport, render target, blend state, and shaders for overlay pass.
        unsafe {
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: crop_w as f32,
                Height: crop_h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            self.context.RSSetViewports(Some(&[viewport]));
            self.context.RSSetState(None);
            self.context.OMSetRenderTargets(Some(&[Some(scratch_rtv.clone())]), None);
            self.context.OMSetBlendState(&self.blend, Some(&[0.0; 4]), 0xffff_ffff);
            self.context.OMSetDepthStencilState(None, 0);
            self.context.IASetInputLayout(None);
            self.context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            self.context.VSSetShader(&self.vs, None);
            self.context.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            self.context.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            self.context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context.GSSetShader(None, None);
        }

        // Watermark overlay
        if self.watermark.any_overlay() {
            let badge_h = self.watermark.badge_height(crop_h);
            if let Err(e) = self.ensure_watermark_tex(badge_h) {
                log::warn!("failed to ensure watermark texture: {e}");
            } else if let Some((_, bw, bh, _, srv)) = &self.watermark_tex
                && *bw > 0 && *bh > 0
                && let Some((x, y)) = self.watermark.place((*bw, *bh), (crop_w, crop_h))
            {
                let ndc_x = (x as f32 / crop_w as f32) * 2.0 - 1.0;
                let ndc_y = 1.0 - (y as f32 / crop_h as f32) * 2.0;
                let ndc_w = (*bw as f32 / crop_w as f32) * 2.0;
                let ndc_h = -(*bh as f32 / crop_h as f32) * 2.0;
                let opacity = (self.watermark.opacity.min(100) as f32) / 100.0;

                // SAFETY: Updates CB0 and issues 4-vertex draw call for sprite.
                unsafe {
                    self.update_cb(
                        [ndc_x, ndc_y, ndc_w, ndc_h],
                        [0.0, 0.0, 0.0, 0.0],
                        [1.0, 1.0, 1.0, opacity],
                        [crop_w as f32, crop_h as f32, 0.0, 0.0],
                    )?;
                    self.context.PSSetShader(&self.ps_sprite, None);
                    self.context.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
                    self.context.Draw(4, 0);
                }
            }
        }

        // Mouse effects
        if let Some(input) = overlay {
            let cursor = input.cursor;
            let scale = input.scale;

            // Highlight halo
            if self.mouse_fx.highlight && self.mouse_fx.highlight_opacity > 0 {
                let r = HIGHLIGHT_RADIUS * self.mouse_fx.highlight_size as f32 / 100.0 * scale;
                let (cx, cy) = (cursor.0 as f32, cursor.1 as f32);
                let ext = r + 2.0;
                let x0 = (cx - ext).clamp(0.0, crop_w as f32);
                let y0 = (cy - ext).clamp(0.0, crop_h as f32);
                let x1 = (cx + ext).clamp(0.0, crop_w as f32);
                let y1 = (cy + ext).clamp(0.0, crop_h as f32);
                if x1 > x0 && y1 > y0 {
                    let ndc_x = (x0 / crop_w as f32) * 2.0 - 1.0;
                    let ndc_y = 1.0 - (y0 / crop_h as f32) * 2.0;
                    let ndc_w = ((x1 - x0) / crop_w as f32) * 2.0;
                    let ndc_h = -((y1 - y0) / crop_h as f32) * 2.0;
                    let col = self.mouse_fx.highlight_color;
                    let alpha = (self.mouse_fx.highlight_opacity.min(100) as f32) / 100.0;
                    // SAFETY: Updates CB0 and issues 4-vertex draw call for shape shader.
                    unsafe {
                        self.update_cb(
                            [ndc_x, ndc_y, ndc_w, ndc_h],
                            [cx, cy, r, 0.0],
                            [col[0] as f32 / 255.0, col[1] as f32 / 255.0, col[2] as f32 / 255.0, alpha],
                            [crop_w as f32, crop_h as f32, 0.0, 0.0],
                        )?;
                        self.context.PSSetShader(&self.ps_shape, None);
                        self.context.Draw(4, 0);
                    }
                }
            }

            // Click ripples
            if self.mouse_fx.click_effect {
                for &(x, y, age, right) in &input.clicks {
                    let t = age.clamp(0.0, 1.0);
                    let k = self.mouse_fx.click_size as f32 / 100.0 * scale;
                    let r = (RIPPLE_START + (RIPPLE_END - RIPPLE_START) * t) * k;
                    let thickness = RIPPLE_THICKNESS * k.max(0.5);
                    let outer_r = r + thickness * 0.5;
                    let inner_r = (r - thickness * 0.5).max(0.0);
                    let ext = outer_r + 2.0;
                    let x0 = (x as f32 - ext).clamp(0.0, crop_w as f32);
                    let y0 = (y as f32 - ext).clamp(0.0, crop_h as f32);
                    let x1 = (x as f32 + ext).clamp(0.0, crop_w as f32);
                    let y1 = (y as f32 + ext).clamp(0.0, crop_h as f32);
                    if x1 > x0 && y1 > y0 {
                        let ndc_x = (x0 / crop_w as f32) * 2.0 - 1.0;
                        let ndc_y = 1.0 - (y0 / crop_h as f32) * 2.0;
                        let ndc_w = ((x1 - x0) / crop_w as f32) * 2.0;
                        let ndc_h = -((y1 - y0) / crop_h as f32) * 2.0;
                        let col = if right { self.mouse_fx.right_color } else { self.mouse_fx.left_color };
                        let alpha = 0.9 * (1.0 - t);
                        // SAFETY: Updates CB0 and issues 4-vertex draw call for shape shader.
                        unsafe {
                            self.update_cb(
                                [ndc_x, ndc_y, ndc_w, ndc_h],
                                [x as f32, y as f32, outer_r, inner_r],
                                [col[0] as f32 / 255.0, col[1] as f32 / 255.0, col[2] as f32 / 255.0, alpha],
                                [crop_w as f32, crop_h as f32, 0.0, 0.0],
                            )?;
                            self.context.PSSetShader(&self.ps_shape, None);
                            self.context.Draw(4, 0);
                        }
                    }
                }
            }

            // Custom cursor arrow
            if self.mouse_fx.draws_cursor() {
                if let Err(e) = self.ensure_cursor_tex() {
                    log::warn!("failed to ensure cursor arrow texture: {e}");
                } else if let Some((_, srv)) = &self.cursor_tex {
                    let s = (self.mouse_fx.cursor_size as f32 / 100.0 * scale).max(0.25);
                    let w = (12.0 * s).round();
                    let h = (19.0 * s).round();
                    let x0 = cursor.0 as f32;
                    let y0 = cursor.1 as f32;
                    let ndc_x = (x0 / crop_w as f32) * 2.0 - 1.0;
                    let ndc_y = 1.0 - (y0 / crop_h as f32) * 2.0;
                    let ndc_w = (w / crop_w as f32) * 2.0;
                    let ndc_h = -(h / crop_h as f32) * 2.0;
                    // SAFETY: Updates CB0 and issues 4-vertex draw call for cursor sprite.
                    unsafe {
                        self.update_cb(
                            [ndc_x, ndc_y, ndc_w, ndc_h],
                            [0.0, 0.0, 0.0, 0.0],
                            [1.0, 1.0, 1.0, 1.0],
                            [crop_w as f32, crop_h as f32, 0.0, 0.0],
                        )?;
                        self.context.PSSetShader(&self.ps_sprite, None);
                        self.context.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
                        self.context.Draw(4, 0);
                    }
                }
            }
        }

        // Unbind render target and SRV so texture can be read by VideoProcessor
        // SAFETY: Clears OM and PS bindings.
        unsafe {
            self.context.OMSetRenderTargets(None, None);
            self.context.PSSetShaderResources(0, Some(&[None]));
        }

        Ok(&self.scratch.as_ref().unwrap().3)
    }
}

/// Number of NV12 textures kept in the GPU ring pool so async hardware
/// encoders never read a texture while the capture thread overwrites it.
const NV12_POOL_SIZE: usize = 16;

/// Enables D3D11 multithread protection so capture (`ID3D11VideoContext`) and
/// asynchronous Media Foundation transforms (`IMFDXGIDeviceManager`) can share
/// one `ID3D11Device` safely across threads.
pub fn enable_multithread(device: &ID3D11Device) {
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        unsafe { let _ = mt.SetMultithreadProtected(true); };
    }
}

/// Attempts to raise the WDDM GPU scheduling priority of `device` via
/// `IDXGIDevice::SetGPUThreadPriority`. Returns `Ok(actual_priority)` on success
/// or `Err((prev_priority, msg))` when denied (e.g. without `SeIncreaseBasePriorityPrivilege`).
pub fn try_set_gpu_priority(device: &ID3D11Device, priority: i32) -> std::result::Result<i32, (Option<i32>, String)> {
    let dxgi: IDXGIDevice = device.cast().map_err(|e| (None, format!("IDXGIDevice cast: {e}")))?;
    let prev = unsafe { dxgi.GetGPUThreadPriority() }.ok();
    match unsafe { dxgi.SetGPUThreadPriority(priority) } {
        Ok(()) => {
            let now = unsafe { dxgi.GetGPUThreadPriority() }.unwrap_or(priority);
            Ok(now)
        }
        Err(e) => Err((prev, e.to_string())),
    }
}

/// Creates a Media Foundation DXGI device manager bound to `device`.
pub fn create_dxgi_device_manager(device: &ID3D11Device) -> Result<(IMFDXGIDeviceManager, u32)> {
    enable_multithread(device);
    let mut reset_token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    unsafe {
        MFCreateDXGIDeviceManager(&mut reset_token, &mut manager).context("MFCreateDXGIDeviceManager")?;
    }
    let manager = manager.ok_or_else(|| anyhow!("MFCreateDXGIDeviceManager returned None"))?;
    unsafe { manager.ResetDevice(device, reset_token) }.context("IMFDXGIDeviceManager::ResetDevice")?;
    Ok((manager, reset_token))
}

/// One pooled GPU `DXGI_FORMAT_NV12` surface and its cached output view + completion event query.
pub struct GpuNv12Slot {
    pub texture: ID3D11Texture2D,
    vpov: ID3D11VideoProcessorOutputView,
    event_query: Option<ID3D11Query>,
    pending_gpu_work: AtomicBool,
    pub width: u32,
    pub height: u32,
}

impl GpuNv12Slot {
    /// Waits for the `VideoProcessorBlt` queued for this slot to finish on the GPU,
    /// returning the wait duration in microseconds (`0` if already completed or waited once).
    pub fn wait_complete(&self, context: &ID3D11DeviceContext) -> u64 {
        if !self.pending_gpu_work.swap(false, Ordering::AcqRel) {
            return 0;
        }
        let Some(query) = &self.event_query else {
            return 0;
        };
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(2);
        let mut done: i32 = 0;
        let size = std::mem::size_of::<i32>() as u32;
        let mut spins = 0u32;
        loop {
            let res = unsafe {
                context.GetData(
                    query,
                    Some((&mut done as *mut i32).cast()),
                    size,
                    D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
                )
            };
            if res.is_ok() && done != 0 {
                return t0.elapsed().as_micros() as u64;
            }
            if let Err(e) = res {
                log::debug!("D3D11 event query GetData failed: {e}");
                return t0.elapsed().as_micros() as u64;
            }
            if Instant::now() >= deadline {
                log::warn!("D3D11 VideoProcessor event query timed out after 2s");
                return t0.elapsed().as_micros() as u64;
            }
            spins += 1;
            if spins <= 8 {
                std::hint::spin_loop();
            } else if spins <= 24 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_micros(250));
            }
        }
    }
}

// SAFETY: Every `GpuNv12Slot` is created on a device with
// `ID3D11Multithread::SetMultithreadProtected(true)`. COM interfaces on that
// device use AddRef/Release synchronization and D3D11 serializes device calls.
unsafe impl Send for GpuNv12Slot {}
unsafe impl Sync for GpuNv12Slot {}

impl std::fmt::Debug for GpuNv12Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuNv12Slot").field("width", &self.width).field("height", &self.height).finish()
    }
}

/// Builds a `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` bitfield matching the D3D11 SDK
/// layout (`Usage:1 | RGB_Range:1 | YCbCr_Matrix:1 | YCbCr_xvYCC:1 | Nominal_Range:2`).
fn vp_color_space(rgb_limited: bool, bt709: bool, nominal_16_235: bool) -> D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
    let nominal = if nominal_16_235 { 1u32 } else { 2u32 };
    let bits = ((rgb_limited as u32) << 1) | ((bt709 as u32) << 2) | (nominal << 4);
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: bits }
}

fn configure_vp_colorspaces(
    vc: &ID3D11VideoContext,
    vp: &ID3D11VideoProcessor,
    input_yuv: Option<bool>, // None = Full-range RGB709, Some(true) = BT.709 Studio YUV, Some(false) = BT.601 Studio YUV
    output_yuv_bt709: bool,
    force_opaque_alpha: bool,
) {
    unsafe {
        vc.VideoProcessorSetStreamAutoProcessingMode(vp, 0, false);
        if force_opaque_alpha {
            vc.VideoProcessorSetOutputAlphaFillMode(vp, D3D11_VIDEO_PROCESSOR_ALPHA_FILL_MODE_OPAQUE, 0);
        }
        if let Ok(vc1) = vc.cast::<ID3D11VideoContext1>() {
            let in_cs = match input_yuv {
                None => DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                Some(true) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                Some(false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
            };
            let out_cs = if output_yuv_bt709 {
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709
            } else {
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
            };
            vc1.VideoProcessorSetStreamColorSpace1(vp, 0, in_cs);
            vc1.VideoProcessorSetOutputColorSpace1(vp, out_cs);
        } else {
            let in_cs = match input_yuv {
                None => vp_color_space(false, true, false),
                Some(bt709) => vp_color_space(true, bt709, true),
            };
            let out_cs = if output_yuv_bt709 {
                vp_color_space(true, true, true)
            } else {
                vp_color_space(false, true, false)
            };
            vc.VideoProcessorSetStreamColorSpace(vp, 0, &in_cs);
            vc.VideoProcessorSetOutputColorSpace(vp, &out_cs);
        }
    }
}

fn create_vp(
    video_device: &ID3D11VideoDevice,
    in_size: (u32, u32),
    out_size: (u32, u32),
    usage: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_USAGE,
) -> Result<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor)> {
    let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
        InputWidth: in_size.0.max(2),
        InputHeight: in_size.1.max(2),
        OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
        OutputWidth: out_size.0.max(2),
        OutputHeight: out_size.1.max(2),
        Usage: usage,
    };
    let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
        .context("CreateVideoProcessorEnumerator")?;
    let processor =
        unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }.context("CreateVideoProcessor")?;
    Ok((enumerator, processor))
}

fn create_nv12_slot(
    device: &ID3D11Device,
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    w: u32,
    h: u32,
) -> Result<Arc<GpuNv12Slot>> {
    // Prefer RENDER_TARGET | SHADER_RESOURCE | VIDEO_ENCODER; some drivers disallow
    // combining VIDEO_ENCODER with RENDER_TARGET/VideoProcessorOutputView, so fall back
    // cleanly to RENDER_TARGET | SHADER_RESOURCE or RENDER_TARGET alone.
    let candidates = [
        (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_VIDEO_ENCODER).0 as u32,
        (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
        D3D11_BIND_RENDER_TARGET.0 as u32,
    ];
    let mut last_err = None;
    for bind in candidates {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex_opt: Option<ID3D11Texture2D> = None;
        if let Err(e) = unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex_opt)) } {
            last_err = Some(anyhow!("CreateTexture2D(NV12, bind={bind:#x}): {e}"));
            continue;
        }
        let texture = tex_opt.ok_or_else(|| anyhow!("CreateTexture2D returned None"))?;
        let res: ID3D11Resource = texture.cast()?;
        let vpov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut vpov_opt: Option<ID3D11VideoProcessorOutputView> = None;
        if let Err(e) = unsafe { video_device.CreateVideoProcessorOutputView(&res, enumerator, &vpov_desc, Some(&mut vpov_opt)) } {
            last_err = Some(anyhow!("CreateVideoProcessorOutputView(bind={bind:#x}): {e}"));
            continue;
        }
        let vpov = vpov_opt.ok_or_else(|| anyhow!("CreateVideoProcessorOutputView returned None"))?;
        let mut q_opt: Option<ID3D11Query> = None;
        let q_desc = D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 };
        let event_query = unsafe { device.CreateQuery(&q_desc, Some(&mut q_opt)) }.ok().and(q_opt);
        return Ok(Arc::new(GpuNv12Slot {
            texture,
            vpov,
            event_query,
            pending_gpu_work: AtomicBool::new(false),
            width: w,
            height: h,
        }));
    }
    Err(last_err.unwrap_or_else(|| anyhow!("failed to create NV12 GPU surface {w}x{h}")))
}

/// Converts captured `BGRA`/`RGBA` `ID3D11Texture2D` surfaces directly to pooled
/// `DXGI_FORMAT_NV12` `ID3D11Texture2D` surfaces on the GPU via `VideoProcessorBlt`.
pub struct GpuCaptureProcessor {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    in_size: (u32, u32),
    out_size: (u32, u32),
    /// Expected cropped input size from frame 0; deviations trigger letterboxing.
    expected_crop: (u32, u32),
    pool: Vec<Arc<GpuNv12Slot>>,
    cursor: usize,
    // Small RGBA preview resources (allocated lazily only while preview is visible).
    preview_vp: Option<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor, (u32, u32))>,
    preview_tex: Option<(ID3D11Texture2D, ID3D11VideoProcessorOutputView, ID3D11Texture2D, (u32, u32))>,
    pub compositor: Option<GpuOverlayCompositor>,
}

impl GpuCaptureProcessor {
    /// Checks whether `device` exposes `ID3D11VideoDevice` and `ID3D11VideoContext`
    /// and enables multithread protection on it.
    pub fn probe(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Result<(ID3D11VideoDevice, ID3D11VideoContext)> {
        enable_multithread(device);
        let vd: ID3D11VideoDevice = device.cast().context("ID3D11VideoDevice unavailable on capture device")?;
        let vc: ID3D11VideoContext = context.cast().context("ID3D11VideoContext unavailable on capture context")?;
        Ok((vd, vc))
    }

    pub fn with_overlays(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        in_size: (u32, u32),
        expected_crop: (u32, u32),
        out_size: (u32, u32),
        mouse_fx: MouseFx,
        watermark: Watermark,
    ) -> Result<Self> {
        let (video_device, video_context) = Self::probe(&device, &context)?;
        let (enumerator, processor) = create_vp(&video_device, in_size, out_size, D3D11_VIDEO_USAGE_OPTIMAL_SPEED)?;
        configure_vp_colorspaces(&video_context, &processor, None, true, false);
        let black = D3D11_VIDEO_COLOR {
            Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_COLOR_0 {
                YCbCr: D3D11_VIDEO_COLOR_YCbCrA { Y: 16.0 / 255.0, Cb: 0.5, Cr: 0.5, A: 1.0 },
            },
        };
        unsafe { video_context.VideoProcessorSetOutputBackgroundColor(&processor, true, &black) };
        let mut pool = Vec::with_capacity(NV12_POOL_SIZE);
        // Preallocate a few slots up front; grow lazily up to `NV12_POOL_SIZE`.
        for _ in 0..6 {
            pool.push(create_nv12_slot(&device, &video_device, &enumerator, out_size.0, out_size.1)?);
        }
        let compositor = if mouse_fx.any_overlay() || watermark.any_overlay() {
            match GpuOverlayCompositor::new(&device, &context, mouse_fx, watermark) {
                Ok(c) => Some(c),
                Err(e) => {
                    log::warn!("failed to initialize GPU overlay compositor: {e}");
                    None
                }
            }
        } else {
            None
        };
        Ok(Self {
            device,
            context,
            video_device,
            video_context,
            enumerator,
            processor,
            in_size,
            out_size,
            expected_crop,
            pool,
            cursor: 0,
            preview_vp: None,
            preview_tex: None,
            compositor,
        })
    }

    pub fn new(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        in_size: (u32, u32),
        expected_crop: (u32, u32),
        out_size: (u32, u32),
    ) -> Result<Self> {
        Self::with_overlays(
            device,
            context,
            in_size,
            expected_crop,
            out_size,
            MouseFx::default_off(),
            Watermark { enabled: false, ..Default::default() },
        )
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    pub fn out_size(&self) -> (u32, u32) {
        self.out_size
    }

    fn ensure_input_size(&mut self, in_size: (u32, u32)) -> Result<()> {
        if self.in_size == in_size {
            return Ok(());
        }
        let (enumerator, processor) =
            create_vp(&self.video_device, in_size, self.out_size, D3D11_VIDEO_USAGE_OPTIMAL_SPEED)?;
        configure_vp_colorspaces(&self.video_context, &processor, None, true, false);
        let black = D3D11_VIDEO_COLOR {
            Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_COLOR_0 {
                YCbCr: D3D11_VIDEO_COLOR_YCbCrA { Y: 16.0 / 255.0, Cb: 0.5, Cr: 0.5, A: 1.0 },
            },
        };
        unsafe { self.video_context.VideoProcessorSetOutputBackgroundColor(&processor, true, &black) };
        self.enumerator = enumerator;
        self.processor = processor;
        self.in_size = in_size;
        Ok(())
    }

    fn acquire_slot(&mut self) -> Result<Arc<GpuNv12Slot>> {
        let len = self.pool.len();
        for offset in 0..len {
            let idx = (self.cursor + offset) % len;
            if Arc::strong_count(&self.pool[idx]) == 1 {
                self.cursor = (idx + 1) % len;
                return Ok(self.pool[idx].clone());
            }
        }
        if len < NV12_POOL_SIZE {
            let slot =
                create_nv12_slot(&self.device, &self.video_device, &self.enumerator, self.out_size.0, self.out_size.1)?;
            self.pool.push(slot.clone());
            self.cursor = 0;
            return Ok(slot);
        }
        // Pool saturated: spin briefly or clone LRU slot only if needed; returning an error
        // triggers a clean CPU fallback rather than corrupting an in-flight surface.
        bail!("GPU NV12 pool exhausted ({len} surfaces still held)")
    }

    /// Converts `src` (cropped to `src_rect`) into a pooled `NV12` GPU surface
    /// of size `self.out_size` and flushes the context so downstream consumers on
    /// `self.device` see the completed surface.
    pub fn convert(
        &mut self,
        src: &ID3D11Texture2D,
        src_size: (u32, u32),
        src_rect: RECT,
    ) -> Result<Arc<GpuNv12Slot>> {
        let (slot, _) = self.convert_timed(src, src_size, src_rect, None)?;
        Ok(slot)
    }

    /// Converts `src` (cropped to `src_rect`) into a pooled `NV12` GPU surface
    /// of size `self.out_size`, marks an `ID3D11Query` event fence on the surface,
    /// and flushes the context. Returns `(slot, vp_submit_us)`.
    ///
    /// If overlays (watermark, cursor highlight, click ripples) are active in
    /// `self.compositor`, they are rendered directly onto a scratch texture in VRAM
    /// before `VideoProcessorBlt` performs zero-readback conversion to NV12.
    pub fn convert_timed(
        &mut self,
        src: &ID3D11Texture2D,
        src_size: (u32, u32),
        src_rect: RECT,
        overlay: Option<&GpuOverlayInput>,
    ) -> Result<(Arc<GpuNv12Slot>, u64)> {
        let t0 = Instant::now();
        let crop_w = (src_rect.right - src_rect.left).max(1) as u32;
        let crop_h = (src_rect.bottom - src_rect.top).max(1) as u32;

        let (input_tex, effective_src_size, effective_src_rect) =
            if self.compositor.as_ref().is_some_and(|c| c.is_active()) {
                let comp = self.compositor.as_mut().unwrap();
                let comp_tex = comp.draw(src, src_rect, crop_w, crop_h, overlay)?;
                let r = RECT { left: 0, top: 0, right: crop_w as i32, bottom: crop_h as i32 };
                (comp_tex.clone(), (crop_w, crop_h), r)
            } else {
                (src.clone(), src_size, src_rect)
            };

        self.ensure_input_size(effective_src_size)?;
        let slot = self.acquire_slot()?;

        let dst_rect = if (crop_w, crop_h) == self.expected_crop {
            RECT { left: 0, top: 0, right: self.out_size.0 as i32, bottom: self.out_size.1 as i32 }
        } else {
            let (fw, fh) = fit_inside((crop_w, crop_h), self.out_size);
            let ox = ((self.out_size.0 - fw) / 2) & !1;
            let oy = ((self.out_size.1 - fh) / 2) & !1;
            RECT { left: ox as i32, top: oy as i32, right: (ox + fw) as i32, bottom: (oy + fh) as i32 }
        };
        let target_rect = RECT { left: 0, top: 0, right: self.out_size.0 as i32, bottom: self.out_size.1 as i32 };

        let src_res: ID3D11Resource = input_tex.cast()?;
        let iv_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut iv_opt: Option<ID3D11VideoProcessorInputView> = None;
        unsafe {
            self.video_device
                .CreateVideoProcessorInputView(&src_res, &self.enumerator, &iv_desc, Some(&mut iv_opt))
                .context("CreateVideoProcessorInputView")?;
        }
        let iv = iv_opt.ok_or_else(|| anyhow!("CreateVideoProcessorInputView returned None"))?;

        unsafe {
            self.video_context.VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&effective_src_rect));
            self.video_context.VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&dst_rect));
            self.video_context.VideoProcessorSetOutputTargetRect(&self.processor, true, Some(&target_rect));

            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(iv)),
                ..Default::default()
            }];
            let blt_res = self.video_context.VideoProcessorBlt(&self.processor, &slot.vpov, 0, &streams);
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
            blt_res.context("VideoProcessorBlt(BGRA->NV12)")?;
            if let Some(q) = &slot.event_query {
                self.context.End(q);
                slot.pending_gpu_work.store(true, Ordering::Release);
            }
            // Submit queued GPU work before releasing the WGC texture or handing
            // this NV12 surface to Media Foundation's async worker.
            self.context.Flush();
        }
        let vp_submit_us = t0.elapsed().as_micros() as u64;
        Ok((slot, vp_submit_us))
    }

    /// Downscales `src` on GPU to a compact `PreviewImage` (`<= max_side`),
    /// avoiding full-resolution readback while the live preview is shown.
    pub fn render_preview(
        &mut self,
        src: &ID3D11Texture2D,
        src_size: (u32, u32),
        src_rect: RECT,
        max_side: u32,
    ) -> Result<PreviewImage> {
        let crop_w = (src_rect.right - src_rect.left).max(1) as u32;
        let crop_h = (src_rect.bottom - src_rect.top).max(1) as u32;

        let (input_tex, p_size, p_rect) = if let Some(comp) = &self.compositor {
            if comp.is_active() && comp.scratch_texture().is_some() {
                (
                    comp.scratch_texture().unwrap().clone(),
                    (crop_w, crop_h),
                    RECT { left: 0, top: 0, right: crop_w as i32, bottom: crop_h as i32 },
                )
            } else {
                (src.clone(), src_size, src_rect)
            }
        } else {
            (src.clone(), src_size, src_rect)
        };

        let scale = (crop_w.max(crop_h) as f32 / max_side.max(2) as f32).max(1.0);
        let pw = (((crop_w as f32 / scale) as u32).max(2)) & !1;
        let ph = (((crop_h as f32 / scale) as u32).max(2)) & !1;

        if self.preview_tex.as_ref().is_none_or(|(_, _, _, d)| *d != (pw, ph)) {
            let (vp_enum, vp) =
                create_vp(&self.video_device, p_size, (pw, ph), D3D11_VIDEO_USAGE_OPTIMAL_SPEED)?;
            configure_vp_colorspaces(&self.video_context, &vp, None, false, true);
            let out_desc = D3D11_TEXTURE2D_DESC {
                Width: pw,
                Height: ph,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut out_tex = None;
            unsafe { self.device.CreateTexture2D(&out_desc, None, Some(&mut out_tex)) }?;
            let out_tex = out_tex.ok_or_else(|| anyhow!("preview CreateTexture2D returned None"))?;
            let out_res: ID3D11Resource = out_tex.cast()?;
            let vpov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut vpov = None;
            unsafe { self.video_device.CreateVideoProcessorOutputView(&out_res, &vp_enum, &vpov_desc, Some(&mut vpov)) }?;
            let vpov = vpov.ok_or_else(|| anyhow!("preview CreateVideoProcessorOutputView returned None"))?;

            let stg_desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..out_desc
            };
            let mut stg = None;
            unsafe { self.device.CreateTexture2D(&stg_desc, None, Some(&mut stg)) }?;
            let stg = stg.ok_or_else(|| anyhow!("preview staging CreateTexture2D returned None"))?;
            self.preview_vp = Some((vp_enum, vp, p_size));
            self.preview_tex = Some((out_tex, vpov, stg, (pw, ph)));
        } else if self.preview_vp.as_ref().is_none_or(|(_, _, s)| *s != p_size) {
            let (vp_enum, vp) =
                create_vp(&self.video_device, p_size, (pw, ph), D3D11_VIDEO_USAGE_OPTIMAL_SPEED)?;
            configure_vp_colorspaces(&self.video_context, &vp, None, false, true);
            // Recreate output view against new enumerator on input resize.
            let out_tex = self.preview_tex.as_ref().unwrap().0.clone();
            let stg = self.preview_tex.as_ref().unwrap().2.clone();
            let out_res: ID3D11Resource = out_tex.cast()?;
            let vpov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut vpov = None;
            unsafe { self.video_device.CreateVideoProcessorOutputView(&out_res, &vp_enum, &vpov_desc, Some(&mut vpov)) }?;
            let vpov = vpov.ok_or_else(|| anyhow!("preview CreateVideoProcessorOutputView returned None"))?;
            self.preview_vp = Some((vp_enum, vp, p_size));
            self.preview_tex = Some((out_tex, vpov, stg, (pw, ph)));
        }

        let (vp_enum, vp, _) = self.preview_vp.as_ref().unwrap();
        let (out_tex, vpov, stg, _) = self.preview_tex.as_ref().unwrap();
        let src_res: ID3D11Resource = input_tex.cast()?;
        let iv_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut iv = None;
        unsafe { self.video_device.CreateVideoProcessorInputView(&src_res, vp_enum, &iv_desc, Some(&mut iv)) }?;
        let iv = iv.ok_or_else(|| anyhow!("preview input view None"))?;
        let dst_rect = RECT { left: 0, top: 0, right: pw as i32, bottom: ph as i32 };
        let mut rgba = Vec::with_capacity((pw * ph * 4) as usize);
        unsafe {
            self.video_context.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&p_rect));
            self.video_context.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dst_rect));
            self.video_context.VideoProcessorSetOutputTargetRect(vp, true, Some(&dst_rect));
            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(iv)),
                ..Default::default()
            }];
            let blt = self.video_context.VideoProcessorBlt(vp, vpov, 0, &streams);
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
            blt?;
            self.context.CopyResource(stg, out_tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(stg, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let row = (pw * 4) as usize;
            let pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            if pitch == row {
                rgba.extend_from_slice(std::slice::from_raw_parts(base, row * ph as usize));
            } else {
                for y in 0..ph as usize {
                    rgba.extend_from_slice(std::slice::from_raw_parts(base.add(y * pitch), row));
                }
            }
            self.context.Unmap(stg, 0);
        }
        // Force opaque alpha in case a driver writes source alpha instead of OPAQUE fill.
        for px in rgba.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
        Ok(PreviewImage { width: pw, height: ph, rgba })
    }
}

/// One-shot CPU readback of a GPU `DXGI_FORMAT_NV12` texture (used only if the
/// first frame was captured on GPU and the chosen encoder then falls back to CPU).
pub fn readback_nv12_texture(
    device: &ID3D11Device,
    tex: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<(Vec<u8>, Vec<u8>, usize, usize)> {
    let context = unsafe { device.GetImmediateContext() }?;
    let mut src_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut src_desc) };
    let stg_desc = D3D11_TEXTURE2D_DESC {
        Width: src_desc.Width,
        Height: src_desc.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut stg = None;
    unsafe { device.CreateTexture2D(&stg_desc, None, Some(&mut stg)) }?;
    let stg = stg.ok_or_else(|| anyhow!("staging CreateTexture2D returned None"))?;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        context.CopyResource(&stg, tex);
        context.Map(&stg, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    }
    let w = width as usize;
    let h = height as usize;
    let uv_h = h.div_ceil(2);
    let pitch = mapped.RowPitch as usize;
    let mut y_plane = Vec::with_capacity(w * h);
    let mut uv_plane = Vec::with_capacity(w * uv_h);
    unsafe {
        let base = mapped.pData as *const u8;
        for row in 0..h {
            y_plane.extend_from_slice(std::slice::from_raw_parts(base.add(row * pitch), w));
        }
        let uv_base = base.add(src_desc.Height as usize * pitch);
        for row in 0..uv_h {
            uv_plane.extend_from_slice(std::slice::from_raw_parts(uv_base.add(row * pitch), w));
        }
        context.Unmap(&stg, 0);
    }
    Ok((y_plane, uv_plane, w, w))
}

/// Reads back an 8-bit RGBA/BGRA texture from GPU to CPU memory for testing/verification.
pub fn readback_bgra_texture(
    device: &ID3D11Device,
    tex: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    let context = unsafe { device.GetImmediateContext() }?;
    let mut src_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut src_desc) };
    let stg_desc = D3D11_TEXTURE2D_DESC {
        Width: src_desc.Width,
        Height: src_desc.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: src_desc.Format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut stg = None;
    unsafe { device.CreateTexture2D(&stg_desc, None, Some(&mut stg)) }?;
    let stg = stg.ok_or_else(|| anyhow!("staging CreateTexture2D returned None"))?;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        context.CopyResource(&stg, tex);
        context.Map(&stg, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    }
    let w = width as usize;
    let h = height as usize;
    let pitch = mapped.RowPitch as usize;
    let mut out = Vec::with_capacity(w * h * 4);
    unsafe {
        let src_ptr = mapped.pData as *const u8;
        for row in 0..h {
            let row_bytes = std::slice::from_raw_parts(src_ptr.add(row * pitch), w * 4);
            out.extend_from_slice(row_bytes);
        }
        context.Unmap(&stg, 0);
    }
    Ok(out)
}

/// Hardware-accelerated `NV12` -> `R8G8B8A8_UNORM` converter + downscaler for
/// the media viewer (`player/decode.rs`), paired with a D3D11 `IMFDXGIDeviceManager`.
pub struct GpuPlaybackProcessor {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    manager: IMFDXGIDeviceManager,
    enumerator: Option<ID3D11VideoProcessorEnumerator>,
    processor: Option<ID3D11VideoProcessor>,
    out_tex: Option<ID3D11Texture2D>,
    vpov: Option<ID3D11VideoProcessorOutputView>,
    staging: Option<ID3D11Texture2D>,
    cpu_nv12_upload: Option<(ID3D11Texture2D, ID3D11Texture2D, (u32, u32))>,
    in_size: (u32, u32),
    out_size: (u32, u32),
    bt709: bool,
    opaque_checked: bool,
    force_opaque_sw: bool,
}

impl GpuPlaybackProcessor {
    pub fn new() -> Result<Self> {
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut dev = None;
        let mut ctx = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            )
            .context("D3D11CreateDevice(VIDEO_SUPPORT) for playback")?;
        }
        let device = dev.ok_or_else(|| anyhow!("D3D11CreateDevice returned None"))?;
        let context = ctx.ok_or_else(|| anyhow!("D3D11CreateDevice returned None context"))?;
        let (video_device, video_context) = GpuCaptureProcessor::probe(&device, &context)?;
        let (manager, _token) = create_dxgi_device_manager(&device)?;
        Ok(Self {
            device,
            context,
            video_device,
            video_context,
            manager,
            enumerator: None,
            processor: None,
            out_tex: None,
            vpov: None,
            staging: None,
            cpu_nv12_upload: None,
            in_size: (0, 0),
            out_size: (0, 0),
            bt709: true,
            opaque_checked: false,
            force_opaque_sw: false,
        })
    }

    pub fn device_manager(&self) -> &IMFDXGIDeviceManager {
        &self.manager
    }

    fn ensure_pipeline(&mut self, in_size: (u32, u32), out_size: (u32, u32), bt709: bool) -> Result<()> {
        if self.in_size == in_size
            && self.out_size == out_size
            && self.bt709 == bt709
            && self.processor.is_some()
        {
            return Ok(());
        }
        let (enumerator, processor) =
            create_vp(&self.video_device, in_size, out_size, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL)?;
        configure_vp_colorspaces(&self.video_context, &processor, Some(bt709), false, true);
        let black = D3D11_VIDEO_COLOR {
            Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_COLOR_0 {
                RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 },
            },
        };
        unsafe { self.video_context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black) };

        let out_desc = D3D11_TEXTURE2D_DESC {
            Width: out_size.0,
            Height: out_size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut out_tex = None;
        unsafe { self.device.CreateTexture2D(&out_desc, None, Some(&mut out_tex)) }?;
        let out_tex = out_tex.ok_or_else(|| anyhow!("playback CreateTexture2D returned None"))?;
        let out_res: ID3D11Resource = out_tex.cast()?;
        let vpov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut vpov = None;
        unsafe { self.video_device.CreateVideoProcessorOutputView(&out_res, &enumerator, &vpov_desc, Some(&mut vpov)) }?;
        let vpov = vpov.ok_or_else(|| anyhow!("playback CreateVideoProcessorOutputView returned None"))?;

        let stg_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..out_desc
        };
        let mut staging = None;
        unsafe { self.device.CreateTexture2D(&stg_desc, None, Some(&mut staging)) }?;
        let staging = staging.ok_or_else(|| anyhow!("playback staging CreateTexture2D returned None"))?;

        self.enumerator = Some(enumerator);
        self.processor = Some(processor);
        self.out_tex = Some(out_tex);
        self.vpov = Some(vpov);
        self.staging = Some(staging);
        self.in_size = in_size;
        self.out_size = out_size;
        self.bt709 = bt709;
        self.opaque_checked = false;
        self.force_opaque_sw = false;
        Ok(())
    }

    /// Converts a DXVA/D3D11 `NV12` surface (`subresource`) into a tightly-packed
    /// top-down RGBA [`PreviewImage`] of size `out_size`, reusing `rgba`.
    pub fn nv12_to_rgba(
        &mut self,
        src: &ID3D11Texture2D,
        subresource: u32,
        src_rect: RECT,
        out_size: (u32, u32),
        bt709: bool,
        mut rgba: Vec<u8>,
    ) -> Result<PreviewImage> {
        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut src_desc) };
        self.ensure_pipeline((src_desc.Width, src_desc.Height), out_size, bt709)?;

        let enumerator = self.enumerator.as_ref().unwrap();
        let vp = self.processor.as_ref().unwrap();
        let vpov = self.vpov.as_ref().unwrap();
        let out_tex = self.out_tex.as_ref().unwrap();
        let staging = self.staging.as_ref().unwrap();

        let src_res: ID3D11Resource = src.cast()?;
        let iv_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: subresource },
            },
        };
        let mut iv = None;
        unsafe {
            self.video_device
                .CreateVideoProcessorInputView(&src_res, enumerator, &iv_desc, Some(&mut iv))
                .context("playback CreateVideoProcessorInputView")?;
        }
        let iv = iv.ok_or_else(|| anyhow!("playback input view None"))?;
        let dst_rect = RECT { left: 0, top: 0, right: out_size.0 as i32, bottom: out_size.1 as i32 };

        unsafe {
            self.video_context.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&src_rect));
            self.video_context.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dst_rect));
            self.video_context.VideoProcessorSetOutputTargetRect(vp, true, Some(&dst_rect));

            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(iv)),
                ..Default::default()
            }];
            let blt = self.video_context.VideoProcessorBlt(vp, vpov, 0, &streams);
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
            blt.context("playback VideoProcessorBlt(NV12->RGBA)")?;

            self.context.CopyResource(staging, out_tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).context("playback Map staging")?;

            let w = out_size.0 as usize;
            let h = out_size.1 as usize;
            let row = w * 4;
            let pitch = mapped.RowPitch as usize;
            rgba.clear();
            if rgba.capacity() < row * h {
                rgba.reserve(row * h - rgba.capacity());
            }
            let base = mapped.pData as *const u8;
            if pitch == row {
                rgba.extend_from_slice(std::slice::from_raw_parts(base, row * h));
            } else {
                for y in 0..h {
                    rgba.extend_from_slice(std::slice::from_raw_parts(base.add(y * pitch), row));
                }
            }
            self.context.Unmap(staging, 0);
        }

        // Probe alpha fill behavior once per pipeline: if driver wrote alpha=255
        // (via ALPHA_FILL_MODE_OPAQUE), avoid an extra 8 MB per-frame CPU pass.
        if !self.opaque_checked {
            self.opaque_checked = true;
            let mid = rgba.len() / 2;
            let corners_opaque = rgba.get(3).copied() == Some(255)
                && rgba.get((mid & !3) + 3).copied() == Some(255)
                && rgba.last().copied() == Some(255);
            self.force_opaque_sw = !corners_opaque;
            log::debug!("playback GPU VideoProcessor alpha opaque={}", !self.force_opaque_sw);
        }
        if self.force_opaque_sw {
            for px in rgba.as_chunks_mut::<4>().0 {
                px[3] = 255;
            }
        }
        Ok(PreviewImage { width: out_size.0, height: out_size.1, rgba })
    }

    /// Uploads a system-memory `NV12` frame to a reusable `D3D11_BIND_DECODER`
    /// surface and converts/downscales it to RGBA on the GPU.
    #[allow(clippy::too_many_arguments)]
    pub fn nv12_cpu_to_rgba(
        &mut self,
        data: &[u8],
        src_stride: usize,
        coded_size: (u32, u32),
        src_rect: RECT,
        out_size: (u32, u32),
        bt709: bool,
        rgba: Vec<u8>,
    ) -> Result<PreviewImage> {
        let cw = coded_size.0.max(2) & !1;
        let ch = coded_size.1.max(2) & !1;
        if self.cpu_nv12_upload.as_ref().is_none_or(|(_, _, s)| *s != (cw, ch)) {
            let stg_desc = D3D11_TEXTURE2D_DESC {
                Width: cw,
                Height: ch,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                MiscFlags: 0,
            };
            let mut stg = None;
            unsafe { self.device.CreateTexture2D(&stg_desc, None, Some(&mut stg)) }?;
            let stg = stg.ok_or_else(|| anyhow!("playback NV12 staging CreateTexture2D returned None"))?;

            let mut tex = None;
            for bind in [D3D11_BIND_DECODER.0 as u32, D3D11_BIND_RENDER_TARGET.0 as u32, 0u32] {
                let def_desc = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: bind,
                    CPUAccessFlags: 0,
                    ..stg_desc
                };
                if unsafe { self.device.CreateTexture2D(&def_desc, None, Some(&mut tex)) }.is_ok() && tex.is_some() {
                    break;
                }
            }
            let tex = tex.ok_or_else(|| anyhow!("playback NV12 GPU CreateTexture2D failed"))?;
            self.cpu_nv12_upload = Some((tex, stg, (cw, ch)));
        }
        let (tex, stg, _) = self.cpu_nv12_upload.as_ref().unwrap().clone();
        let w = cw as usize;
        let h = ch as usize;
        let uv_h = h / 2;
        let stride = src_stride.max(w);
        let needed = stride * (h + uv_h);
        if data.len() < needed {
            bail!("short CPU NV12 buffer: {} < {needed} bytes ({cw}x{ch}, stride {stride})", data.len());
        }
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&stg, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped)).context("Map NV12 upload")?;
            let dst_pitch = mapped.RowPitch as usize;
            let dst_base = mapped.pData as *mut u8;
            for row in 0..h {
                std::ptr::copy_nonoverlapping(data.as_ptr().add(row * stride), dst_base.add(row * dst_pitch), w);
            }
            let src_uv = data.as_ptr().add(h * stride);
            let dst_uv = dst_base.add(h * dst_pitch);
            for row in 0..uv_h {
                std::ptr::copy_nonoverlapping(src_uv.add(row * stride), dst_uv.add(row * dst_pitch), w);
            }
            self.context.Unmap(&stg, 0);
            self.context.CopyResource(&tex, &stg);
        }
        self.nv12_to_rgba(&tex, 0, src_rect, out_size, bt709, rgba)
    }
}

/// Checks if a surface format is an 8-bit format supported by [`GpuCaptureProcessor`].
pub fn is_supported_capture_format(format: DXGI_FORMAT) -> bool {
    matches!(
        format,
        DXGI_FORMAT_B8G8R8A8_UNORM
            | windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
            | DXGI_FORMAT_R8G8B8A8_UNORM
            | windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM_SRGB
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{Converter, FrameInput, InputLayout, PixelFormat, RawFrame};
    use std::time::Duration;
    use windows::Win32::Graphics::Direct3D11::D3D11_SUBRESOURCE_DATA;

    fn create_wgc_style_device() -> (ID3D11Device, ID3D11DeviceContext) {
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
        (dev.unwrap(), ctx.unwrap())
    }

    #[test]
    fn gpu_bgra_to_nv12_matches_cpu_bt709_colors() {
        let (device, context) = create_wgc_style_device();
        let (w, h) = (64u32, 32u32);
        // 4 vertical color stripes: white, black, pure red, pure blue (BGRA bytes).
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let px = match x / 16 {
                    0 => [255, 255, 255, 255], // White
                    1 => [0, 0, 0, 255],       // Black
                    2 => [0, 0, 255, 255],     // Red (BGRA)
                    _ => [255, 0, 0, 255],     // Blue (BGRA)
                };
                let i = (y * w as usize + x) * 4;
                bgra[i..i + 4].copy_from_slice(&px);
            }
        }

        // Reference CPU BT.709 Limited conversion.
        let mut conv = Converter::new(w, h, InputLayout::Nv12).unwrap();
        conv.convert(&RawFrame {
            data: bgra.clone(),
            width: w,
            height: h,
            stride: w * 4,
            format: PixelFormat::Bgra,
            pts: Duration::ZERO,
            mouse: None,
        })
        .unwrap();
        let FrameInput::Nv12 { y: cpu_y, uv: cpu_uv, strides: cpu_strides, .. } = conv.frame() else {
            panic!("expected Nv12")
        };

        // Upload BGRA into D3D11 texture (just like WGC capture surface).
        let src_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: bgra.as_ptr().cast(), SysMemPitch: w * 4, SysMemSlicePitch: 0 };
        let mut src_tex = None;
        unsafe { device.CreateTexture2D(&src_desc, Some(&init), Some(&mut src_tex)) }.unwrap();
        let src_tex = src_tex.unwrap();

        let Ok(mut vp) = GpuCaptureProcessor::new(device.clone(), context, (w, h), (w, h), (w, h)) else {
            // Headless CI virtual machine without hardware ID3D11VideoDevice support.
            return;
        };
        let slot = vp.convert(&src_tex, (w, h), RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }).unwrap();
        let (gpu_y, gpu_uv, _, _) = readback_nv12_texture(&device, &slot.texture, w, h).unwrap();

        // Sample interior pixels of each 16-px stripe to avoid chroma edge filter.
        let mut max_y_diff = 0i32;
        let mut max_uv_diff = 0i32;
        for stripe in 0..4usize {
            let x = stripe * 16 + 8;
            let cy = cpu_y[4 * cpu_strides.0 + x] as i32;
            let gy = gpu_y[4 * w as usize + x] as i32;
            max_y_diff = max_y_diff.max((cy - gy).abs());

            let uv_idx_c = 2 * cpu_strides.1 + (x & !1);
            let uv_idx_g = 2 * w as usize + (x & !1);
            max_uv_diff = max_uv_diff
                .max((cpu_uv[uv_idx_c] as i32 - gpu_uv[uv_idx_g] as i32).abs())
                .max((cpu_uv[uv_idx_c + 1] as i32 - gpu_uv[uv_idx_g + 1] as i32).abs());
            assert!(
                (cy - gy).abs() <= 3,
                "stripe {stripe} Y mismatch: cpu={cy}, gpu={gy}"
            );
            assert!(
                (cpu_uv[uv_idx_c] as i32 - gpu_uv[uv_idx_g] as i32).abs() <= 4
                    && (cpu_uv[uv_idx_c + 1] as i32 - gpu_uv[uv_idx_g + 1] as i32).abs() <= 4,
                "stripe {stripe} UV mismatch: cpu={:?}, gpu={:?}",
                &cpu_uv[uv_idx_c..uv_idx_c + 2],
                &gpu_uv[uv_idx_g..uv_idx_g + 2]
            );
        }
        eprintln!("BT.709 GPU vs CPU color check: max_y_diff={max_y_diff}, max_uv_diff={max_uv_diff}");

        // Verify GPU NV12 -> RGBA playback processor on an NV12 surface created on `pb.device`.
        let mut pb = GpuPlaybackProcessor::new().unwrap();
        let mut nv12_bytes = Vec::with_capacity(gpu_y.len() + gpu_uv.len());
        nv12_bytes.extend_from_slice(&gpu_y);
        nv12_bytes.extend_from_slice(&gpu_uv);
        // Try D3D11_BIND_DECODER (matches DXVA decoder surfaces) or fallback BindFlags = 0.
        let mut pb_nv12 = None;
        for bind in [
            windows::Win32::Graphics::Direct3D11::D3D11_BIND_DECODER.0 as u32,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            0u32,
        ] {
            let nv12_desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: bind,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let nv12_init = D3D11_SUBRESOURCE_DATA {
                pSysMem: nv12_bytes.as_ptr().cast(),
                SysMemPitch: w,
                SysMemSlicePitch: 0,
            };
            if unsafe { pb.device.CreateTexture2D(&nv12_desc, Some(&nv12_init), Some(&mut pb_nv12)) }.is_ok() {
                break;
            }
        }
        let pb_nv12 = pb_nv12.expect("pb_nv12 texture");
        let pb_rgba = pb
            .nv12_to_rgba(
                &pb_nv12,
                0,
                RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 },
                (32, 16),
                true,
                Vec::new(),
            )
            .unwrap();
        assert_eq!((pb_rgba.width, pb_rgba.height), (32, 16));
        assert!(pb_rgba.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
        // Red stripe (cols 16..23 scaled to 32px width -> col ~20): R high, G low, B low.
        let r_px = &pb_rgba.rgba[(4 * 32 + 20) * 4..][..4];
        assert!(r_px[0] > 235 && r_px[1] < 20 && r_px[2] < 20, "unexpected decoded red {r_px:?}");
    }

    #[test]
    fn gpu_overlay_compositor_renders_overlays_cleanly() {
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        let mut device_opt = None;
        let mut context_opt = None;
        let res = unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device_opt),
                None,
                Some(&mut context_opt),
            )
        };
        let (device, context) = match res {
            Ok(()) => (device_opt.unwrap(), context_opt.unwrap()),
            Err(_) => {
                // Headless CI or machine without D3D11 hardware support
                return;
            }
        };

        let w = 256u32;
        let h = 256u32;

        let mouse_fx = MouseFx {
            highlight: true,
            highlight_color: [255, 255, 0], // Yellow
            highlight_opacity: 50,
            highlight_size: 100,
            click_effect: true,
            left_color: [255, 0, 0], // Red
            right_color: [0, 0, 255],
            click_size: 100,
            show_cursor: true,
            cursor_size: 150, // draws_cursor() = true
        };
        let watermark = Watermark {
            enabled: true,
            position: crate::video::watermark::Corner::TopRight,
            size: 100,
            opacity: 80,
            ..Default::default()
        };

        let mut compositor = match GpuOverlayCompositor::new(&device, &context, mouse_fx, watermark) {
            Ok(c) => c,
            Err(_) => return, // Headless environment where shader creation or features fail
        };
        assert!(compositor.is_active());

        // Create a solid blue BGRA input texture (B=255, G=0, R=0, A=255)
        let bgra: Vec<u8> = (0..w * h).flat_map(|_| [255u8, 0, 0, 255]).collect();
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
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: bgra.as_ptr().cast(),
            SysMemPitch: w * 4,
            SysMemSlicePitch: 0,
        };
        let mut src_tex = None;
        unsafe { device.CreateTexture2D(&src_desc, Some(&init), Some(&mut src_tex)) }.unwrap();
        let src_tex = src_tex.unwrap();

        let overlay_input = GpuOverlayInput {
            cursor: (128, 128),
            clicks: vec![(64, 64, 0.5, false)], // half-aged left click
            scale: 1.0,
        };

        let comp_tex = compositor
            .draw(
                &src_tex,
                RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 },
                w,
                h,
                Some(&overlay_input),
            )
            .unwrap();

        let pixels = readback_bgra_texture(&device, comp_tex, w, h).unwrap();
        assert_eq!(pixels.len(), (w * h * 4) as usize);

        // Highlight halo check at (148, 128) - 20px offset from center (inside radius 32, outside arrow):
        // Background was Blue (B=255, G=0, R=0).
        // Yellow is (R=255, G=255, B=0) blended at 50% opacity.
        // So R should be > 80, G should be > 80!
        let idx = (128 * w as usize + 148) * 4;
        let b = pixels[idx];
        let g = pixels[idx + 1];
        let r = pixels[idx + 2];
        assert!(r > 80 && g > 80, "cursor highlight not blended at (148, 128): R={r}, G={g}, B={b}");

        // Untouched corner at (10, 240) should be untouched blue:
        let corner_idx = (240 * w as usize + 10) * 4;
        assert_eq!(pixels[corner_idx], 255, "untouched corner B mismatch");
        assert_eq!(pixels[corner_idx + 1], 0, "untouched corner G mismatch");
        assert_eq!(pixels[corner_idx + 2], 0, "untouched corner R mismatch");

        // Also test GpuCaptureProcessor::with_overlays with full conversion to NV12:
        if let Ok(mut vp) = GpuCaptureProcessor::with_overlays(
            device.clone(),
            context.clone(),
            (w, h),
            (w, h),
            (w, h),
            compositor.mouse_fx.clone(),
            compositor.watermark.clone(),
        ) {
            let (slot, _) = vp
                .convert_timed(
                    &src_tex,
                    (w, h),
                    RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 },
                    Some(&overlay_input),
                )
                .unwrap();
            let (gpu_y, gpu_uv, _, _) = readback_nv12_texture(&device, &slot.texture, w, h).unwrap();
            assert_eq!(gpu_y.len(), (w * h) as usize);
            assert_eq!(gpu_uv.len(), (w * (h / 2)) as usize);
        }
    }
}
