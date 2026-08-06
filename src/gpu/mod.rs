//! Optional GPU display path — tone mapping (and Compute-pane reductions) on
//! the graphics card, behind the `hardware_accel` setting.
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

/// The graphics backends this app will accept, for **both** the startup probe
/// and the renderer eframe builds — they have to agree, or the probe green-lights
/// a backend the renderer then falls over on.
///
/// `PRIMARY` is "everything except OpenGL/GLES". Excluding GL is deliberate and
/// is not about performance:
///
/// * OpenGL is already covered, and better — a run without hardware acceleration
///   uses **glow**, eframe's own OpenGL renderer, which is the tested path for
///   every VNC / software-GL machine this tool runs on. wgpu's GLES backend would
///   be a second, worse OpenGL implementation of the same thing.
/// * It crashes rather than declining. `wgpu-hal`'s GLES backend `unwrap()`s
///   `eglMakeCurrent` (`gles/egl.rs`), so a display that refuses the context —
///   `BadAccess`, which is what an X/VNC session hands back once a context is
///   current elsewhere — takes the process down inside eframe's setup, before
///   any of this module's own fallbacks can run.
/// * The GPU path's value is compute residency (see the module docs), and GLES
///   compute needs ES 3.1 anyway, which the software stacks in question are
///   exactly the ones not to have.
///
/// So the shape of the fallback is: no Vulkan (or Metal / DX12) adapter → no GPU
/// → the CPU path on glow, which is the same outcome as a machine with no card
/// at all, and a supported state rather than an error.
pub const BACKENDS: wgpu::Backends = wgpu::Backends::PRIMARY;

/// An instance limited to [`BACKENDS`].
fn instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: BACKENDS,
        ..Default::default()
    })
}

/// Human-readable adapter summary for the Settings readout, e.g.
/// `NVIDIA GeForce RTX 3060 (Vulkan)`.
fn describe(info: &wgpu::AdapterInfo) -> String {
    format!("{} ({:?})", info.name, info.backend)
}

/// A device the compute passes run on, plus the facts the rest of the app needs
/// to size its work to it.
///
/// In the running app this **wraps eframe's own device** rather than creating a
/// second one ([`from_render_state`](Self::from_render_state)): sharing it is
/// what lets a toned texture be handed to egui without ever leaving the card.
/// The standalone constructor exists for the tests.
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

    /// Open a device of our own, with no surface — **the tests only**, never the
    /// app: the display path shares eframe's device, and the Settings probe
    /// ([`adapter_summary`]) stops at the adapter without opening anything.
    ///
    /// `accept_software` admits a CPU adapter (llvmpipe / lavapipe). The tests
    /// want that, since CI has no real GPU; the app never does, because a
    /// software "GPU" is slower than the CPU path it would replace.
    #[cfg(test)]
    pub fn new_standalone(accept_software: bool) -> Option<Arc<Self>> {
        let instance = instance();
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
        describe(&self.info)
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
    /// The format egui samples a registered native texture as.
    ///
    /// **It must be sRGB**, and this is not a preference: `egui-wgpu` builds its
    /// own managed textures as `Rgba8UnormSrgb` (so the sampler decodes to linear)
    /// and its fragment shader assumes what it samples is already linear, doing
    /// the linear→gamma step itself on the way to the framebuffer.
    /// `register_native_texture` is documented to require the same format.
    ///
    /// A plain `Rgba8Unorm` here is byte-for-byte the same texture and still
    /// *draws* — which is why this was easy to miss — but the sampler then hands
    /// egui the display-encoded byte as though it were linear, and the shader
    /// gamma-encodes it a second time. The image comes out visibly lighter and
    /// flatter than the CPU render of the same pixels: it reads as a different
    /// tone window even though `[lo, hi]` and every mapped byte are identical.
    /// Since the GPU takes only full-resolution renders of large frames, that
    /// showed up as the *same pane changing tone* as it was zoomed past the
    /// decimation threshold and the CPU handed the render over.
    pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

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
            format: Self::FORMAT,
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

/// Whether the *Hardware acceleration* setting is offered at all this run, read
/// once from `CIM_GPU=1` (as `crate::debug::enabled` reads `CIM_DEBUG`).
///
/// **The path is shelved, not deleted.** It is kept compiled, tested and
/// documented so the work can be picked back up, but on the deployment it was
/// built for — an NVIDIA card driven over VNC — it measured as a loss on every
/// axis that matters, so it should not be reachable by a user who wanders into
/// Settings. What was measured, and why the conclusion is about the display
/// stack rather than about this module:
///
/// * The tone map itself is **excellent**: re-toning a resident frame is under a
///   millisecond, against ~7 ms on the CPU. Residency does exactly what it was
///   designed to do.
/// * But the wgpu renderer costs more per frame than glow does there, whatever
///   the tone map does, so the win never reaches the frame rate.
/// * And it **tears** — a seam between two halves of the window showing
///   different frames. That happens while merely panning a still frame, when no
///   render is dispatched at all, so it is the Vulkan → X → VNC presentation
///   path, not anything this module writes. `PresentMode` is already FIFO and
///   `desired_maximum_frame_latency: Some(1)` changed nothing.
/// * Moving the tone map to a background thread — the obvious fix for it sitting
///   on the UI thread — made a 4.5 ms render take 170 ms, on a single pane: the
///   device is monopolised by a presentation slow enough that any concurrent use
///   of it queues behind. (That change was reverted; see the history.)
///
/// None of this rules the path out on a **local display**, which is where it
/// should be evaluated next, and is what the env var is for.
pub fn exposed() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("CIM_GPU").as_deref() == Ok("1"))
}

/// Whether this run should use the GPU, and hence whether eframe is asked for
/// the wgpu renderer. Resolved once, before the window exists, because the
/// renderer can only be chosen at startup.
///
/// Three conditions, all required: the path is [`exposed`] this run (`CIM_GPU=1`),
/// the user asked for hardware acceleration (`config.hardware_accel`, **off** by
/// default), and this machine actually has a **hardware** adapter. The probe is
/// why a GPU-less machine can carry the setting on without anything changing, and
/// why a software rasteriser (llvmpipe / lavapipe) doesn't count — it is slower
/// than the CPU path it would replace, so accepting it would make the toggle a
/// pessimisation.
///
/// The env gate is checked **here** rather than only in Settings on purpose: a
/// config left with `hardware_accel: true` from before the setting was hidden
/// must not go on selecting the wgpu renderer with no visible control to turn it
/// off. Hiding the switch and leaving the wiring live is how a user ends up
/// stuck.
pub fn wants_gpu(hardware_accel: bool) -> bool {
    exposed() && hardware_accel && adapter_summary().is_some()
}

/// The hardware adapter this machine offers, described for the Settings readout,
/// or `None` when it has none — a software rasteriser included, since accepting
/// one would make the toggle slower than leaving it off.
///
/// **Stops at the adapter**: it reads the name and nothing more, rather than
/// opening a device it would immediately throw away. That matters at startup,
/// where this runs *before* eframe builds the renderer — a probe device is a
/// second driver context on the same display for no reason, and the GL backends
/// that go wrong when two exist are exactly the ones [`BACKENDS`] excludes.
/// Still not free (it initialises the driver), so callers remember the answer.
pub fn adapter_summary() -> Option<String> {
    let instance = instance();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))?;
    let info = adapter.get_info();
    (info.device_type != wgpu::DeviceType::Cpu).then(|| describe(&info))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The setting resolves the same way on every machine, which is what makes
    /// "works without a GPU" a property rather than a hope. Written as
    /// invariants rather than expected values, since the answer legitimately
    /// differs between a workstation with a card, a laptop without one, and CI.
    #[test]
    fn backend_resolution_never_forces_a_gpu() {
        // The default (toggle off) is absolute: no probing, no adapter, no wgpu
        // renderer — the machine behaves as it did before this option existed.
        assert!(!wants_gpu(false));
        // And whatever this machine is, asking twice gives the same answer.
        assert_eq!(wants_gpu(true), wants_gpu(true));
    }

    /// The texture egui samples must be sRGB-encoded, because egui's own
    /// managed textures are and its shader assumes what it samples has already
    /// been decoded to linear. A plain `Rgba8Unorm` here still draws — it is the
    /// same bytes — but they are read as linear and gamma-encoded a second time,
    /// which shows up as the GPU-rendered pane being lighter and flatter than
    /// the CPU render of the identical pixels. Cheap to assert, and it is the
    /// kind of thing a refactor "simplifies" back to `Rgba8Unorm`.
    #[test]
    fn the_texture_egui_samples_is_srgb() {
        assert!(GpuTex::FORMAT.is_srgb());
    }

    /// GL is deliberately not in [`BACKENDS`]: a run without acceleration is
    /// already on eframe's own OpenGL renderer, and wgpu's GLES backend panics
    /// (rather than declining) on a display that refuses `eglMakeCurrent`. The
    /// probe and eframe's renderer are handed this same set, so the probe can
    /// never green-light a backend the renderer then fails on.
    #[test]
    fn opengl_is_not_an_accepted_backend() {
        assert!(!BACKENDS.contains(wgpu::Backends::GL));
        assert!(BACKENDS.contains(wgpu::Backends::VULKAN));
    }

    /// The toggle must never pick a software rasteriser: llvmpipe is slower than
    /// the CPU path it would be replacing, so silently "finding a GPU" there
    /// would make enabling acceleration a pessimisation. The tests reach one
    /// through `new_standalone(true)` directly — that is how they run at all.
    #[test]
    fn hardware_accel_refuses_a_software_adapter() {
        let Some(gpu) = GpuContext::new_standalone(true) else {
            eprintln!("skipped: no wgpu adapter on this machine");
            return;
        };
        if gpu.info.device_type == wgpu::DeviceType::Cpu {
            assert!(
                !wants_gpu(true),
                "the only adapter here is software, so the app must stay on the CPU"
            );
        }
    }
}
