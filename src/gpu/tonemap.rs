//! The GPU display tone map: resident sample buffers, the display table, and
//! the compute dispatch that turns one into RGBA (`tone.wgsl`).
//!
//! The residency is the point (see the module docs): a frame's samples are
//! uploaded on the update that first shows it and stay in VRAM, so re-toning it
//! — the contrast-slider drag this path exists for — uploads nothing but the
//! table. Buffers are keyed by [`FrameData::uid`] rather than by pane and frame
//! index, so a frame shown in two panes is uploaded once and a reloaded or
//! recomputed pane can't be handed the previous image's buffer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use wgpu::util::DeviceExt;

use super::{GpuContext, GpuError};
use crate::media::{FrameData, Samples};
use crate::palette::Palette;

/// The tone a pane wants applied, as `stage` already resolved it.
#[derive(Clone, Copy)]
pub struct Tone {
    /// Linear display bounds `[lo, hi] → [0, 255]`, as the CPU path computes them.
    pub lo: f32,
    pub hi: f32,
    /// The Colormap palette, or `None` for the plain grey render. Mono frames
    /// only, exactly as `render_into_scaled_cmap` requires.
    pub palette: Option<Palette>,
}

/// A pane's toned pixels on the GPU: the buffer the compute pass writes, sized
/// and strided for the copy into a texture.
///
/// Rows are padded to a 256-byte stride because that is what a buffer→texture
/// copy demands; `row_words` is what the shader indexes by, so the padding never
/// reaches the texture.
pub struct GpuOutput {
    buf: wgpu::Buffer,
    pub size: [usize; 2],
    row_words: u32,
}

impl GpuOutput {
    /// Bytes per padded output row.
    fn row_bytes(&self) -> u32 {
        self.row_words * 4
    }

    /// Copy the toned pixels into `tex`, which must be [`GpuTex::FORMAT`] at
    /// [`size`](Self::size). A byte copy, so the texture holds exactly the bytes
    /// the shader wrote — no float quantisation between the tone map and the
    /// screen (see `tone.wgsl`). The format is sRGB because that is how egui
    /// samples it; the copy itself is untouched by that, since a copy moves
    /// bytes and the encoding only decides how the sampler reads them.
    fn copy_to_texture(&self, enc: &mut wgpu::CommandEncoder, tex: &wgpu::Texture) {
        enc.copy_buffer_to_texture(
            wgpu::ImageCopyBuffer {
                buffer: &self.buf,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(self.row_bytes()),
                    rows_per_image: None,
                },
            },
            wgpu::ImageCopyTexture {
                texture: tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: self.size[0] as u32,
                height: self.size[1] as u32,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Read the toned pixels back to the CPU as tightly packed RGBA, dropping
    /// the row padding.
    ///
    /// Test-only, and deliberately so: this **blocks** until the GPU is done,
    /// which is exactly what the display path exists to avoid. It is here so the
    /// tests can hold the shader's output against the CPU render's; nothing in
    /// the running app ever brings these pixels back.
    #[cfg(test)]
    pub fn read_back(&self, gpu: &GpuContext) -> Vec<[u8; 4]> {
        let [w, h] = self.size;
        let bytes = self.row_bytes() as u64 * h as u64;
        let staging = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cim tone readback"),
            size: bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut enc = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        enc.copy_buffer_to_buffer(&self.buf, 0, &staging, 0, bytes);
        gpu.queue.submit([enc.finish()]);

        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        gpu.device.poll(wgpu::Maintain::Wait);
        let _ = rx.recv();

        let view = slice.get_mapped_range();
        let stride = self.row_bytes() as usize;
        let mut out = Vec::with_capacity(w * h);
        for y in 0..h {
            let row = &view[y * stride..y * stride + w * 4];
            out.extend(row.chunks_exact(4).map(|p| [p[0], p[1], p[2], p[3]]));
        }
        drop(view);
        staging.unmap();
        out
    }
}

/// A frame's samples, resident in VRAM.
///
/// The buffer is an `Arc` so a caller can take a handle to it and use it after
/// releasing the cache lock — see [`GpuToneMapper::resident_frame`].
struct FrameBuf {
    buf: Arc<wgpu::Buffer>,
    bytes: u64,
    /// Value of the cache's tick when this was last rendered from, for the LRU.
    touched: u64,
}

/// A pane's uploaded display table, rebuilt only when its tone changes.
struct LutBuf {
    buf: wgpu::Buffer,
    key: TableKey,
}

/// What a display table is a function of — the same inputs `ToneLut` keys on,
/// plus the entry count and the sample type.
///
/// The **sample type** is not implied by the entry count, which is why it is
/// here: `tone_table_rgba` builds a u8 frame's 256 entries as *mapped values
/// indexed by the raw sample*, and a float frame's 256 entries as the flat ramp
/// (or palette) *indexed by the level the shader computes*. Same length, opposite
/// meanings — so without this a pane whose frame changed width between the two
/// would reuse the other's table and render a tone window it was never given.
#[derive(PartialEq, Eq, Clone, Copy)]
struct TableKey {
    lo: u32,
    hi: u32,
    mask: bool,
    palette: Option<u8>,
    entries: usize,
    kernel: Kernel,
}

/// Which kernel a frame's samples need.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kernel {
    U8,
    U16,
    F32,
}

impl Kernel {
    /// The kernel `frame`'s samples need.
    fn of(frame: &FrameData) -> Self {
        match &frame.samples {
            Samples::U8(_) => Kernel::U8,
            Samples::U16(_) => Kernel::U16,
            Samples::F32(_) => Kernel::F32,
        }
    }
}

/// Uniform block matching `Params` in `tone.wgsl`. Field order and padding are
/// load-bearing — WGSL lays a uniform struct out with 16-byte alignment, and
/// eight 4-byte scalars fill exactly two rows with nothing to pad.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    w: u32,
    h: u32,
    ch: u32,
    cc: u32,
    row_words: u32,
    lo: f32,
    scale: f32,
    mask: u32,
}

/// Threads per workgroup edge — must match `@workgroup_size` in `tone.wgsl`.
const TILE: u32 = 16;

/// Row stride a buffer→texture copy requires, in bytes.
const COPY_ROW_ALIGN: u32 = 256;

/// Fraction of the adapter's reported VRAM the resident frame cache may hold.
/// Deliberately modest: on an integrated GPU "VRAM" *is* system RAM, so every
/// byte here is a byte the frame cache (`cache_budget_mb`) can no longer use,
/// and the CPU-side cache is the one the app cannot work without.
const VRAM_SHARE: f64 = 0.25;

/// Ceiling on the resident frame cache regardless of card size — a handful of
/// big frames, which is what the re-tone path actually revisits.
const VRAM_CAP: u64 = 1 << 30;

/// The resident sample buffers, keyed by [`FrameData::uid`], plus their LRU.
///
/// **Shared between panes on purpose** — a frame shown in two panes is uploaded
/// once — which is why it is the one piece of mapper state behind a lock.
struct FrameCache {
    map: HashMap<u64, FrameBuf>,
    resident: u64,
    budget: u64,
    tick: u64,
}

/// The parts of the tone map every pane shares: the compute pipelines (built
/// once, immutable) and the resident-frame cache.
///
/// Everything here takes `&self`, so this lives in an `Arc` handed to every
/// pane's render worker and the panes tone **concurrently** — see
/// [`PaneTone`] for the per-pane half and [`resident_frame`](Self::resident_frame)
/// for how the one shared, mutable piece stays out of the way.
pub struct GpuToneMapper {
    pipelines: HashMap<Kernel, wgpu::ComputePipeline>,
    layout: wgpu::BindGroupLayout,
    frames: Mutex<FrameCache>,
}

/// One pane's tone-map state, owned by that pane's render worker thread.
///
/// The display table is a function of the pane's own `[lo, hi]`, so it was
/// already per-pane when the mapper kept them all in a map keyed by pane id;
/// giving it to the worker instead just puts it where its single owner is —
/// exactly as the CPU worker owns the pane's `ToneLut` and its operator
/// instances. Nothing here is shared, so nothing here locks, and a closed pane's
/// table is freed when its worker thread exits rather than needing a `forget`.
#[derive(Default)]
pub struct PaneTone {
    lut: Option<LutBuf>,
    /// CPU-side scratch for building a display table before upload.
    table: Vec<u32>,
}

impl GpuToneMapper {
    pub fn new(gpu: &GpuContext) -> Self {
        let device = &gpu.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cim tone map"),
            source: wgpu::ShaderSource::Wgsl(include_str!("tone.wgsl").into()),
        });
        let entry = |binding: u32, ty: wgpu::BindingType| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty,
            count: None,
        };
        let storage = |read_only: bool| wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cim tone map"),
            entries: &[
                entry(
                    0,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                entry(1, storage(true)),
                entry(2, storage(true)),
                entry(3, storage(false)),
            ],
        });
        let pipe_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cim tone map"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let mut pipelines = HashMap::new();
        for (kernel, entry_point) in [
            (Kernel::U8, "tone_u8"),
            (Kernel::U16, "tone_u16"),
            (Kernel::F32, "tone_f32"),
        ] {
            pipelines.insert(
                kernel,
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(entry_point),
                    layout: Some(&pipe_layout),
                    module: &module,
                    entry_point,
                    compilation_options: Default::default(),
                    cache: None,
                }),
            );
        }
        Self {
            pipelines,
            layout,
            frames: Mutex::new(FrameCache {
                map: HashMap::new(),
                resident: 0,
                budget: vram_budget(gpu),
                tick: 0,
            }),
        }
    }

    /// Whether this frame's samples are already in VRAM.
    ///
    /// Residency is the module's whole contract — it is what makes a re-tone
    /// cost nothing — but nothing in the app has to *ask*, since the tone map
    /// establishes it itself. This is here for the tests that pin the contract.
    #[cfg(test)]
    pub fn is_resident(&self, uid: u64) -> bool {
        self.frames
            .lock()
            .expect("frame cache")
            .map
            .contains_key(&uid)
    }

    /// Tone `frame` into `out` using `pane`'s display table, and — when `tex` is
    /// given — copy the result into it, all in one submission.
    ///
    /// Uploads only what changed: the samples on the first render of a frame,
    /// the display table when the tone moved, and nothing at all when a repaint
    /// re-renders the same frame at the same tone.
    ///
    /// Takes `&self`, so **every pane's worker may be in here at once**. The
    /// sample upload — the expensive part, and a plain CPU memcpy into mapped
    /// memory — genuinely runs in parallel; the dispatches behind it are still
    /// serialised by the one device, which is the GPU's business, not ours.
    ///
    /// Full resolution only. The decimated render (`step > 1`) is small, cheap
    /// and already off the critical path, and its output would not match a
    /// full-resolution texture's identity anyway — `stage` keeps it on the CPU.
    pub fn tone(
        &self,
        gpu: &GpuContext,
        pane: &mut PaneTone,
        frame: &FrameData,
        tone: Tone,
        out: &mut Option<GpuOutput>,
        tex: Option<&wgpu::Texture>,
    ) -> Result<(), GpuError> {
        if !gpu.healthy() {
            return Err(GpuError::DeviceLost);
        }
        let [w, h] = frame.size;
        if w == 0 || h == 0 {
            return Err(GpuError::Unsupported("empty frame"));
        }
        // The kernels read channel 0 (mono / palette) or channels 0..3 (RGB),
        // which covers every layout the decoder produces.
        if frame.channels == 0 || frame.channels > 4 {
            return Err(GpuError::Unsupported("channel count"));
        }

        let kernel = Kernel::of(frame);
        let bytes = match &frame.samples {
            Samples::U8(v) => bytemuck::cast_slice::<u8, u8>(v),
            Samples::U16(v) => bytemuck::cast_slice::<u16, u8>(v),
            Samples::F32(v) => bytemuck::cast_slice::<f32, u8>(v),
        };
        gpu.check_binding(bytes.len() as u64)?;

        // Bring all three buffers up to date first, then borrow them together.
        let src = self.resident_frame(gpu, frame.uid(), bytes);
        let lut = Self::resident_lut(gpu, pane, frame, tone);
        let output = Self::resident_output(gpu, out, [w, h])?;

        let denom = tone.hi - tone.lo;
        let params = Params {
            w: w as u32,
            h: h as u32,
            ch: frame.channels as u32,
            cc: frame.color_channels() as u32,
            row_words: output.row_words,
            lo: tone.lo,
            // Exactly `map_u8`'s scale, including its empty-window rule.
            scale: if denom > 0.0 { 255.0 / denom } else { 0.0 },
            mask: u32::from(frame.is_mask()),
        };
        let params_buf = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("cim tone params"),
                contents: bytemuck::bytes_of(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cim tone map"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: src.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: lut.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: output.buf.as_entire_binding(),
                },
            ],
        });

        let mut enc = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("cim tone map"),
            });
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("cim tone map"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipelines[&kernel]);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups((w as u32).div_ceil(TILE), (h as u32).div_ceil(TILE), 1);
        }
        if let Some(tex) = tex {
            output.copy_to_texture(&mut enc, tex);
        }
        gpu.queue.submit([enc.finish()]);
        // The submission is queued, not awaited: egui's own paint lands on the
        // same queue afterwards, so it observes these writes without this thread
        // ever blocking on the GPU.
        self.frames.lock().expect("frame cache").evict_over_budget();
        Ok(())
    }

    /// Ensure the frame's samples are in VRAM, uploading them on first use, and
    /// mark them as used this tick (for the LRU). Returns the buffer to bind.
    ///
    /// **The upload happens with the lock released.** That is the whole reason
    /// the buffer is an `Arc`: holding the cache locked across a 30-odd-MB
    /// memcpy would serialise exactly the work this path just moved off the UI
    /// thread to get done in parallel, leaving panes queued behind each other as
    /// badly as before, only somewhere less visible.
    ///
    /// So two panes that first show the *same* frame in the same instant can
    /// both upload it, and one of the two buffers is dropped unused. That is
    /// accepted deliberately: it costs one redundant upload in a rare race,
    /// where the alternative costs a serialised upload in the common case. The
    /// insert resolves the race — whoever gets there second takes the winner's
    /// buffer and drops its own — so both panes still bind the same VRAM, and
    /// the accounting counts it once.
    fn resident_frame(&self, gpu: &GpuContext, uid: u64, bytes: &[u8]) -> Arc<wgpu::Buffer> {
        {
            let mut cache = self.frames.lock().expect("frame cache");
            cache.tick += 1;
            let tick = cache.tick;
            if let Some(f) = cache.map.get_mut(&uid) {
                f.touched = tick;
                return f.buf.clone();
            }
        }
        let buf = Arc::new(
            gpu.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("cim frame samples"),
                    contents: bytes,
                    usage: wgpu::BufferUsages::STORAGE,
                }),
        );
        let size = buf.size();
        let mut cache = self.frames.lock().expect("frame cache");
        let tick = cache.tick;
        if let Some(f) = cache.map.get_mut(&uid) {
            // Lost the race: take the resident buffer and drop ours.
            f.touched = tick;
            return f.buf.clone();
        }
        cache.resident += size;
        cache.map.insert(
            uid,
            FrameBuf {
                buf: buf.clone(),
                bytes: size,
                touched: tick,
            },
        );
        buf
    }

    /// Ensure the pane's display table is in VRAM, rebuilding and re-uploading
    /// it only when its tone changed. This is the one upload a slider drag pays.
    ///
    /// Pane-local, so no lock: the table belongs to the worker that calls this.
    fn resident_lut<'a>(
        gpu: &GpuContext,
        pane: &'a mut PaneTone,
        frame: &FrameData,
        tone: Tone,
    ) -> &'a wgpu::Buffer {
        let key = TableKey {
            lo: tone.lo.to_bits(),
            hi: tone.hi.to_bits(),
            mask: frame.is_mask(),
            palette: tone.palette.map(|p| p.id()),
            entries: frame.tone_table_entries(),
            kernel: Kernel::of(frame),
        };
        if !pane.lut.as_ref().is_some_and(|l| l.key == key) {
            let pal = tone.palette.map(|p| (p.table(), p.id()));
            frame.tone_table_rgba(tone.lo, tone.hi, pal, &mut pane.table);
            let buf = gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("cim tone table"),
                    contents: bytemuck::cast_slice(&pane.table),
                    usage: wgpu::BufferUsages::STORAGE,
                });
            pane.lut = Some(LutBuf { buf, key });
        }
        &pane.lut.as_ref().expect("just built").buf
    }

    /// The pane's output buffer, reallocated only when the frame size changes
    /// (so a playback run through same-sized pages reuses one allocation).
    fn resident_output<'a>(
        gpu: &GpuContext,
        out: &'a mut Option<GpuOutput>,
        size: [usize; 2],
    ) -> Result<&'a GpuOutput, GpuError> {
        if out.as_ref().is_some_and(|o| o.size == size) {
            return Ok(out.as_ref().unwrap());
        }
        let row_words = row_words(size[0]);
        let bytes = row_words as u64 * 4 * size[1] as u64;
        gpu.check_binding(bytes)?;
        let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cim toned pixels"),
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        Ok(out.insert(GpuOutput {
            buf,
            size,
            row_words,
        }))
    }

    /// Bytes of frame samples currently held in VRAM. See
    /// [`is_resident`](Self::is_resident) on why this is test-only.
    #[cfg(test)]
    pub fn resident_bytes(&self) -> u64 {
        self.frames.lock().expect("frame cache").resident
    }

    /// Set the resident-frame budget. Test-only: the app sizes it from the
    /// adapter (see [`vram_budget`]).
    #[cfg(test)]
    pub fn set_budget(&self, bytes: u64) {
        self.frames.lock().expect("frame cache").budget = bytes;
    }
}

impl FrameCache {
    /// Drop the least recently toned frames until the resident set is back
    /// inside its budget. A frame just rendered from is never a candidate — it
    /// was touched on the way in, so it sorts last.
    fn evict_over_budget(&mut self) {
        while self.resident > self.budget && self.map.len() > 1 {
            let Some((&uid, _)) = self.map.iter().min_by_key(|(_, f)| f.touched) else {
                break;
            };
            if let Some(f) = self.map.remove(&uid) {
                self.resident -= f.bytes;
            }
        }
    }
}

/// Words per padded output row for a `w`-pixel image.
fn row_words(w: usize) -> u32 {
    ((w as u32 * 4).next_multiple_of(COPY_ROW_ALIGN)) / 4
}

/// How much VRAM the resident frame cache may use on this adapter. wgpu doesn't
/// report memory size, so this keys off the adapter *kind* instead: a discrete
/// card gets the flat cap, while an integrated or software one — whose "VRAM" is
/// the same RAM the decoded-frame cache lives in — gets a quarter of that, since
/// taking memory from the CPU cache would cost more decodes than the residency
/// saves tone maps.
fn vram_budget(gpu: &GpuContext) -> u64 {
    match gpu.info.device_type {
        wgpu::DeviceType::DiscreteGpu => VRAM_CAP,
        _ => (VRAM_CAP as f64 * VRAM_SHARE) as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::ToneLut;
    use std::sync::Arc;

    /// A device for the GPU tests, or `None` where there is no adapter at all —
    /// a headless CI box, or this project's own sandbox. Every test below opens
    /// with a skip on `None` rather than failing, because "no GPU here" is a
    /// supported state of the product, not a broken test run.
    ///
    /// Software adapters (lavapipe) are accepted: they execute the real kernels,
    /// which is what these tests are checking, and are the only way this file
    /// gets covered on a machine without a card.
    fn gpu() -> Option<Arc<GpuContext>> {
        GpuContext::new_standalone(true)
    }

    macro_rules! gpu_or_skip {
        () => {
            match gpu() {
                Some(g) => g,
                None => {
                    eprintln!("skipped: no wgpu adapter on this machine");
                    return;
                }
            }
        };
    }

    /// The CPU render of the same frame and tone, as flat RGBA pixels — the
    /// reference every GPU render below is held to.
    fn cpu_render(frame: &FrameData, tone: Tone) -> Vec<[u8; 4]> {
        let mut out = Vec::<u8>::new();
        let mut lut = ToneLut::default();
        let _ = match tone.palette {
            Some(p) => frame.render_into_scaled_cmap(
                tone.lo,
                tone.hi,
                1,
                p.table(),
                p.id(),
                &mut lut,
                &mut out,
            ),
            None => frame.render_into_scaled_lut(tone.lo, tone.hi, 1, &mut lut, &mut out),
        };
        out.chunks_exact(4)
            .map(|p| [p[0], p[1], p[2], p[3]])
            .collect()
    }

    fn gpu_render(gpu: &GpuContext, frame: &FrameData, tone: Tone) -> Vec<[u8; 4]> {
        let mapper = GpuToneMapper::new(gpu);
        let mut pane = PaneTone::default();
        let mut out = None;
        mapper
            .tone(gpu, &mut pane, frame, tone, &mut out, None)
            .expect("gpu tone map");
        out.unwrap().read_back(gpu)
    }

    fn plain(lo: f32, hi: f32) -> Tone {
        Tone {
            lo,
            hi,
            palette: None,
        }
    }

    /// The integer paths must be **bit-identical** to the CPU render, not close:
    /// they index the very table the CPU built, so any difference is a bug in
    /// the fetch/pack arithmetic. Covers both sample widths, mono and RGB, an
    /// odd width (so the 16×16 tile bounds check and the padded row stride are
    /// both exercised), a single row, and the degenerate `lo >= hi` windows.
    #[test]
    fn integer_tone_map_matches_cpu_exactly() {
        let gpu = gpu_or_skip!();
        // 8-bit mono at an odd width, 16-bit mono, 16-bit RGB, a 1-row frame,
        // and a 4-channel frame (whose alpha the render ignores).
        let mono8 = FrameData::new(
            [37, 21],
            1,
            Samples::U8((0..37 * 21).map(|i| i as u8).collect()),
        );
        let mono16 = FrameData::new(
            [40, 30],
            1,
            Samples::U16((0..40 * 30).map(|i| (i * 53) as u16).collect()),
        );
        let rgb16 = FrameData::new(
            [17, 9],
            3,
            Samples::U16((0..17 * 9 * 3).map(|i| (i * 401) as u16).collect()),
        );
        let row = FrameData::new([64, 1], 1, Samples::U8((0..64).map(|i| i as u8).collect()));
        let rgba8 = FrameData::new(
            [10, 10],
            4,
            Samples::U8((0..10 * 10 * 4).map(|i| i as u8).collect()),
        );

        let cases: [(&FrameData, f32, f32); 7] = [
            (&mono8, 0.0, 255.0),
            (&mono8, 40.0, 200.0),
            (&mono16, 1000.0, 60000.0),
            (&mono16, 5.0, 5.0), // empty window: everything collapses to one level
            (&rgb16, 400.0, 61000.0),
            (&row, 0.0, 255.0),
            (&rgba8, 10.0, 240.0),
        ];
        for (i, (frame, lo, hi)) in cases.into_iter().enumerate() {
            let tone = plain(lo, hi);
            assert_eq!(
                gpu_render(&gpu, frame, tone),
                cpu_render(frame, tone),
                "case {i}"
            );
        }
    }

    /// A boolean mask renders pure black/white on the GPU too — for integer
    /// samples because the rule is folded into the uploaded table, and for
    /// floats because the shader applies it before the linear map. Checked at a
    /// tone window that would otherwise map the values to something else.
    #[test]
    fn mask_tone_map_matches_cpu_exactly() {
        let gpu = gpu_or_skip!();
        let bits: Vec<u8> = (0..24 * 8).map(|i| u8::from(i % 3 == 0)).collect();
        let u8_mask = FrameData::new_mask([24, 8], 1, Samples::U8(bits.clone()));
        let u16_mask = FrameData::new_mask(
            [24, 8],
            1,
            Samples::U16(bits.iter().map(|&b| b as u16).collect()),
        );
        let f32_mask = FrameData::new_mask(
            [24, 8],
            1,
            Samples::F32(bits.iter().map(|&b| b as f32).collect()),
        );
        for frame in [&u8_mask, &u16_mask, &f32_mask] {
            let tone = plain(1000.0, 2000.0);
            assert_eq!(gpu_render(&gpu, frame, tone), cpu_render(frame, tone));
        }
    }

    /// The Colormap tone: an integer source indexes the CPU's own per-value RGB
    /// table, so it too must match exactly.
    #[test]
    fn colormap_tone_map_matches_cpu_exactly() {
        let gpu = gpu_or_skip!();
        let pal = crate::palette::Palette::Viridis;
        let u8f = FrameData::new(
            [31, 5],
            1,
            Samples::U8((0..31 * 5).map(|i| i as u8).collect()),
        );
        let u16f = FrameData::new(
            [31, 5],
            1,
            Samples::U16((0..31 * 5).map(|i| (i * 397) as u16).collect()),
        );
        for frame in [&u8f, &u16f] {
            let tone = Tone {
                lo: 10.0,
                hi: 40000.0,
                palette: Some(pal),
            };
            assert_eq!(gpu_render(&gpu, frame, tone), cpu_render(frame, tone));
        }
    }

    /// Float sources are the one path that computes rather than looks up, so
    /// they are held to a tolerance rather than to equality: the shader mirrors
    /// `map_u8` term for term and should agree bit for bit, but a driver is
    /// allowed to flush denormals, which can move a sample sitting exactly on a
    /// level boundary by one. One code value is invisible; silently mapping to
    /// the wrong end of the range would not be, and that is what this catches.
    /// NaN and the infinities are pinned exactly, since those are rules rather
    /// than rounding.
    #[test]
    fn float_tone_map_matches_cpu_within_one_level() {
        let gpu = gpu_or_skip!();
        let mut v: Vec<f32> = (0..30 * 20).map(|i| i as f32 * 0.37 - 5.0).collect();
        v[0] = f32::NAN;
        v[1] = f32::INFINITY;
        v[2] = f32::NEG_INFINITY;
        v[3] = 0.0;
        let frame = FrameData::new([30, 20], 1, Samples::F32(v));
        let tone = plain(-2.0, 90.0);

        let got = gpu_render(&gpu, &frame, tone);
        let want = cpu_render(&frame, tone);
        assert_eq!(got.len(), want.len());
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            // NaN -> 0, +inf -> 255, -inf -> 0: exact, they are clamping rules.
            if i < 3 {
                assert_eq!(g, w, "special value at {i}");
                continue;
            }
            for c in 0..4 {
                let d = g[c].abs_diff(w[c]);
                assert!(d <= 1, "pixel {i} channel {c}: gpu {:?} cpu {:?}", g, w);
            }
        }
    }

    /// Residency is the point of the whole path: rendering the same frame twice
    /// must reuse its VRAM buffer rather than re-upload it, and re-toning it
    /// must still produce the CPU's answer for the *new* tone (i.e. the display
    /// table is rebuilt even though the samples are not).
    #[test]
    fn frame_stays_resident_across_retone() {
        let gpu = gpu_or_skip!();
        let frame = FrameData::new(
            [64, 64],
            1,
            Samples::U16((0..64 * 64).map(|i| (i * 7) as u16).collect()),
        );
        let mapper = GpuToneMapper::new(&gpu);
        let mut pane = PaneTone::default();
        let mut out = None;

        mapper
            .tone(&gpu, &mut pane, &frame, plain(0.0, 65535.0), &mut out, None)
            .unwrap();
        assert!(mapper.is_resident(frame.uid()));
        let after_first = mapper.resident_bytes();
        assert_eq!(after_first, 64 * 64 * 2);

        let retone = plain(2000.0, 50000.0);
        mapper
            .tone(&gpu, &mut pane, &frame, retone, &mut out, None)
            .unwrap();
        assert_eq!(
            mapper.resident_bytes(),
            after_first,
            "re-toning must not upload the samples again"
        );
        assert_eq!(out.unwrap().read_back(&gpu), cpu_render(&frame, retone));
    }

    /// The display table is cached per pane, so the *reuse* has to be as correct
    /// as the build: a pane that goes on rendering with one mapper must get the
    /// right table for each frame, not the one its predecessor left behind.
    ///
    /// The trap this pins is that a u8 frame and a float frame both want a
    /// 256-entry table with opposite meanings — mapped values indexed by the raw
    /// sample, versus a flat ramp indexed by a computed level. At a matching tone
    /// every other part of the key agrees, so a key blind to the sample type
    /// hands the second frame the first's table and renders it as though no tone
    /// window had been applied at all.
    #[test]
    fn cached_table_follows_the_sample_type() {
        let gpu = gpu_or_skip!();
        let tone = plain(40.0, 200.0);
        // The same values, one frame per sample width, all rendered by one
        // mapper into one pane id — the app's steady state.
        let u8f = FrameData::new(
            [16, 16],
            1,
            Samples::U8((0..256).map(|i| i as u8).collect()),
        );
        let f32f = FrameData::new(
            [16, 16],
            1,
            Samples::F32((0..256).map(|i| i as f32).collect()),
        );
        let u16f = FrameData::new(
            [16, 16],
            1,
            Samples::U16((0..256).map(|i| i as u16).collect()),
        );
        let mapper = GpuToneMapper::new(&gpu);
        let mut pane = PaneTone::default();
        let mut out = None;
        // Twice around, so each frame is rendered both first (building the
        // table) and after the others (reusing or rebuilding it).
        for _ in 0..2 {
            for frame in [&u8f, &f32f, &u16f] {
                mapper
                    .tone(&gpu, &mut pane, frame, tone, &mut out, None)
                    .unwrap();
                let got = out.as_ref().unwrap().read_back(&gpu);
                let want = cpu_render(frame, tone);
                // The float path is the ±1 one (see the test above); comparing
                // channel-wise covers all three uniformly.
                for (i, (g, w)) in got.iter().zip(&want).enumerate() {
                    for c in 0..4 {
                        assert!(
                            g[c].abs_diff(w[c]) <= 1,
                            "pixel {i} channel {c}: gpu {g:?} cpu {w:?}"
                        );
                    }
                }
            }
        }
    }

    /// Panes tone **concurrently** through one shared mapper — that is the whole
    /// point of `tone` taking `&self`, and what keeps a grid of panes from
    /// queueing behind each other the way they did when this ran on the UI
    /// thread.
    ///
    /// Two things have to hold at once, and they pull against each other. Every
    /// pane must get the CPU render's answer (so the sharing hasn't crossed any
    /// wires), *and* a frame several panes show at the same instant must end up
    /// resident **once** (so the race in `resident_frame` — which deliberately
    /// uploads outside the lock, and so can upload twice — still resolves to one
    /// buffer and one accounting entry).
    #[test]
    fn panes_tone_concurrently_and_share_one_upload() {
        let gpu = gpu_or_skip!();
        let shared = FrameData::new(
            [64, 64],
            1,
            Samples::U16((0..64 * 64).map(|i| (i * 11) as u16).collect()),
        );
        let tone = plain(300.0, 60000.0);
        let want = cpu_render(&shared, tone);
        let mapper = GpuToneMapper::new(&gpu);

        // Eight "panes" hitting the same not-yet-resident frame at once.
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    let mut pane = PaneTone::default();
                    let mut out = None;
                    mapper
                        .tone(&gpu, &mut pane, &shared, tone, &mut out, None)
                        .expect("gpu tone map");
                    assert_eq!(out.unwrap().read_back(&gpu), want);
                });
            }
        });
        assert!(mapper.is_resident(shared.uid()));
        assert_eq!(
            mapper.resident_bytes(),
            64 * 64 * 2,
            "a frame every pane shows must be uploaded and accounted for once, \
             however many of them raced to upload it"
        );

        // And distinct frames in parallel: each pane's own table, one shared
        // cache, no crossed results.
        let frames: Vec<FrameData> = (1..=4)
            .map(|k| {
                FrameData::new(
                    [32, 32],
                    1,
                    Samples::U16((0..32 * 32).map(|i| (i * k) as u16).collect()),
                )
            })
            .collect();
        std::thread::scope(|s| {
            for f in &frames {
                s.spawn(|| {
                    let mut pane = PaneTone::default();
                    let mut out = None;
                    mapper
                        .tone(&gpu, &mut pane, f, tone, &mut out, None)
                        .expect("gpu tone map");
                    assert_eq!(out.unwrap().read_back(&gpu), cpu_render(f, tone));
                });
            }
        });
    }

    /// The resident set is bounded: past its budget the least recently toned
    /// frames are dropped, and the one just rendered is never the victim.
    #[test]
    fn resident_frames_evict_over_budget() {
        let gpu = gpu_or_skip!();
        let mapper = GpuToneMapper::new(&gpu);
        mapper.set_budget(4 * 1024); // a couple of the frames below
        let mut pane = PaneTone::default();
        let frames: Vec<FrameData> = (0..6)
            .map(|k| FrameData::new([32, 16], 1, Samples::U16(vec![k as u16; 32 * 16])))
            .collect();
        let mut out = None;
        for f in &frames {
            mapper
                .tone(&gpu, &mut pane, f, plain(0.0, 10.0), &mut out, None)
                .unwrap();
        }
        assert!(
            mapper.resident_bytes() <= 4 * 1024,
            "resident {} over budget {}",
            mapper.resident_bytes(),
            4 * 1024
        );
        assert!(
            mapper.is_resident(frames.last().unwrap().uid()),
            "the frame just rendered must survive eviction"
        );
    }

    /// A frame too big for the device is reported, not submitted — the caller
    /// renders it on the CPU. Checked against the limit itself so the test
    /// doesn't have to allocate anything enormous.
    #[test]
    fn oversized_frame_is_rejected() {
        let gpu = gpu_or_skip!();
        let limit = gpu.limits.max_storage_buffer_binding_size as u64;
        assert!(gpu.check_binding(limit).is_ok());
        assert!(matches!(
            gpu.check_binding(limit + 1),
            Err(GpuError::TooLarge { .. })
        ));
    }

    /// The whole display data path, end to end: tone map, then the copy into the
    /// `Rgba8Unorm` texture egui actually samples, read back **from the texture**.
    ///
    /// This is the step the pixel-exactness claim rests on. Writing a storage
    /// texture directly would have put an f32→unorm8 quantisation between the
    /// tone map and the screen, whose rounding the specs pin only to within
    /// 0.6 ULP; going through a buffer and a byte copy has no such latitude. A
    /// width of 100 px makes the copy's row padding (400 B → 512 B) real, so a
    /// stride mistake would shear the image rather than hide.
    #[test]
    fn texture_holds_exactly_the_toned_bytes() {
        let gpu = gpu_or_skip!();
        let [w, h] = [100usize, 24usize];
        let frame = FrameData::new(
            [w, h],
            1,
            Samples::U16((0..w * h).map(|i| (i * 613) as u16).collect()),
        );
        let tone = plain(500.0, 62000.0);

        let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test target"),
            size: wgpu::Extent3d {
                width: w as u32,
                height: h as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        let mapper = GpuToneMapper::new(&gpu);
        let mut pane = PaneTone::default();
        let mut out = None;
        mapper
            .tone(&gpu, &mut pane, &frame, tone, &mut out, Some(&tex))
            .expect("gpu tone map");

        // Pull the texture back the same way it was filled, padding and all.
        let stride = row_words(w) * 4;
        let staging = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: stride as u64 * h as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut enc = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        enc.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &staging,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: w as u32,
                height: h as u32,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.submit([enc.finish()]);
        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        gpu.device.poll(wgpu::Maintain::Wait);
        let _ = rx.recv();
        let view = slice.get_mapped_range();
        let got: Vec<[u8; 4]> = (0..h)
            .flat_map(|y| {
                view[y * stride as usize..y * stride as usize + w * 4]
                    .chunks_exact(4)
                    .map(|p| [p[0], p[1], p[2], p[3]])
                    .collect::<Vec<_>>()
            })
            .collect();
        drop(view);
        staging.unmap();

        assert_eq!(got, cpu_render(&frame, tone));
    }

    /// Output rows are padded to the 256-byte stride a buffer→texture copy
    /// needs, and a width already on that stride is left alone.
    #[test]
    fn output_rows_are_copy_aligned() {
        for w in [1usize, 17, 63, 64, 100, 4096] {
            let words = row_words(w);
            assert!(words >= w as u32, "width {w} must fit");
            assert_eq!((words * 4) % COPY_ROW_ALIGN, 0, "width {w} stride");
        }
        assert_eq!(row_words(64), 64); // 64 px * 4 B = 256 B exactly
    }
}
