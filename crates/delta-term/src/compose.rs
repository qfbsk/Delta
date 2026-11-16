//! Whole-window glass. The canvas keeps drawing with GDI, but the frame is
//! uploaded into a DirectComposition swap chain instead of BitBlt'ing to the
//! window DC. The window carries WS_EX_NOREDIRECTIONBITMAP, so this visual is
//! its only content, and DWM draws the acrylic (gaussian blur of what is
//! behind) underneath it. Translucency is one opacity on the visual; the
//! canvas' own pixels stay opaque.

use std::ffi::c_void;

use windows::core::{Interface, IUnknown};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::{
	D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
	D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
	D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, ID3D11Device,
	ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::DirectComposition::{
	DCompositionCreateDevice, IDCompositionDevice, IDCompositionEffect, IDCompositionEffectGroup,
	IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
	DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
	DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
	DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2,
	IDXGIOutput, IDXGISwapChain1,
};

/// How much of the window the canvas' base colour covers; the rest is the
/// blurred desktop. The darkness that keeps a bright window behind from
/// washing the base colour into gray lives in the acrylic tint instead.
const OPACITY: f32 = 0.8;

pub struct Compositor {
	context: ID3D11DeviceContext,
	swap: IDXGISwapChain1,
	/// Kept alive: the target and the visual die with their device.
	_device: IDCompositionDevice,
	_target: IDCompositionTarget,
	_visual: IDCompositionVisual,
	width: u32,
	height: u32,
}

impl Compositor {
	pub fn new(hwnd: *mut c_void, width: i32, height: i32) -> Option<Self> {
		if width <= 0 || height <= 0 {
			return None;
		}
		let (device, context) = device()?;
		let dxgi: IDXGIDevice = device.cast().ok()?;
		let adapter = unsafe { dxgi.GetAdapter() }.ok()?;
		let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }.ok()?;
		let desc = DXGI_SWAP_CHAIN_DESC1 {
			Width: width as u32,
			Height: height as u32,
			Format: DXGI_FORMAT_B8G8R8A8_UNORM,
			Stereo: false.into(),
			SampleDesc: DXGI_SAMPLE_DESC {
				Count: 1,
				Quality: 0,
			},
			BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
			BufferCount: 2,
			Scaling: DXGI_SCALING_STRETCH,
			SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
			AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
			Flags: 0,
		};
		let none: Option<&IDXGIOutput> = None;
		let swap = unsafe { factory.CreateSwapChainForComposition(&device, &desc, none) }.ok()?;
		let dcomp: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi) }.ok()?;
		let target = unsafe { dcomp.CreateTargetForHwnd(HWND(hwnd), true) }.ok()?;
		let visual = unsafe { dcomp.CreateVisual() }.ok()?;
		let content: IUnknown = swap.cast().ok()?;
		unsafe { visual.SetContent(&content) }.ok()?;
		let group: IDCompositionEffectGroup = unsafe { dcomp.CreateEffectGroup() }.ok()?;
		unsafe { group.SetOpacity2(OPACITY) }.ok()?;
		let effect: IDCompositionEffect = group.cast().ok()?;
		unsafe { visual.SetEffect(&effect) }.ok()?;
		unsafe { target.SetRoot(&visual) }.ok()?;
		unsafe { dcomp.Commit() }.ok()?;
		Some(Self {
			context,
			swap,
			_device: dcomp,
			_target: target,
			_visual: visual,
			width: width as u32,
			height: height as u32,
		})
	}

	pub fn resize(&mut self, width: i32, height: i32) {
		if width <= 0 || height <= 0 {
			return;
		}
		if width as u32 == self.width && height as u32 == self.height {
			return;
		}
		let result = unsafe {
			self.swap.ResizeBuffers(
				2,
				width as u32,
				height as u32,
				DXGI_FORMAT_B8G8R8A8_UNORM,
				DXGI_SWAP_CHAIN_FLAG(0),
			)
		};
		if result.is_ok() {
			self.width = width as u32;
			self.height = height as u32;
		}
	}

	/// `bits` is a top-down 32-bit DIB with alpha bytes already set to 255.
	pub fn present(&self, bits: *const u8) {
		if bits.is_null() {
			return;
		}
		let stride = self.width * 4;
		unsafe {
			let buffer: ID3D11Texture2D = match self.swap.GetBuffer(0) {
				Ok(buffer) => buffer,
				Err(_) => return,
			};
			self.context.UpdateSubresource(
				&buffer,
				0,
				None,
				bits as *const c_void,
				stride,
				stride * self.height,
			);
			let _ = self.swap.Present(1, DXGI_PRESENT(0));
		}
	}
}

fn device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
	let levels = [
		D3D_FEATURE_LEVEL_11_0,
		D3D_FEATURE_LEVEL_10_1,
		D3D_FEATURE_LEVEL_10_0,
	];
	for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
		let mut device: Option<ID3D11Device> = None;
		let mut context: Option<ID3D11DeviceContext> = None;
		let result = unsafe {
			D3D11CreateDevice(
				None,
				driver,
				Default::default(),
				D3D11_CREATE_DEVICE_BGRA_SUPPORT,
				Some(&levels),
				D3D11_SDK_VERSION,
				Some(&mut device),
				None,
				Some(&mut context),
			)
		};
		if result.is_ok() {
			if let (Some(device), Some(context)) = (device, context) {
				return Some((device, context));
			}
		}
	}
	None
}
