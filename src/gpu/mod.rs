//! Optional GPU display path — tone mapping (and Compute-pane reductions) on
//! the graphics card, behind the `render_backend` setting.
//!
//! # Why this exists, and what it is actually worth
//!
//! The CPU tone map is already parallel and close to memory bandwidth
//! (`media::render`), so simply *moving* it to the GPU buys nothing: a compute
//! pass whose result has to be read back over PCIe spends on the readback what
//! it saved on the map. The win comes from what the GPU can then avoid doing
//! again. A decoded frame's samples stay **resident in VRAM**, and the toned
//! pixels are written into a texture egui samples directly. Re-toning that
//! frame — dragging the contrast slider on a 4096² image, the interaction this
//! whole path exists for — is then a ~256 KB table upload and a dispatch,
//! instead of a full-image CPU map plus a full-image texture upload.
//!
//! So the ordering matters: a frame is uploaded once (on the update that first
//! shows it) and re-toned many times for free, which is exactly the shape of
//! the interaction. Playback, where every frame is new, gains little — the
//! sample upload replaces the RGBA texture upload the CPU path was doing
//! anyway, at half the bytes for a u16 source.
//!
//! # Not accelerated here, on purpose
//!
//! * **Decoding.** LZW is serial and branch-heavy; it is what a big TIFF
//!   actually costs, and it is not a GPU workload.
//! * **The proprietary C++ operators** (`crate::imageproc`). Closed-source CPU
//!   code mutating a `u16` buffer in place — a pane running them stays entirely
//!   on the CPU, which `CimApp::stage` enforces.
//! * **The export composite** (`crate::export`). Offline, already parallel, and
//!   every frame is uploaded once and used once — no residency to exploit.
//! * **The Compute panes** (`app::compute`, `media::stats`), for two separate
//!   reasons, both worth stating because the idea is an obvious one:
//!   - **Mean / Std** accumulate the stack in `f64` on purpose, split by sample
//!     index so the non-associative sum reproduces bit for bit. WGSL has no
//!     `f64`, so a GPU version could not be the same number — and these are
//!     measurements, read off as text and saved as data, not pixels where a
//!     last-ulp difference would go unseen.
//!   - **Add / Sub** would be exact (plain `f32`), but they lose on traffic. The
//!     result is a float frame — *larger* than the 16-bit inputs — and the rest
//!     of the app needs it in system memory anyway for the histogram, the region
//!     statistics, the line profile and saving. So the readback alone moves more
//!     bytes than the CPU version moves in total, and that is before the
//!     dispatch. Residency doesn't rescue it: the inputs being in VRAM saves the
//!     upload, not the return trip.
//!
//!   A Compute pane still benefits from this module, just on the other side: its
//!   float result is displayed through the same GPU tone map as any other large
//!   frame, so panning and re-toning it are as cheap as for a decoded image.
//!
//! # Pixel exactness
//!
//! The tool's invariant is that a displayed texel is a true source sample, and
//! the CPU render paths are pinned to each other byte for byte. The GPU path
//! holds to the same standard rather than being allowed to be "close":
//! integer sources index the very table `ToneLut` built on the CPU (see
//! [`FrameData::tone_table_rgba`]) and do no arithmetic at all, and the toned
//! bytes reach the texture by a byte copy rather than a float quantisation. Only
//! float sources compute anything on the GPU, mirroring `map_u8` term for term.
//! `gpu::tonemap`'s tests hold both to the CPU render's output.
//!
//! # Falling back
//!
//! Every entry point returns `Result`, and `CimApp` treats any error as "this
//! machine is not doing GPU work this session": it drops the context, logs once
//! under `CIM_DEBUG`, and the CPU path — which is always compiled, always
//! tested, and never removed — renders the frame instead. A machine with no
//! adapter never builds a context in the first place.

mod tonemap;

pub use tonemap::{GpuOutput, GpuToneMapper, Tone};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::settings::RenderBackend;

/// What the GPU path can fail with. Every variant means the same thing to the
/// caller — render this on the CPU instead — so the payload is only ever used
/// for the `CIM_DEBUG` log line.
#[derive(Debug)]
pub enum GpuError {
    /// The frame needs a buffer larger than this device allows. Reported per
    /// frame rather than fatally: a smaller frame on the same device is fine.
    TooLarge { bytes: u64, limit: u64 },
    /// The device was lost (driver reset, GPU removed, laptop switched away
    /// from the discrete card).
    DeviceLost,
    /// A frame whose sample layout the kernels don't cover.
    Unsupported(&'static str),
}

impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GpuError::TooLarge { bytes, limit } => {
                write!(f, "frame needs {bytes} B, device allows {limit} B")
            }
            GpuError::DeviceLost => write!(f, "device lost"),
            GpuError::Unsupported(why) => write!(f, "unsupported: {why}"),
        }
    }
}

/// A device the compute passes run on, plus the facts the rest of the app needs
/// to size its work to it.
///
/// In the running app this **wraps eframe's own device** rather than creating a
/// second one ([`from_render_state`](Self::from_render_state)): sharing it is
/// what lets a toned texture be handed to egui without ever leaving the card.
/// The standalone constructor exists for the settings probe (which only wants
/// the adapter's name) and for the tests.
pub struct GpuContext {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    /// Adapter name / backend, for the Settings readout.
    pub info: wgpu::AdapterInfo,
    /// The device's real limits, which the frame-size checks read. These are the
    /// adapter's maxima, not wgpu's conservative defaults — a 4096² RGBA u16
    /// frame is 134 MB and would not fit the default 128 MiB storage binding.
    pub limits: wgpu::Limits,
    /// Shared with the driver's device-lost callback, which is why this is an
    /// `Arc` rather than a plain flag.
    lost: Arc<AtomicBool>,
}

impl GpuContext {
    /// Wrap the device eframe created for the wgpu renderer. This is the one the
    /// app uses; a texture written through it can be handed straight to egui.
    pub fn from_render_state(rs: &eframe::egui_wgpu::RenderState) -> Arc<Self> {
        Arc::new(Self::wrap(
            rs.device.clone(),
            rs.queue.clone(),
            rs.adapter.get_info(),
            rs.device.limits(),
        ))
    }

    /// Open a device of our own, with no surface — used by the Settings adapter
    /// probe and by the tests, never by the display path.
    ///
    /// `accept_software` admits a CPU adapter (llvmpipe / lavapipe). The tests
    /// want that, since CI has no real GPU; `RenderBackend::Auto` does not,
    /// because a software "GPU" is slower than the CPU path it would replace.
    pub fn new_standalone(accept_software: bool) -> Option<Arc<Self>> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))?;
        let info = adapter.get_info();
        if !accept_software && info.device_type == wgpu::DeviceType::Cpu {
            return None;
        }
        let (device, queue) =
            pollster::block_on(adapter.request_device(&device_descriptor(&adapter), None)).ok()?;
        let limits = device.limits();
        Some(Arc::new(Self::wrap(
            Arc::new(device),
            Arc::new(queue),
            info,
            limits,
        )))
    }

    fn wrap(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        info: wgpu::AdapterInfo,
        limits: wgpu::Limits,
    ) -> Self {
        let lost = Arc::new(AtomicBool::new(false));
        // A lost device (driver reset, GPU removed, a laptop switching away from
        // its discrete card) can't be recovered in place. Raise the flag so the
        // next `healthy` check sends the app back to the CPU instead of
        // submitting work that will never complete.
        //
        // Deliberately *not* installing an uncaptured-error handler as well: on
        // the shared device that would displace the one eframe's own renderer
        // relies on.
        let flag = lost.clone();
        device.set_device_lost_callback(move |reason, msg| {
            crate::debug::log(&format!("gpu: device lost ({reason:?}): {msg}"));
            flag.store(true, Ordering::Relaxed);
        });
        Self {
            device,
            queue,
            info,
            limits,
            lost,
        }
    }

    /// Whether GPU work should still be submitted. Once this goes false it stays
    /// false — the app drops the context and renders on the CPU from then on.
    pub fn healthy(&self) -> bool {
        !self.lost.load(Ordering::Relaxed)
    }

    /// Human-readable adapter summary for the Settings readout, e.g.
    /// `NVIDIA GeForce RTX 3060 (Vulkan)`.
    pub fn describe(&self) -> String {
        format!("{} ({:?})", self.info.name, self.info.backend)
    }

    /// Reject a buffer this device can't bind, before wgpu would raise it as an
    /// uncaptured error (which is fatal by default).
    pub fn check_binding(&self, bytes: u64) -> Result<(), GpuError> {
        let limit: u64 = self.limits.max_storage_buffer_binding_size.into();
        if bytes > limit || bytes > self.limits.max_buffer_size {
            return Err(GpuError::TooLarge {
                bytes,
                limit: limit.min(self.limits.max_buffer_size),
            });
        }
        Ok(())
    }
}

/// A pane's toned image living entirely on the GPU: the compute pass's output
/// buffer, the texture it is copied into, and that texture's registration with
/// egui.
///
/// This is the payoff of sharing eframe's device — the pixels are produced and
/// consumed on the card, so nothing crosses the bus in either direction. It
/// replaces an `egui::TextureHandle` in a pane's texture slot and is reused
/// across frames of the same size, so a playback run neither reallocates the
/// texture nor re-registers its id.
pub struct GpuTex {
    /// The compute output, allocated on first use by the tone mapper.
    pub(crate) out: Option<GpuOutput>,
    pub(crate) tex: wgpu::Texture,
    id: eframe::egui::TextureId,
    size: [usize; 2],
    /// Kept so the registration can be released when this is dropped. Cheap —
    /// `RenderState` is a bundle of `Arc`s.
    rs: eframe::egui_wgpu::RenderState,
}

impl GpuTex {
    /// Allocate a `size` texture and register it with egui, **NEAREST**-filtered
    /// like every other texture the tool shows: a displayed texel must be a true
    /// source sample, never a blend of neighbours, at any zoom.
    pub fn new(
        rs: &eframe::egui_wgpu::RenderState,
        gpu: &GpuContext,
        size: [usize; 2],
    ) -> Result<Self, GpuError> {
        let dim = gpu.limits.max_texture_dimension_2d as usize;
        if size[0] > dim || size[1] > dim {
            return Err(GpuError::TooLarge {
                bytes: size[0].max(size[1]) as u64,
                limit: dim as u64,
            });
        }
        let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cim pane image"),
            size: wgpu::Extent3d {
                width: size[0] as u32,
                height: size[1] as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let id = rs.renderer.write().register_native_texture(
            &gpu.device,
            &view,
            wgpu::FilterMode::Nearest,
        );
        Ok(Self {
            out: None,
            tex,
            id,
            size,
            rs: rs.clone(),
        })
    }

    /// The id to draw with.
    pub fn id(&self) -> eframe::egui::TextureId {
        self.id
    }

    /// Pixel size of the texture, so a pane can tell whether it can be reused
    /// for the next frame or has to be reallocated.
    pub fn size(&self) -> [usize; 2] {
        self.size
    }
}

impl Drop for GpuTex {
    fn drop(&mut self) {
        // Releases egui's bind group for this texture. It does *not* destroy the
        // texture — `register_native_texture` never took ownership of one — so
        // the `wgpu::Texture` below frees itself as usual, after any submission
        // still using it retires.
        self.rs.renderer.write().free_texture(&self.id);
    }
}

/// The device to ask an adapter for — eframe's own descriptor, but with the
/// adapter's **real** limits rather than wgpu's portable defaults.
///
/// The defaults cap a storage binding at 128 MiB and a 2-D texture at 8192 px,
/// neither of which covers the images this path exists for (a 4096² RGBA u16
/// frame alone is 134 MB). Asking for the adapter's maxima can't fail — they
/// are by definition supported — and leaves the frame-size check
/// ([`GpuContext::check_binding`]) as the one place that decides a frame is too
/// big, which then falls back to the CPU for that frame only.
pub fn device_descriptor(adapter: &wgpu::Adapter) -> wgpu::DeviceDescriptor<'static> {
    wgpu::DeviceDescriptor {
        label: Some("cim gpu device"),
        required_features: wgpu::Features::default(),
        required_limits: adapter.limits(),
        memory_hints: wgpu::MemoryHints::default(),
    }
}

/// Whether this run should use the GPU, and hence whether eframe is asked for
/// the wgpu renderer. Resolved once, before the window exists, because the
/// renderer can only be chosen at startup.
///
/// `Auto` probes for a hardware adapter and quietly stays on the CPU when there
/// isn't one — the case every GPU-less machine takes, and the reason this is a
/// probe rather than a compile-time choice.
pub fn wants_gpu(backend: RenderBackend) -> bool {
    match backend {
        RenderBackend::Cpu => false,
        RenderBackend::Auto => adapter_summary(false).is_some(),
        RenderBackend::Gpu => adapter_summary(true).is_some(),
    }
}

/// The adapter `backend` would pick, described for the Settings readout, or
/// `None` when this machine has none to offer. Opens a throwaway device, so
/// call it once and remember the answer.
pub fn adapter_summary(accept_software: bool) -> Option<String> {
    GpuContext::new_standalone(accept_software).map(|g| g.describe())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The backend setting resolves the same way on every machine, which is what
    /// makes "works without a GPU" a property rather than a hope. Written as
    /// invariants rather than expected values, since the answer legitimately
    /// differs between a workstation with a card, a laptop without one, and CI.
    #[test]
    fn backend_resolution_never_forces_a_gpu() {
        // Choosing CPU is absolute: no probing, no adapter, no wgpu renderer —
        // the machine behaves as it did before this option existed.
        assert!(!wants_gpu(RenderBackend::Cpu));
        // Auto is the strictly more cautious of the two GPU choices: it rejects
        // a software adapter that `Gpu` would take. So anything Auto accepts,
        // `Gpu` accepts too — never the reverse.
        if wants_gpu(RenderBackend::Auto) {
            assert!(
                wants_gpu(RenderBackend::Gpu),
                "Auto must not accept an adapter that an explicit Gpu would refuse"
            );
        }
        // And whatever this machine is, asking twice gives the same answer.
        assert_eq!(wants_gpu(RenderBackend::Auto), wants_gpu(RenderBackend::Auto));
    }

    /// `Auto` must never pick a software rasteriser: llvmpipe is slower than the
    /// CPU path it would be replacing, so silently "finding a GPU" there would
    /// make the default setting a pessimisation. An explicit `Gpu` may have it —
    /// that is how these tests run at all.
    #[test]
    fn auto_refuses_a_software_adapter() {
        let Some(gpu) = GpuContext::new_standalone(true) else {
            eprintln!("skipped: no wgpu adapter on this machine");
            return;
        };
        if gpu.info.device_type == wgpu::DeviceType::Cpu {
            assert!(
                !wants_gpu(RenderBackend::Auto),
                "the only adapter here is software, so Auto must stay on the CPU"
            );
        }
    }
}
