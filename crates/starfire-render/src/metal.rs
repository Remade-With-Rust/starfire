// SPDX-License-Identifier: Apache-2.0
//! macOS zero-copy present via **Metal** + **CVMetalTextureCache**.
//!
//! # Clean-room provenance
//! Direct, first-party bindings to Apple's public frameworks — **Metal**,
//! **QuartzCore** (`CAMetalLayer`), **CoreVideo** (`CVMetalTextureCache`),
//! **AppKit** (`NSView`) — via raw `#[link] extern "C"` + the Objective-C runtime
//! (`objc_msgSend`), exactly the style of the VideoToolbox decode backend and the
//! `keep_awake` helper. No third-party Metal binding crate, no Moonlight source,
//! and a first-party MSL shader (`yuv.metal`).
//!
//! # What it does
//! The VideoToolbox decoder hands up a retained, IOSurface-backed `CVPixelBuffer`
//! (NV12) in [`VideoFrame::native`]. Per present we wrap its two planes as Metal
//! textures through a `CVMetalTextureCache` (no CPU copy, no GPU re-upload), run a
//! BT.709 YUV→RGB shader, and present to a `CAMetalLayer` drawable attached to the
//! winit window's `NSView`.
//!
//! Threading: all Metal/CAMetalLayer/NSView calls happen on the main (winit)
//! thread — the renderer is built in `resumed()` and `present()` runs in
//! `RedrawRequested`. The `CVPixelBuffer` is produced on the VT thread but only
//! retained there and sampled here; CoreFoundation refcounting is atomic.
#![allow(non_upper_case_globals, non_snake_case)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::mem;

use starfire_decode::VideoFrame;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::{ColorMode, RenderError, Renderer};

// ---------------------------------------------------------------------------
// Objective-C runtime + framework FFI
// ---------------------------------------------------------------------------

type Id = *mut c_void;
type Sel = *const c_void;
type CVPixelBufferRef = *const c_void;
type CVMetalTextureRef = *const c_void;
type CVMetalTextureCacheRef = *const c_void;
type CVReturn = i32;

#[link(name = "objc", kind = "dylib")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

#[link(name = "Metal", kind = "framework")]
extern "C" {
    fn MTLCreateSystemDefaultDevice() -> Id;
}

// Ensure these frameworks are loaded so their classes (CAMetalLayer) and symbols
// resolve at runtime; the actual calls go through objc_msgSend / CoreVideo.
#[link(name = "QuartzCore", kind = "framework")]
extern "C" {}
#[link(name = "AppKit", kind = "framework")]
extern "C" {}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVMetalTextureCacheCreate(
        allocator: *const c_void,
        cacheAttributes: *const c_void,
        metalDevice: Id,
        textureAttributes: *const c_void,
        cacheOut: *mut CVMetalTextureCacheRef,
    ) -> CVReturn;
    fn CVMetalTextureCacheCreateTextureFromImage(
        allocator: *const c_void,
        textureCache: CVMetalTextureCacheRef,
        sourceImage: CVPixelBufferRef,
        textureAttributes: *const c_void,
        pixelFormat: u64,
        width: usize,
        height: usize,
        planeIndex: usize,
        textureOut: *mut CVMetalTextureRef,
    ) -> CVReturn;
    fn CVMetalTextureGetTexture(image: CVMetalTextureRef) -> Id;
    fn CVMetalTextureCacheFlush(textureCache: CVMetalTextureCacheRef, options: u64);
}

// MTLPixelFormat values.
const MTL_PF_R8_UNORM: u64 = 10;
const MTL_PF_RG8_UNORM: u64 = 30;
const MTL_PF_BGRA8_UNORM_SRGB: u64 = 81;
// MTLLoadAction::DontCare / MTLStoreAction::Store.
const MTL_LOAD_DONTCARE: u64 = 0;
const MTL_STORE_STORE: u64 = 1;
// MTLPrimitiveType::Triangle.
const MTL_PRIM_TRIANGLE: u64 = 3;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

/// Uniform handed to the fragment shader (mirrors `Params` in `yuv.metal`).
#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    format: u32,
    full_range: u32,
    pad0: u32,
    pad1: u32,
}

// --- objc_msgSend typed shims (transmute per arity/signature) ---------------

#[inline]
unsafe fn class(name: &CStr) -> Id {
    objc_getClass(name.as_ptr())
}
#[inline]
unsafe fn sel(name: &CStr) -> Sel {
    sel_registerName(name.as_ptr())
}
#[inline]
unsafe fn msg(obj: Id, s: Sel) -> Id {
    let f: extern "C" fn(Id, Sel) -> Id = mem::transmute(objc_msgSend as *const ());
    f(obj, s)
}
#[inline]
unsafe fn msg_ret_f64(obj: Id, s: Sel) -> f64 {
    let f: extern "C" fn(Id, Sel) -> f64 = mem::transmute(objc_msgSend as *const ());
    f(obj, s)
}
#[inline]
unsafe fn msg_id(obj: Id, s: Sel, a: Id) -> Id {
    let f: extern "C" fn(Id, Sel, Id) -> Id = mem::transmute(objc_msgSend as *const ());
    f(obj, s, a)
}
#[inline]
unsafe fn msg_void_id(obj: Id, s: Sel, a: Id) {
    let f: extern "C" fn(Id, Sel, Id) = mem::transmute(objc_msgSend as *const ());
    f(obj, s, a)
}
#[inline]
unsafe fn msg_u64(obj: Id, s: Sel, a: u64) -> Id {
    let f: extern "C" fn(Id, Sel, u64) -> Id = mem::transmute(objc_msgSend as *const ());
    f(obj, s, a)
}
#[inline]
unsafe fn msg_void_u64(obj: Id, s: Sel, a: u64) {
    let f: extern "C" fn(Id, Sel, u64) = mem::transmute(objc_msgSend as *const ());
    f(obj, s, a)
}
#[inline]
unsafe fn msg_void_bool(obj: Id, s: Sel, a: bool) {
    let f: extern "C" fn(Id, Sel, bool) = mem::transmute(objc_msgSend as *const ());
    f(obj, s, a)
}
#[inline]
unsafe fn msg_void_f64(obj: Id, s: Sel, a: f64) {
    let f: extern "C" fn(Id, Sel, f64) = mem::transmute(objc_msgSend as *const ());
    f(obj, s, a)
}
#[inline]
unsafe fn msg_void_size(obj: Id, s: Sel, a: CGSize) {
    let f: extern "C" fn(Id, Sel, CGSize) = mem::transmute(objc_msgSend as *const ());
    f(obj, s, a)
}
#[inline]
unsafe fn msg_str(obj: Id, s: Sel, a: *const c_char) -> Id {
    let f: extern "C" fn(Id, Sel, *const c_char) -> Id = mem::transmute(objc_msgSend as *const ());
    f(obj, s, a)
}
/// `[NSString stringWithUTF8String:]` (autoreleased).
unsafe fn nsstring(s: &str) -> Id {
    let c = CString::new(s).unwrap_or_default();
    msg_str(
        class(c"NSString"),
        sel(c"stringWithUTF8String:"),
        c.as_ptr(),
    )
}

// ---------------------------------------------------------------------------
// Per-frame GPU resource holder
// ---------------------------------------------------------------------------

/// Keeps the surface + its Metal textures alive until the GPU has finished
/// sampling them. We hold the *previous* present's resources for one extra frame
/// (double-buffered) before releasing — by which point that command buffer has
/// completed under normal pacing.
struct FrameHold {
    pb: CVPixelBufferRef,
    tex_y: CVMetalTextureRef,
    tex_cbcr: CVMetalTextureRef,
}

impl Drop for FrameHold {
    fn drop(&mut self) {
        unsafe {
            if !self.tex_y.is_null() {
                CFRelease(self.tex_y);
            }
            if !self.tex_cbcr.is_null() {
                CFRelease(self.tex_cbcr);
            }
            if !self.pb.is_null() {
                CFRelease(self.pb);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

/// Native Metal renderer presenting zero-copy NV12 surfaces to a `CAMetalLayer`.
pub struct MetalRenderer {
    device: Id,
    queue: Id,
    pipeline: Id,
    layer: Id,
    cache: CVMetalTextureCacheRef,
    color_mode: ColorMode,
    /// Previous present's resources, released one frame later (GPU-completion).
    last_hold: Option<FrameHold>,
    prev_hold: Option<FrameHold>,
}

// The Metal/CA objects are only ever touched on the main thread (the renderer is
// constructed and presented from the winit event loop), satisfying the Renderer
// seam without making the raw pointers structurally Send/Sync across threads.

impl MetalRenderer {
    /// Build a renderer attached to `window`'s `NSView`. `(width, height)` is the
    /// initial drawable size in physical pixels. `vsync` toggles tear-free
    /// presentation (false = immediate, lowest latency).
    pub fn new<W>(window: &W, width: u32, height: u32, vsync: bool) -> Result<Self, RenderError>
    where
        W: HasWindowHandle,
    {
        let ns_view = appkit_ns_view(window)?;
        // SAFETY: standard objc/Metal setup, all on the main thread.
        unsafe {
            let pool = objc_autoreleasePoolPush();
            let result = Self::new_inner(ns_view, width, height, vsync);
            objc_autoreleasePoolPop(pool);
            result
        }
    }

    unsafe fn new_inner(
        ns_view: Id,
        width: u32,
        height: u32,
        vsync: bool,
    ) -> Result<Self, RenderError> {
        let device = MTLCreateSystemDefaultDevice();
        if device.is_null() {
            return Err(RenderError::Failed("no Metal device".into()));
        }
        let queue = msg(device, sel(c"newCommandQueue"));
        if queue.is_null() {
            return Err(RenderError::Failed("newCommandQueue failed".into()));
        }

        // Compile the first-party MSL shader at runtime (nil options).
        let src = nsstring(include_str!("yuv.metal"));
        let mut err: Id = std::ptr::null_mut();
        let library = {
            let f: extern "C" fn(Id, Sel, Id, Id, *mut Id) -> Id =
                mem::transmute(objc_msgSend as *const ());
            f(
                device,
                sel(c"newLibraryWithSource:options:error:"),
                src,
                std::ptr::null_mut(),
                &mut err,
            )
        };
        if library.is_null() {
            return Err(RenderError::Failed("newLibraryWithSource failed".into()));
        }
        let vs = msg_id(library, sel(c"newFunctionWithName:"), nsstring("vs_main"));
        let fs = msg_id(library, sel(c"newFunctionWithName:"), nsstring("fs_main"));
        if vs.is_null() || fs.is_null() {
            return Err(RenderError::Failed("shader function missing".into()));
        }

        // Render pipeline: vs/fs + the sRGB color attachment format.
        let desc = msg(
            msg(class(c"MTLRenderPipelineDescriptor"), sel(c"alloc")),
            sel(c"init"),
        );
        msg_void_id(desc, sel(c"setVertexFunction:"), vs);
        msg_void_id(desc, sel(c"setFragmentFunction:"), fs);
        let color_attachments = msg(desc, sel(c"colorAttachments"));
        let ca0 = msg_u64(color_attachments, sel(c"objectAtIndexedSubscript:"), 0);
        msg_void_u64(ca0, sel(c"setPixelFormat:"), MTL_PF_BGRA8_UNORM_SRGB);

        let mut err2: Id = std::ptr::null_mut();
        let pipeline = {
            let f: extern "C" fn(Id, Sel, Id, *mut Id) -> Id =
                mem::transmute(objc_msgSend as *const ());
            f(
                device,
                sel(c"newRenderPipelineStateWithDescriptor:error:"),
                desc,
                &mut err2,
            )
        };
        if pipeline.is_null() {
            return Err(RenderError::Failed(
                "newRenderPipelineStateWithDescriptor failed".into(),
            ));
        }

        // CVMetalTextureCache bound to our device.
        let mut cache: CVMetalTextureCacheRef = std::ptr::null();
        let cv = CVMetalTextureCacheCreate(
            std::ptr::null(),
            std::ptr::null(),
            device,
            std::ptr::null(),
            &mut cache,
        );
        if cv != 0 || cache.is_null() {
            return Err(RenderError::Failed(format!(
                "CVMetalTextureCacheCreate failed: {cv}"
            )));
        }

        // CAMetalLayer, attached to the NSView.
        let layer = msg(
            msg(class(c"CAMetalLayer"), sel(c"alloc")),
            sel(c"init"),
        );
        msg_void_id(layer, sel(c"setDevice:"), device);
        msg_void_u64(layer, sel(c"setPixelFormat:"), MTL_PF_BGRA8_UNORM_SRGB);
        msg_void_bool(layer, sel(c"setFramebufferOnly:"), true);
        msg_void_u64(layer, sel(c"setMaximumDrawableCount:"), 3);
        msg_void_bool(layer, sel(c"setDisplaySyncEnabled:"), vsync);
        msg_void_bool(layer, sel(c"setPresentsWithTransaction:"), false);
        msg_void_size(
            layer,
            sel(c"setDrawableSize:"),
            CGSize {
                width: width as f64,
                height: height as f64,
            },
        );

        // Attach: make the NSView layer-hosting and install our Metal layer.
        let ns_window = msg(ns_view, sel(c"window"));
        if !ns_window.is_null() {
            let scale = msg_ret_f64(ns_window, sel(c"backingScaleFactor"));
            if scale > 0.0 {
                msg_void_f64(layer, sel(c"setContentsScale:"), scale);
            }
        }
        msg_void_bool(ns_view, sel(c"setWantsLayer:"), true);
        msg_void_id(ns_view, sel(c"setLayer:"), layer);
        // Keep our own +1 on the layer for the renderer's lifetime.
        let _ = CFRetain(layer);

        Ok(Self {
            device,
            queue,
            pipeline,
            layer,
            cache,
            color_mode: ColorMode::Bt709Sdr,
            last_hold: None,
            prev_hold: None,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        unsafe {
            msg_void_size(
                self.layer,
                sel(c"setDrawableSize:"),
                CGSize {
                    width: width.max(1) as f64,
                    height: height.max(1) as f64,
                },
            );
        }
    }

    /// Wrap one NV12 plane as a Metal texture via the cache. Returns the
    /// `CVMetalTextureRef` (owned, +1) and its `MTLTexture` (borrowed from it).
    unsafe fn plane_texture(
        &self,
        pb: CVPixelBufferRef,
        pf: u64,
        w: usize,
        h: usize,
        plane: usize,
    ) -> Option<(CVMetalTextureRef, Id)> {
        let mut tex: CVMetalTextureRef = std::ptr::null();
        let r = CVMetalTextureCacheCreateTextureFromImage(
            std::ptr::null(),
            self.cache,
            pb,
            std::ptr::null(),
            pf,
            w,
            h,
            plane,
            &mut tex,
        );
        if r != 0 || tex.is_null() {
            return None;
        }
        let mtl = CVMetalTextureGetTexture(tex);
        if mtl.is_null() {
            CFRelease(tex);
            return None;
        }
        Some((tex, mtl))
    }
}

impl Renderer for MetalRenderer {
    fn set_color_mode(&mut self, mode: ColorMode) {
        self.color_mode = mode;
    }

    fn present(&mut self, frame: &VideoFrame) -> Result<(), RenderError> {
        let Some(pb) = frame.native.as_ref().map(|s| s.as_ptr()) else {
            // Not a zero-copy frame — nothing for the Metal path to draw.
            return Ok(());
        };
        let (w, h) = (frame.width as usize, frame.height as usize);
        let (cw, ch) = ((w + 1) / 2, (h + 1) / 2);
        let full_range = matches!(frame.color_space, starfire_decode::ColorSpace::Bt709Full);

        // SAFETY: all Metal calls on the main thread; pointers checked for null.
        unsafe {
            let pool = objc_autoreleasePoolPush();
            let res = (|| {
                let Some((tex_y, mtl_y)) = self.plane_texture(pb, MTL_PF_R8_UNORM, w, h, 0) else {
                    return Err(RenderError::Failed("Y plane texture import failed".into()));
                };
                let Some((tex_cbcr, mtl_cbcr)) =
                    self.plane_texture(pb, MTL_PF_RG8_UNORM, cw, ch, 1)
                else {
                    CFRelease(tex_y);
                    return Err(RenderError::Failed("CbCr plane texture import failed".into()));
                };

                let drawable = msg(self.layer, sel(c"nextDrawable"));
                if drawable.is_null() {
                    // Drawable pool exhausted — drop this frame rather than block.
                    CFRelease(tex_y);
                    CFRelease(tex_cbcr);
                    return Ok(());
                }
                let dst = msg(drawable, sel(c"texture"));

                // Render pass → draw fullscreen triangle.
                let rpd = msg(
                    class(c"MTLRenderPassDescriptor"),
                    sel(c"renderPassDescriptor"),
                );
                let rca = msg_u64(
                    msg(rpd, sel(c"colorAttachments")),
                    sel(c"objectAtIndexedSubscript:"),
                    0,
                );
                msg_void_id(rca, sel(c"setTexture:"), dst);
                msg_void_u64(rca, sel(c"setLoadAction:"), MTL_LOAD_DONTCARE);
                msg_void_u64(rca, sel(c"setStoreAction:"), MTL_STORE_STORE);

                let cb = msg(self.queue, sel(c"commandBuffer"));
                let enc = msg_id(cb, sel(c"renderCommandEncoderWithDescriptor:"), rpd);
                msg_void_id(enc, sel(c"setRenderPipelineState:"), self.pipeline);
                // setFragmentTexture:atIndex:
                let set_tex: extern "C" fn(Id, Sel, Id, u64) =
                    mem::transmute(objc_msgSend as *const ());
                set_tex(enc, sel(c"setFragmentTexture:atIndex:"), mtl_y, 0);
                set_tex(enc, sel(c"setFragmentTexture:atIndex:"), mtl_cbcr, 1);
                // setFragmentBytes:length:atIndex:
                let params = Params {
                    format: 0,
                    full_range: full_range as u32,
                    pad0: 0,
                    pad1: 0,
                };
                let set_bytes: extern "C" fn(Id, Sel, *const c_void, u64, u64) =
                    mem::transmute(objc_msgSend as *const ());
                set_bytes(
                    enc,
                    sel(c"setFragmentBytes:length:atIndex:"),
                    &params as *const Params as *const c_void,
                    mem::size_of::<Params>() as u64,
                    0,
                );
                // drawPrimitives:vertexStart:vertexCount:
                let draw: extern "C" fn(Id, Sel, u64, u64, u64) =
                    mem::transmute(objc_msgSend as *const ());
                draw(
                    enc,
                    sel(c"drawPrimitives:vertexStart:vertexCount:"),
                    MTL_PRIM_TRIANGLE,
                    0,
                    3,
                );
                msg(enc, sel(c"endEncoding"));
                msg_void_id(cb, sel(c"presentDrawable:"), drawable);
                msg(cb, sel(c"commit"));

                // Rotate the GPU-completion holders: the resources two frames old
                // are released now (their command buffer has long completed); this
                // frame's resources are held for the next.
                self.prev_hold = self.last_hold.take();
                self.last_hold = Some(FrameHold {
                    pb: CFRetain(pb) as CVPixelBufferRef,
                    tex_y,
                    tex_cbcr,
                });
                CVMetalTextureCacheFlush(self.cache, 0);
                Ok(())
            })();
            objc_autoreleasePoolPop(pool);
            res
        }
    }
}

impl Drop for MetalRenderer {
    fn drop(&mut self) {
        unsafe {
            // Release per-frame holds (surfaces + textures) and the texture cache.
            self.last_hold = None;
            self.prev_hold = None;
            if !self.cache.is_null() {
                CFRelease(self.cache);
            }
            if !self.layer.is_null() {
                CFRelease(self.layer);
            }
        }
    }
}

/// Pull the `NSView` pointer out of a winit window handle (macOS/AppKit).
fn appkit_ns_view<W: HasWindowHandle>(window: &W) -> Result<Id, RenderError> {
    let handle = window
        .window_handle()
        .map_err(|e| RenderError::Failed(format!("window_handle: {e}")))?;
    match handle.as_raw() {
        RawWindowHandle::AppKit(h) => Ok(h.ns_view.as_ptr() as Id),
        other => Err(RenderError::Failed(format!(
            "expected AppKit window handle, got {other:?}"
        ))),
    }
}
