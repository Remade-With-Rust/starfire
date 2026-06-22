// SPDX-License-Identifier: Apache-2.0
//! Windows zero-copy present via native **D3D11**.
//!
//! # Clean-room provenance
//! Direct, first-party bindings to **D3D11** + **DXGI** via the MIT/Apache
//! `windows` crate, and a first-party HLSL YUV→RGB shader. No Moonlight, no
//! ffmpeg, no third-party renderer.
//!
//! The Media Foundation decoder hands up a decoded NV12 frame as an
//! `ID3D11Texture2D` on the **shared** decode/render device
//! ([`starfire_decode::win_device::SharedDevice`]). We create two SRVs over it
//! (Y = R8, CbCr = R8G8), run a BT.709 shader, and present to a DXGI flip-model
//! swapchain on the winit `HWND` — no CPU readback, no GPU re-upload.

use std::ffi::c_void;

use starfire_decode::win_device::SharedDevice;
use starfire_decode::VideoFrame;
use windows::core::{s, Interface};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
    D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SAMPLER_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_RTV, D3D11_TEX2D_SRV, D3D11_TEXTURE_ADDRESS_CLAMP,
    D3D11_VIEWPORT, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_RENDER_TARGET_VIEW_DESC,
    D3D11_RENDER_TARGET_VIEW_DESC_0,
};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_UNSPECIFIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
    DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::{ColorMode, RenderError, Renderer};

/// First-party HLSL: fullscreen triangle + NV12 → RGB (BT.709 limited range).
const SHADER_HLSL: &str = r#"
Texture2D<float>  YTex  : register(t0);
Texture2D<float2> UVTex : register(t1);
SamplerState Samp : register(s0);

struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

VSOut VSMain(uint vid : SV_VertexID) {
    VSOut o;
    float2 uv = float2((vid << 1) & 2, vid & 2);
    o.uv = uv;
    o.pos = float4(uv * 2.0 - 1.0, 0.0, 1.0);
    o.pos.y = -o.pos.y;
    return o;
}

float4 PSMain(VSOut i) : SV_Target {
    float  y  = YTex.Sample(Samp, i.uv).r;
    float2 cc = UVTex.Sample(Samp, i.uv).rg;
    float yf = (y      - 16.0/255.0)  * (255.0/219.0);
    float uf = (cc.x   - 128.0/255.0) * (255.0/224.0);
    float vf = (cc.y   - 128.0/255.0) * (255.0/224.0);
    float r = yf + 1.5748*vf;
    float g = yf - 0.1873*uf - 0.4681*vf;
    float b = yf + 1.8556*uf;
    return float4(saturate(float3(r,g,b)), 1.0);
}
"#;

fn err(msg: impl Into<String>) -> RenderError {
    RenderError::Failed(msg.into())
}
fn werr(e: windows::core::Error) -> RenderError {
    RenderError::Failed(format!("d3d11: {e}"))
}

/// Keeps a presented frame's texture + SRVs alive one extra present (the GPU
/// finishes sampling before we release).
type Hold = (
    ID3D11Texture2D,
    ID3D11ShaderResourceView,
    ID3D11ShaderResourceView,
);

/// Native D3D11 renderer presenting zero-copy NV12 frames to a DXGI swapchain.
pub struct D3d11Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swapchain: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    width: u32,
    height: u32,
    sync_interval: u32,
    color_mode: ColorMode,
    last_hold: Option<Hold>,
    prev_hold: Option<Hold>,
}

impl D3d11Renderer {
    /// Build a renderer on the shared decode device, bound to `window`'s `HWND`.
    /// `vsync` false → immediate present (lowest latency).
    pub fn new<W: HasWindowHandle>(
        window: &W,
        shared: SharedDevice,
        width: u32,
        height: u32,
        vsync: bool,
    ) -> Result<Self, RenderError> {
        let hwnd = win32_hwnd(window)?;
        let device = shared.device;
        let context = shared.context;
        unsafe {
            // DXGI factory from our device, then a flip-model swapchain on the HWND.
            let dxgi_dev: IDXGIDevice = device.cast().map_err(werr)?;
            let adapter: IDXGIAdapter = dxgi_dev.GetAdapter().map_err(werr)?;
            let factory: IDXGIFactory2 = adapter.GetParent().map_err(werr)?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width.max(1),
                Height: height.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
                Flags: 0,
            };
            let swapchain = factory
                .CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)
                .map_err(werr)?;

            let (vs, ps) = compile_shaders(&device)?;
            let samp_desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler: Option<ID3D11SamplerState> = None;
            device
                .CreateSamplerState(&samp_desc, Some(&mut sampler))
                .map_err(werr)?;

            Ok(Self {
                device,
                context,
                swapchain,
                rtv: None,
                vs,
                ps,
                sampler: sampler.ok_or_else(|| err("no sampler"))?,
                width: width.max(1),
                height: height.max(1),
                sync_interval: if vsync { 1 } else { 0 },
                color_mode: ColorMode::Bt709Sdr,
                last_hold: None,
                prev_hold: None,
            })
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.rtv = None; // released; recreated against the resized backbuffer
        unsafe {
            let _ = self.swapchain.ResizeBuffers(
                0,
                self.width,
                self.height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            );
        }
    }

    /// (Re)create the sRGB render-target view over the current backbuffer.
    unsafe fn ensure_rtv(&mut self) -> Result<ID3D11RenderTargetView, RenderError> {
        if let Some(rtv) = &self.rtv {
            return Ok(rtv.clone());
        }
        let backbuffer: ID3D11Texture2D = self.swapchain.GetBuffer(0).map_err(werr)?;
        let rtv_desc = D3D11_RENDER_TARGET_VIEW_DESC {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
            },
        };
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        self.device
            .CreateRenderTargetView(&backbuffer, Some(&rtv_desc), Some(&mut rtv))
            .map_err(werr)?;
        let rtv = rtv.ok_or_else(|| err("no rtv"))?;
        self.rtv = Some(rtv.clone());
        Ok(rtv)
    }

    /// Build an SRV over one NV12 plane (Y = R8, CbCr = R8G8).
    unsafe fn plane_srv(
        &self,
        tex: &ID3D11Texture2D,
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    ) -> Result<ID3D11ShaderResourceView, RenderError> {
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        self.device
            .CreateShaderResourceView(tex, Some(&desc), Some(&mut srv))
            .map_err(werr)?;
        srv.ok_or_else(|| err("no srv"))
    }
}

impl Renderer for D3d11Renderer {
    fn set_color_mode(&mut self, mode: ColorMode) {
        self.color_mode = mode;
    }

    fn present(&mut self, frame: &VideoFrame) -> Result<(), RenderError> {
        let Some(tex) = frame.native_d3d11.as_ref().map(|f| f.texture.clone()) else {
            return Ok(()); // not a zero-copy frame — nothing for the D3D11 path
        };
        unsafe {
            let y_srv = self.plane_srv(&tex, DXGI_FORMAT_R8_UNORM)?;
            let uv_srv = self.plane_srv(&tex, DXGI_FORMAT_R8G8_UNORM)?;
            let rtv = self.ensure_rtv()?;
            let ctx = &self.context;

            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.width as f32,
                Height: self.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.OMSetRenderTargets(Some(&[Some(rtv)]), None);
            ctx.RSSetViewports(Some(&[viewport]));
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.PSSetShaderResources(0, Some(&[Some(y_srv.clone()), Some(uv_srv.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.Draw(3, 0);

            self.swapchain
                .Present(self.sync_interval, DXGI_PRESENT(0))
                .ok()
                .map_err(werr)?;

            // GPU-completion holders: release resources two frames old.
            self.prev_hold = self.last_hold.take();
            self.last_hold = Some((tex, y_srv, uv_srv));
        }
        Ok(())
    }
}

/// Pull the `HWND` from a winit window handle (Win32).
fn win32_hwnd<W: HasWindowHandle>(window: &W) -> Result<HWND, RenderError> {
    let handle = window
        .window_handle()
        .map_err(|e| err(format!("window_handle: {e}")))?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Ok(HWND(h.hwnd.get() as *mut c_void)),
        other => Err(err(format!("expected Win32 window handle, got {other:?}"))),
    }
}

/// Compile + create the vertex and pixel shaders.
unsafe fn compile_shaders(
    device: &ID3D11Device,
) -> Result<(ID3D11VertexShader, ID3D11PixelShader), RenderError> {
    let vs_blob = compile(SHADER_HLSL, s!("VSMain"), s!("vs_5_0"))?;
    let ps_blob = compile(SHADER_HLSL, s!("PSMain"), s!("ps_5_0"))?;
    let vs_bytes =
        std::slice::from_raw_parts(vs_blob.GetBufferPointer() as *const u8, vs_blob.GetBufferSize());
    let ps_bytes =
        std::slice::from_raw_parts(ps_blob.GetBufferPointer() as *const u8, ps_blob.GetBufferSize());
    let mut vs: Option<ID3D11VertexShader> = None;
    let mut ps: Option<ID3D11PixelShader> = None;
    device
        .CreateVertexShader(vs_bytes, None, Some(&mut vs))
        .map_err(werr)?;
    device
        .CreatePixelShader(ps_bytes, None, Some(&mut ps))
        .map_err(werr)?;
    Ok((
        vs.ok_or_else(|| err("no vs"))?,
        ps.ok_or_else(|| err("no ps"))?,
    ))
}

unsafe fn compile(
    src: &str,
    entry: windows::core::PCSTR,
    target: windows::core::PCSTR,
) -> Result<ID3DBlob, RenderError> {
    let mut blob: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let r = D3DCompile(
        src.as_ptr() as *const c_void,
        src.len(),
        None,
        None,
        None,
        entry,
        target,
        0,
        0,
        &mut blob,
        Some(&mut errors),
    );
    if r.is_err() {
        let msg = errors
            .as_ref()
            .map(|e| {
                let s = std::slice::from_raw_parts(
                    e.GetBufferPointer() as *const u8,
                    e.GetBufferSize(),
                );
                String::from_utf8_lossy(s).into_owned()
            })
            .unwrap_or_default();
        return Err(err(format!("HLSL compile failed: {msg}")));
    }
    blob.ok_or_else(|| err("no shader blob"))
}
