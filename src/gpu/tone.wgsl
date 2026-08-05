// Display tone mapping: native samples -> packed 8-bit RGBA, one thread per
// output pixel. The CPU counterpart is `media::render::fill_lut` / `fill_cmap`,
// and these kernels are written to be **bit-identical** to it, not merely close:
//
//   * Integer sources never do arithmetic here. The host uploads the very table
//     `ToneLut` built on the CPU (`lut`, one packed RGBA word per sample value),
//     so a u8/u16 frame is a pure lookup and cannot drift from the CPU render by
//     construction — including the boolean-mask rule and the Colormap palette,
//     both of which are already folded into that table.
//   * Float sources have no bounded domain to tabulate, so they map
//     arithmetically, mirroring `map_u8` term for term (see `tone_index`).
//
// Results go to a storage **buffer**, not a storage texture: the host then does a
// plain byte copy into the texture egui samples. Writing a texture directly would
// route every pixel through an f32 -> unorm8 quantisation whose rounding the
// specs only pin to within 0.6 ULP — a byte copy has no such latitude.

struct Params {
    // Source geometry. `ch` is the interleaved channel count of the samples,
    // `cc` the channels that carry colour (1 = mono/palette, 3 = RGB).
    w: u32,
    h: u32,
    ch: u32,
    cc: u32,
    // Words per output row. Rows are padded so the row stride is a multiple of
    // 256 bytes, which is what a buffer -> texture copy requires.
    row_words: u32,
    // The float map `[lo, hi] -> [0, 255]`, precomputed as in `map_u8`:
    // `scale = 255 / (hi - lo)`, or 0 for an empty window. Unused by the
    // integer paths, which look the same map up in `lut`.
    lo: f32,
    scale: f32,
    // 1 when the frame is a boolean mask (float sources only — an integer mask
    // is folded into `lut`).
    mask: u32,
}

@group(0) @binding(0) var<uniform> p: Params;
// Native samples, exactly the bytes of the frame's `Samples` buffer.
@group(0) @binding(1) var<storage, read> src: array<u32>;
// Sample value -> packed RGBA (0xAABBGGRR), built on the CPU.
@group(0) @binding(2) var<storage, read> lut: array<u32>;
@group(0) @binding(3) var<storage, read_write> out: array<u32>;

// Sample fetches. The buffer is the frame's own little-endian bytes, so a u32
// word holds 4 consecutive u8 or 2 consecutive u16 samples, lowest first.
fn fetch_u8(i: u32) -> u32 {
    return (src[i >> 2u] >> ((i & 3u) * 8u)) & 0xffu;
}

fn fetch_u16(i: u32) -> u32 {
    return (src[i >> 1u] >> ((i & 1u) * 16u)) & 0xffffu;
}

/// The float map, mirroring `media::render::map_u8` case for case: the
/// mask rule, then NaN (Rust's `f32::clamp` passes NaN through and the
/// saturating `as u8` cast turns it into 0), then the linear window. `u32()`
/// truncates toward zero, as `as u8` does, and clamping first makes both ends
/// saturate rather than wrap.
fn tone_index(s: f32) -> u32 {
    if (p.mask != 0u) {
        return select(0u, 255u, s != 0.0);
    }
    if (s != s) {
        return 0u;
    }
    return u32(clamp((s - p.lo) * p.scale, 0.0, 255.0));
}

/// Assemble one output pixel from its already-mapped colour indices. A mono
/// source takes the table entry whole (so a Colormap palette keeps its colour);
/// an RGB source takes each channel's grey level, which is the low byte of that
/// channel's table entry.
fn pack(e0: u32, e1: u32, e2: u32) -> u32 {
    if (p.cc == 1u) {
        return e0;
    }
    return 0xff000000u | ((e2 & 0xffu) << 16u) | ((e1 & 0xffu) << 8u) | (e0 & 0xffu);
}

fn store(x: u32, y: u32, px: u32) {
    out[y * p.row_words + x] = px;
}

@compute @workgroup_size(16, 16)
fn tone_u8(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.w || gid.y >= p.h) {
        return;
    }
    let base = (gid.y * p.w + gid.x) * p.ch;
    if (p.cc == 1u) {
        store(gid.x, gid.y, lut[fetch_u8(base)]);
    } else {
        store(gid.x, gid.y, pack(
            lut[fetch_u8(base)],
            lut[fetch_u8(base + 1u)],
            lut[fetch_u8(base + 2u)],
        ));
    }
}

@compute @workgroup_size(16, 16)
fn tone_u16(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.w || gid.y >= p.h) {
        return;
    }
    let base = (gid.y * p.w + gid.x) * p.ch;
    if (p.cc == 1u) {
        store(gid.x, gid.y, lut[fetch_u16(base)]);
    } else {
        store(gid.x, gid.y, pack(
            lut[fetch_u16(base)],
            lut[fetch_u16(base + 1u)],
            lut[fetch_u16(base + 2u)],
        ));
    }
}

// Floats map arithmetically to an 8-bit index, then take their colour from a
// 256-entry table — grey levels for the plain render, the palette for Colormap.
@compute @workgroup_size(16, 16)
fn tone_f32(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.w || gid.y >= p.h) {
        return;
    }
    let base = (gid.y * p.w + gid.x) * p.ch;
    if (p.cc == 1u) {
        store(gid.x, gid.y, lut[tone_index(bitcast<f32>(src[base]))]);
    } else {
        store(gid.x, gid.y, pack(
            lut[tone_index(bitcast<f32>(src[base]))],
            lut[tone_index(bitcast<f32>(src[base + 1u]))],
            lut[tone_index(bitcast<f32>(src[base + 2u]))],
        ));
    }
}
