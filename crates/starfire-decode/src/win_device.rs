// SPDX-License-Identifier: Apache-2.0
//! A shared D3D11 device for the Windows zero-copy pipeline.
//!
//! The Media Foundation decoder and the native D3D11 renderer run on **one**
//! multithread-protected D3D11 device, so a decoded NV12 texture flows from
//! decode → present with no cross-device/adapter handle sharing (which fails on
//! hybrid GPUs). The app creates one [`SharedDevice`] and hands a clone to both.
#![cfg(target_os = "windows")]

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};

/// A D3D11 device + immediate context shared by the decoder and renderer. Cheap
/// to clone (COM ref-count). Multithread-protected, so the decode thread and the
/// render thread can both drive it.
#[derive(Clone)]
pub struct SharedDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

// Used across the decode + render threads on a multithread-protected device.
unsafe impl Send for SharedDevice {}
unsafe impl Sync for SharedDevice {}

impl SharedDevice {
    /// Create a hardware D3D11 device with video support, multithread-protected.
    pub fn create() -> windows::core::Result<Self> {
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
            let device = device.ok_or_else(windows::core::Error::from_win32)?;
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            let context = device.GetImmediateContext()?;
            Ok(Self { device, context })
        }
    }
}
