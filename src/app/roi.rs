//! Adaptive (viewport-region) rendering — geometry, activation, and the region
//! texture cache.
//!
//! With the **Adaptive rendering** setting on, a pane that is zoomed into part
//! of its image renders one **view-centred region** (the visible rect plus
//! margin) at the zoom's own decimation step and paints it over a heavily
//! decimated whole-image base. The point is the per-frame cost of a *playing*
//! sequence: the classic path re-renders and re-uploads the entire image every
//! frame even when a few percent of it is on screen, and for the sizes this
//! tool compares (roughly 1000–4000 px) that is where the frame rate goes.
//!
//! The mode engages only when it demonstrably pays — `roi_plan` weighs both
//! layers against the whole-image render they replace and declines below
//! [`MIN_GAIN`], so zooming out (where the region approaches the whole image)
//! quietly returns the pane to the classic path and adaptive rendering can
//! never be a pessimisation.
//!
//! Everything view-dependent — where the region is, how big, which step — is
//! computed here as pure functions so it can be unit-tested headlessly; the
//! staging that drives them (`stage_region`, called from `refresh_textures` so
//! regions ride the lock-step commit) and the paint site live with their
//! pipelines (`decode`, `canvas`).
//!
//! Coordinates: a region lives in **output-texel space at `step`** — the image
//! decimated axis-wise by `step` (`decode::decimated_size`). Its `origin`/`dims` snap to
//! `QUANTUM`-texel multiples so a smooth pan crosses cache keys rarely, and so
//! the C++ operator instances (keyed on their input size) see few distinct
//! sizes. The source-pixel rect is `origin * step ..` and is what the region
//! renders sample (`RegionKey::region`, fed to `media::FrameData::render_lut`).

use super::*;

/// Side cap (texels) on the whole-image **base** texture while adaptive
/// rendering is active for a pane.
///
/// Deliberately tiny, because while the region path is active the base is
/// **fully occluded**: a region always spans at least the visible rect
/// (`dims >= span + 2 * QUANTUM`), so the only base pixels that can reach the
/// screen are in the letterbox around a rotated or edge-clamped image. Its jobs
/// are to keep the pane's geometry and lock-step identity (`disp_size`,
/// `CachedTex`) and to cover that letterbox — neither needs resolution. Since
/// the base re-renders on **every frame** of a playing sequence, this cap is
/// what stops a whole-image render from dominating the very case adaptive
/// rendering exists for.
///
/// It is a *side* cap, so it bites hardest on the small end of the target range
/// — and that is where it has to: at 512 a 1000² sequence still spent 43% of
/// its adaptive budget on invisible base pixels, which held the whole mode
/// below its engagement threshold (`activation_engages_only_when_it_saves_work`
/// pins that). The cost of going lower is a blurrier fallback for the frame or
/// two after zooming out past the engagement threshold, before the full-size
/// base commits — the same brief staleness any `step` change already has.
pub(super) const BASE_MAX: usize = 256;

/// Region origin/dims granularity, in output texels. Coarse enough that panning
/// re-keys (and re-renders) the region only every `QUANTUM` texels and operator
/// input sizes repeat; fine enough that a region never overshoots the viewport
/// by much. It also sets the **floor** region size (`2 * QUANTUM` of snap
/// allowance, rounded up), so it bounds what a heavily zoomed-in pane costs:
/// 128 puts that floor at 384², where 256 put it at 768² — 4× the texels for a
/// viewport that may be showing a few hundred pixels.
pub(super) const QUANTUM: usize = 128;

/// Region span per axis, in multiples of the visible rect, **while the sequence
/// is paused**: extra pre-rendered runway so an interactive pan stays sharp
/// between re-renders.
const COVER_PAUSED: f32 = 2.0;

/// [`COVER_PAUSED`] while **playing**. Runway is nearly worthless here: every
/// frame needs a fresh region anyway (the key carries the frame's `uid`), so a
/// margin is re-rendered per frame and thrown away, and the `2 * QUANTUM` snap
/// allowance already absorbs a frame's worth of pan. Paying only for what is on
/// screen is the whole point of the mode during playback.
const COVER_PLAYING: f32 = 1.0;

/// The least work adaptive rendering must save before it engages, as a ratio of
/// the classic whole-image render it replaces. Below this the two layers (base +
/// region), their two uploads and the cache churn aren't worth it — and the
/// margin keeps the mode from *ever* being a pessimisation, which the previous
/// sharpness-based predicate could not promise.
const MIN_GAIN: usize = 2;

/// How far ahead of a pan the region centre is biased, in seconds of the
/// current pan velocity — coverage shifts toward where the view is heading
/// without ever changing the region's (instance-keyed) size.
const LOOKAHEAD_SECS: f32 = 0.3;

/// Pan-velocity EMA time constant (seconds): fast enough to catch a flick,
/// slow enough that the bias doesn't jitter the snapped origin.
const VEL_TAU: f32 = 0.2;

/// Soft ceiling on region textures kept alive across all panes, in bytes of
/// texture memory (`dims.x * dims.y * 4` each). Beyond it the least-recently
/// used regions are dropped (their `TextureHandle` frees the texture), so
/// panning around a paused frame keeps a generous trail of revisitable spots
/// without growing without bound.
const REGION_CACHE_BYTES: usize = 256 << 20;

/// The largest power of two ≤ `step` (`step ≥ 1`) — the region path's
/// quantisation of `stage_step`. Rendering at the next power of two **down**
/// keeps the region at least as sharp as the screen needs, and makes zoom
/// re-render (and re-key the cache, and re-size the operator input) only at
/// band crossings instead of every integer step.
pub(super) fn roi_step(step: usize) -> usize {
    let step = step.max(1);
    1 << (usize::BITS - 1 - step.leading_zeros())
}

/// The decimation step for a pane's whole-image **base** texture while adaptive
/// rendering is active: the zoom's own step, but at least enough to bring the
/// image inside the [`BASE_MAX`] cap.
///
/// With `ops` (a proprietary-operator pane, which in adaptive mode accepts
/// reduced input by design) it is rounded **up** to a power of two, so the
/// size-keyed C++ instance for the base rebuilds only at band crossings rather
/// than continuously while zooming.
///
/// The single definition of the base's cost, so `want_step` (which renders it)
/// and [`gain`] (which weighs it) can never disagree about how big the base is —
/// a base capped without a region over it is precisely the "permanently blurry
/// pane" failure.
pub(super) fn base_step(stage_step: usize, size: [usize; 2], max_side: usize, ops: bool) -> usize {
    let step = stage_step.max(decode::texture_fit_step(size, max_side.min(BASE_MAX)));
    if ops {
        step.next_power_of_two()
    } else {
        step
    }
}

/// How much work adaptive rendering saves over the whole-image render it
/// replaces, as a ratio: the classic path's own decimated texel count over the
/// two layers' (`region + base`) together. The mode engages at [`MIN_GAIN`] or
/// better.
///
/// Comparing decimated against decimated is what makes this like-for-like at any
/// zoom. Zoomed out the region approaches the whole image and the ratio
/// collapses, which is exactly when adaptive rendering should get out of the
/// way. Pure, so `activation_engages_only_when_it_saves_work` pins the real
/// arithmetic rather than a copy of it.
pub(super) fn gain(size: [usize; 2], stage_step: usize, dims: [usize; 2], base: usize) -> f64 {
    let classic = texels(decode::decimated_size(size, stage_step));
    let adaptive = texels(dims) + texels(decode::decimated_size(size, base));
    classic as f64 / (adaptive.max(1) as f64)
}

/// Region size in output texels at `step`: `COVER` × the visible image-space
/// span (rotation-aware) per axis, plus **two** `QUANTUM` so the origin snap
/// can never uncover a viewport edge, rounded up to `QUANTUM` multiples and
/// clamped to the image. A function of the pane size and zoom band only —
/// **never** of pan speed — so the operator-input size stays constant while
/// panning.
///
/// The snap allowance must be `2 * QUANTUM`, not one: snapping the origin down
/// loses up to `QUANTUM - 1` texels of trailing coverage, and at high zoom the
/// visible span (a few dozen image px in a grid cell) is far smaller than that
/// loss — with a single-`QUANTUM` allowance the region's far edge landed right
/// at the view centre and the bottom half of the pane stayed on the blurry
/// base (`region_covers_the_viewport` locks the invariant down).
pub(super) fn region_dims(span: Vec2, cover: f32, step: usize, img: [usize; 2]) -> [usize; 2] {
    let os = decode::decimated_size(img, step);
    let step_f = step.max(1) as f32;
    let mut dims = [0usize; 2];
    for (a, span) in [span.x, span.y].into_iter().enumerate() {
        let out = (span * cover / step_f).ceil().max(1.0) as usize;
        let quantized = (out + 2 * QUANTUM).div_ceil(QUANTUM) * QUANTUM;
        dims[a] = quantized.min(os[a]);
    }
    dims
}

/// The visible image-space span of pane rect `cell` at `zoom`, widened to the
/// axis-aligned bounds when the pane is rotated by `theta` (over-covering at
/// diagonal angles, which the region margin absorbs). This is the quantity both
/// the activation predicate and the region geometry are built on.
pub(super) fn visible_span(cell: Vec2, zoom: f32, theta: f32) -> Vec2 {
    let d = cell / zoom.max(1e-6);
    if theta == 0.0 {
        return d;
    }
    let (s, c) = (theta.sin().abs(), theta.cos().abs());
    Vec2::new(d.x * c + d.y * s, d.x * s + d.y * c)
}

/// The image point at the centre of pane rect `cell` — what the region must be
/// centred on.
///
/// On an unrotated pane this is just the view centre. **Rotated it is not**, and
/// that was the bug: the pane rotates about the *image* centre's screen
/// position, not about the view centre, so the image point under the middle of
/// the cell is `v.center` turned by `-theta` about the image centre. The
/// displacement is `2·sin(theta/2)` times the distance from the image centre —
/// zero when centred, hundreds of pixels once panned away, and *changing with
/// the angle*, which is exactly how it showed up: patches of the pane falling
/// back to the blurry base, shifting as the image was turned.
///
/// This is the inverse of what `paint_rotated_about` does when it draws the
/// region, so the two cannot drift.
pub(super) fn view_center(
    v: &crate::view::ViewTransform,
    disp: [usize; 2],
    cell: Rect,
    theta: f32,
) -> Vec2 {
    canvas::unrotate_screen_to_img(v, disp, cell, cell.center(), theta)
}

/// Texels in an `[w, h]` output.
fn texels(d: [usize; 2]) -> usize {
    d[0] * d[1]
}

/// The largest velocity bias (image px, per axis) that still leaves the
/// **unbiased** viewport fully covered after the origin snap: half the
/// region's slack over the visible span, minus one `QUANTUM` of snap loss.
/// Bounding the bias by this — rather than by the viewport — matters because
/// the velocity estimate can freeze between paced repaints (an idle app stops
/// sampling), and an over-shifted region would then leave a *persistent*
/// blurry trail behind the last pan.
pub(super) fn bias_limit(dims: [usize; 2], span: Vec2, step: usize) -> Vec2 {
    let s = step.max(1) as f32;
    let limit =
        |d: usize, sp: f32| ((((d * step.max(1)) as f32) - sp) / 2.0 - QUANTUM as f32 * s).max(0.0);
    Vec2::new(limit(dims[0], span.x), limit(dims[1], span.y))
}

/// Region origin in output texels: centred on `center` (image px — the view
/// centre, plus any velocity bias), snapped **down** to the `QUANTUM` grid, and
/// slid to stay inside the image (so `dims` never has to shrink — the far-edge
/// clamp moves the region, keeping the operator-instance size stable).
pub(super) fn region_origin(
    center: Vec2,
    dims: [usize; 2],
    img: [usize; 2],
    step: usize,
) -> [usize; 2] {
    let os = decode::decimated_size(img, step);
    let step_f = step.max(1) as f32;
    let mut origin = [0usize; 2];
    for (a, c) in [center.x, center.y].into_iter().enumerate() {
        let want = (c / step_f - dims[a] as f32 / 2.0).max(0.0) as usize;
        origin[a] = (want / QUANTUM * QUANTUM).min(os[a] - dims[a]);
    }
    origin
}

/// Pan-velocity tracker for one pane's view centre (image px/s, EMA). Drives
/// the region's directional bias; deliberately **not** part of the region size.
#[derive(Default)]
pub(super) struct PanVel {
    /// Last sampled `(time, view centre)`.
    last: Option<(f64, Vec2)>,
    vel: Vec2,
}

impl PanVel {
    /// Fold in the view centre at time `now` (seconds), returning the updated
    /// velocity estimate. A long gap (the pane wasn't being updated) resets the
    /// estimate rather than reading as a huge jump.
    pub(super) fn sample(&mut self, now: f64, center: Vec2) -> Vec2 {
        if let Some((t, c)) = self.last {
            let dt = (now - t) as f32;
            if dt > 1.0 {
                self.vel = Vec2::ZERO;
            } else if dt > 0.0 {
                let inst = (center - c) / dt;
                let k = (dt / VEL_TAU).min(1.0);
                self.vel = self.vel + (inst - self.vel) * k;
            }
        }
        self.last = Some((now, center));
        self.vel
    }

    /// The region-centre bias for the current estimate: `LOOKAHEAD_SECS` of
    /// travel, clamped per axis to `max` ([`bias_limit`] — the most the region
    /// can shift while still covering the unbiased viewport).
    pub(super) fn bias(&self, max: Vec2) -> Vec2 {
        Vec2::new(
            (self.vel.x * LOOKAHEAD_SECS).clamp(-max.x, max.x),
            (self.vel.y * LOOKAHEAD_SECS).clamp(-max.y, max.y),
        )
    }
}

/// The region geometry a pane wants right now (see `CimApp::roi_plan`): the
/// decimation band, the output size, the visible image-space span it was sized
/// from (which `bias_limit` needs), and the image point to centre on.
/// Deliberately view-derived only — no frame or tone identity, which
/// `RegionKey` adds.
#[derive(Clone, Copy, Debug)]
pub(super) struct RegionPlan {
    pub step: usize,
    pub dims: [usize; 2],
    pub span: Vec2,
    /// The image point under the middle of the pane — the view centre, turned by
    /// the pane's rotation (see [`view_center`]).
    pub center: Vec2,
}

/// Identity of one cached region texture. `uid` is the frame's process-unique
/// id (`FrameData::uid` — immune to Arc-address aliasing after cache eviction),
/// `sig` the pane's tone signature, and `step`/`origin`/`dims` the region
/// geometry in output-texel space. A view/tone/frame change simply stops
/// matching; stale entries age out by LRU.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(super) struct RegionKey {
    /// Owning pane id. Regions are per-pane rather than shared between panes
    /// showing the same frame: it lets a pane retire its own stale-frame
    /// regions on insert (see `RegionCache::retire_stale`), which is what keeps
    /// a playing sequence's cache bounded and its texture handles recyclable.
    pub pane: u64,
    pub uid: u64,
    pub sig: u64,
    pub step: usize,
    pub origin: [usize; 2],
    pub dims: [usize; 2],
}

impl RegionKey {
    /// What to render for this key: the same rect in **source** pixels, which is
    /// also what the region's texels sample (`origin * step`).
    pub(super) fn region(&self) -> media::Region {
        media::Region {
            origin: [self.origin[0] * self.step, self.origin[1] * self.step],
            out: self.dims,
            step: self.step,
        }
    }

    /// Texture bytes this region occupies (RGBA).
    fn bytes(&self) -> usize {
        self.dims[0] * self.dims[1] * 4
    }
}

/// App-global LRU of finished region textures, byte-budgeted on texture memory
/// (the `SeqCache` idiom: a recency-ordered set over `CimApp.clock` ticks, so
/// eviction picks the globally oldest region without scanning). Generic over
/// the stored value so the LRU/accounting logic tests headlessly.
pub(super) struct RegionCache<V = TextureHandle> {
    map: HashMap<RegionKey, (V, u64)>,
    /// `(tick, key)` in recency order — first = least recently used.
    lru: std::collections::BTreeSet<(u64, RegionKey)>,
    bytes: usize,
}

impl<V> Default for RegionCache<V> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            lru: std::collections::BTreeSet::new(),
            bytes: 0,
        }
    }
}

impl<V> RegionCache<V> {
    /// The cached value for `key`, if resident (recency untouched — see `touch`).
    pub(super) fn get(&self, key: &RegionKey) -> Option<&V> {
        self.map.get(key).map(|(v, _)| v)
    }

    /// Mark `key` as used at `clock` so it survives eviction longest.
    pub(super) fn touch(&mut self, key: &RegionKey, clock: u64) {
        if let Some((_, tick)) = self.map.get_mut(key) {
            self.lru.remove(&(*tick, *key));
            *tick = clock;
            self.lru.insert((clock, *key));
        }
    }

    /// Insert (or replace) `key`'s value, accounting its bytes.
    pub(super) fn insert_value(&mut self, key: RegionKey, value: V, clock: u64) {
        if let Some((_, tick)) = self.map.remove(&key) {
            self.lru.remove(&(tick, key));
            self.bytes -= key.bytes();
        }
        self.map.insert(key, (value, clock));
        self.lru.insert((clock, key));
        self.bytes += key.bytes();
    }

    /// Drop the entries `key`'s pane keeps for a **different frame or tone** —
    /// they can never be wanted again — and hand back one same-sized value to
    /// recycle. Called on every insert, which is what bounds the cache while a
    /// sequence plays (each frame supersedes the last) without touching the
    /// paused case, where the frame and tone hold still and the pane's regions
    /// at other origins *are* the pan-back cache.
    ///
    /// A region a pane is **currently showing** (`protect`) is never retired,
    /// however stale its frame: staging runs ahead of the commit, so between an
    /// async region landing for frame N+1 and the group actually flipping to it,
    /// `region_show` still points at frame N's — retiring that out from under
    /// the painter dropped the pane to its heavily decimated base for a frame,
    /// which read as flickering between low and high resolution wherever
    /// regions were large enough to render off-thread.
    ///
    /// This is deliberately not "evict the least-recently-used entry": doing
    /// that on insert let each pane's insert throw away the previous pane's
    /// region, so with more than one pane the cache never held more than one
    /// entry, every pane missed every frame, and — since regions gate the
    /// lock-step commit — playback stopped dead.
    ///
    /// Scans the whole key set. Deliberately: the map holds tens of entries in
    /// practice (a pane's pan trail, bounded by [`REGION_CACHE_BYTES`]), and a
    /// per-pane index would be a second structure to keep consistent with the
    /// LRU for a scan that has never shown up in a profile.
    fn retire_stale(&mut self, key: &RegionKey, protect: &[RegionKey]) -> Option<V> {
        let stale: Vec<RegionKey> = self
            .map
            .keys()
            .filter(|k| {
                k.pane == key.pane && (k.uid != key.uid || k.sig != key.sig) && !protect.contains(k)
            })
            .copied()
            .collect();
        let mut reclaimed = None;
        for k in stale {
            if let Some((v, tick)) = self.map.remove(&k) {
                self.lru.remove(&(tick, k));
                self.bytes -= k.bytes();
                if reclaimed.is_none() && k.dims == key.dims {
                    reclaimed = Some(v);
                }
            }
        }
        reclaimed
    }

    /// Drop least-recently-used regions until within `budget` bytes, never
    /// touching one a pane is currently showing (`protect`) — the same rule
    /// `SeqCache::lru_evictable` follows for the shown frame, and for the same
    /// reason: evicting what is on screen makes the pane fall back to the
    /// heavily decimated base for a frame.
    pub(super) fn enforce(&mut self, budget: usize, protect: &[RegionKey]) {
        while self.bytes > budget {
            let victim = self.lru.iter().find(|(_, k)| !protect.contains(k)).copied();
            let Some((tick, key)) = victim else {
                break; // everything left is on screen
            };
            self.lru.remove(&(tick, key));
            self.map.remove(&key);
            self.bytes -= key.bytes();
        }
    }

    /// Resident texture bytes (the debug window reports it).
    pub(super) fn resident_bytes(&self) -> usize {
        self.bytes
    }

    /// Drop every region belonging to `pane` — it was closed or reloaded, so
    /// nothing it cached can ever be wanted again and `retire_stale` (which only
    /// runs on that pane's own next insert) will never come for them.
    pub(super) fn forget_pane(&mut self, pane: u64) {
        let gone: Vec<RegionKey> = self
            .map
            .keys()
            .filter(|k| k.pane == pane)
            .copied()
            .collect();
        for k in gone {
            if let Some((_, tick)) = self.map.remove(&k) {
                self.lru.remove(&(tick, k));
                self.bytes -= k.bytes();
            }
        }
    }

    /// Drop everything (reload / operator library change — anything that makes
    /// every cached render stale at once).
    pub(super) fn clear(&mut self) {
        self.map.clear();
        self.lru.clear();
        self.bytes = 0;
    }
}

impl RegionCache<TextureHandle> {
    /// Store `img` under `key`, retiring the pane's stale-frame regions and
    /// **reusing a texture handle** when one of the same size is available: the
    /// entry already at `key` (a re-render of the same region), else one of the
    /// stale entries just retired — during playback that is the previous
    /// frame's region for this pane, which is the same size by construction.
    /// Writing into a live handle (`TextureHandle::set`) queues a texture delta
    /// instead of allocating and freeing a texture per pane per frame.
    fn insert(
        &mut self,
        ctx: &egui::Context,
        key: RegionKey,
        img: ColorImage,
        clock: u64,
        protect: &[RegionKey],
    ) {
        let existing = self.map.get(&key).map(|(h, _)| h.clone());
        // Always retire, whether or not the handle is reusable, so the pane
        // keeps at most its current frame's regions while playing.
        let stale = self.retire_stale(&key, protect);
        let recycled = existing
            .filter(|h| h.size() == img.size)
            .or_else(|| stale.filter(|h| h.size() == img.size));
        match recycled {
            Some(mut h) => {
                h.set(img, TextureOptions::NEAREST);
                self.insert_value(key, h, clock);
            }
            None => {
                let name = format!("roi{}x{}", key.dims[0], key.dims[1]);
                let h = ctx.load_texture(name, img, TextureOptions::NEAREST);
                self.insert_value(key, h, clock);
            }
        }
    }
}

impl CimApp {
    /// The region geometry pane `idx` would use right now, or `None` when
    /// adaptive rendering shouldn't engage. Every caller — `want_step`, the
    /// staging pass and the commit — goes through this one function, so they
    /// cannot disagree about whether a pane is adaptive or how big its region
    /// is (a capped base with no region over it is precisely the "permanently
    /// blurry pane" failure).
    ///
    /// It declines when the setting is off, when the view hasn't been fitted yet
    /// (a pre-fit pane is still at zoom 1 on a possibly huge image), before the
    /// pane's first draw (nothing to size a region against), when the region
    /// would exceed the backend's texture limit, and — the substantive test —
    /// when the two layers wouldn't save [`MIN_GAIN`]x the work of the
    /// whole-image render they replace (see [`gain`]).
    ///
    /// **Playing is the target case, not an exclusion.** The classic path
    /// re-renders and re-uploads the whole image every frame however little of it
    /// is on screen, and that is the frame-rate ceiling this mode exists to lift;
    /// playback only shortens the region's pan runway (`COVER_PLAYING`), since
    /// margin rendered for a frame that is about to be replaced is wasted.
    /// Operator panes qualify like any other — the region is exactly how their
    /// reduced input gets fed.
    pub(super) fn roi_plan(
        &self,
        idx: usize,
        target: usize,
        ppp: f32,
        max_side: usize,
    ) -> Option<RegionPlan> {
        if !self.config.adaptive_render || self.view_ref(idx).needs_fit {
            return None;
        }
        // The pane's screen rect, as of the last frame that drew it (see
        // `Pane::cell`). Zero before the first draw — nothing to size against.
        let cell = self.panes[idx].cell;
        if cell.width() < 1.0 || cell.height() < 1.0 {
            return None;
        }
        let size = self.staged_size(idx, target);
        let v = self.view_ref(idx);
        let theta = self.pane_theta(idx);
        let stage_step = self.stage_step(idx, ppp);
        let step = roi_step(stage_step);
        let cover = if self.playback.playing {
            COVER_PLAYING
        } else {
            COVER_PAUSED
        };
        let span = visible_span(cell.size(), v.zoom, theta);
        let dims = region_dims(span, cover, step, size);

        // A region past the backend's limit could never be uploaded, and since
        // the commit waits on it that would stall the pane rather than just look
        // blurry. Unreachable on a real backend (16384 against a viewport-sized
        // region); it guards the conservative 2048 fallback used before the
        // backend reports its limit.
        if dims[0] > max_side || dims[1] > max_side {
            return None;
        }
        // The base `want_step` will actually render (an operator pane takes the
        // reduced base whenever the setting is on, so `ops` alone decides there).
        let base = base_step(stage_step, size, max_side, self.pane_ops_active(idx));
        (gain(size, stage_step, dims, base) >= MIN_GAIN as f64).then(|| RegionPlan {
            step,
            dims,
            span,
            // The image point under the middle of the pane — `v.center` only on
            // an unrotated pane (see `view_center`).
            center: view_center(v, size, cell, theta),
        })
    }

    /// Enforce the region-cache byte budget. Called from the frame cache's own
    /// budget enforcement each update, after the textures have settled — the
    /// regions themselves are staged with the base (`stage_region`).
    pub(super) fn enforce_region_budget(&mut self) {
        let protect = self.live_regions();
        self.regions.enforce(REGION_CACHE_BYTES, &protect);
    }

    /// The regions the panes are drawing right now **or are about to** — off
    /// limits to eviction and retirement. Dropping a shown one blanks that pane's
    /// sharp layer until the next commit; dropping a staged-but-uncommitted one
    /// (`region_want`, still waiting on a slower pane in the group) merely throws
    /// away work that is about to be asked for again, but there is no reason to
    /// pay for it.
    fn live_regions(&self) -> Vec<RegionKey> {
        self.panes
            .iter()
            .flat_map(|p| [p.region_show, p.region_want])
            .flatten()
            .collect()
    }

    /// Stage pane `idx`'s viewport region for frame `target` under `plan` (the
    /// pane's `roi_plan`, computed once by the caller), returning whether it is
    /// ready. Called from `refresh_textures`' staging loop right beside `stage`,
    /// so the region is part of the **lock-step commit**: a pane counts as ready
    /// only when its base *and* its region are both in hand, and the group flips
    /// them together.
    ///
    /// That coupling is what keeps a playing sequence from flickering. The base
    /// is capped at [`BASE_MAX`] while the region path is active, so a region
    /// arriving a frame late would show a heavily blurred frame in the middle of
    /// otherwise sharp playback — far worse than pacing playback on the work it
    /// actually takes, which is how decode and the heavy operators already pace
    /// it. The staged key lands in `Pane::region_want`; the commit promotes it to
    /// `region_show` (which drawing reads), exactly as `pending` becomes `front`,
    /// so a half-updated pane can never pair one frame's base with another
    /// frame's region.
    ///
    /// A pane that isn't adaptive right now reports ready with no region; so does
    /// an errored one, and one whose frame isn't decoded yet — `stage` has
    /// already reported *that* as not-ready and requested the decode, and
    /// reporting it twice would only confuse the reason.
    pub(super) fn stage_region(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        target: usize,
        plan: Option<RegionPlan>,
        now: f64,
    ) -> bool {
        // Since regions gate the commit, anything that can't produce one must
        // stand aside rather than hold every pane's timeline still.
        let errored = self.panes[idx].error.is_some() || self.panes[idx].tex_error.is_some();
        let frame = self.panes[idx].media.resident(target).filter(|_| !errored);
        let (Some(plan), Some(frame)) = (plan, frame) else {
            self.panes[idx].region_want = None;
            return true;
        };
        // Sampled once per pane per update (this is the only caller), so the
        // velocity estimate tracks real elapsed time. The bias is clamped so the
        // *unbiased* viewport stays covered even if the estimate freezes between
        // paced repaints.
        let pan = &mut self.panes[idx].pan_vel;
        pan.sample(now, plan.center);
        let bias = pan.bias(bias_limit(plan.dims, plan.span, plan.step));
        let origin = region_origin(plan.center + bias, plan.dims, frame.size, plan.step);
        let key = RegionKey {
            pane: self.panes[idx].id,
            uid: frame.uid(),
            sig: self.tone_sig(idx),
            step: plan.step,
            origin,
            dims: plan.dims,
        };
        self.panes[idx].region_want = Some(key);
        if self.regions.get(&key).is_some() {
            self.regions.touch(&key, self.clock);
            return true;
        }
        // A synchronous render leaves the region in the cache, so the pane is
        // ready in this same update — reporting otherwise would cost the commit
        // (and so every playback frame) a needless extra round of staging.
        self.render_region(ctx, idx, target, &frame, key)
    }

    /// Render the region `key` describes for pane `idx`, or leave it to the
    /// render already in flight for this pane. Routing mirrors `stage`: the
    /// proprietary operators (whose instances belong to the pane's worker
    /// thread) and any large output go off-thread; a small plain region renders
    /// inline, where the worker round-trip would cost more than the render.
    ///
    /// Returns whether the region is in the cache when this returns — true for
    /// the inline path, false when it was handed to the worker.
    fn render_region(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        target: usize,
        frame: &std::sync::Arc<media::FrameData>,
        key: RegionKey,
    ) -> bool {
        let id = self.panes[idx].id;
        let contrast = self.contrast_of(idx);
        let ops = self.ops_of(idx);
        let cmap = crate::tone::uses_colormap(contrast, frame);
        let heavy = !cmap && crate::imageproc::ops_active(frame, ops);
        let (lo, hi) = self.tone_bounds(idx, frame);
        let tone = crate::imageproc::Display {
            lo,
            hi,
            // Carried into the job as well as used inline: a Colormap region big
            // enough to render off-thread must still come back false-coloured.
            palette: cmap.then(|| self.tone_of(idx).palette),
            ops,
        };
        let region = key.region();

        if heavy || region.texels() >= ASYNC_RENDER_PIXELS {
            // One region render per pane at a time — keyed by **pane**, not by
            // the region's identity, so a playing sequence (whose key changes
            // every frame) can't queue a job per frame behind the pane's base
            // render. That is `RenderPool`'s documented invariant. The key is
            // filed here and read back in `land_region`, so a result never has
            // to be re-derived from the geometry it was rendered at.
            if let std::collections::hash_map::Entry::Vacant(e) = self.roi_inflight.entry(id) {
                e.insert(key);
                self.renderer.request(crate::renderer::RenderJob {
                    id,
                    frame: target,
                    sig: key.sig,
                    data: frame.clone(),
                    tone,
                    region,
                    target: crate::renderer::Target::Viewport,
                });
            }
            return false;
        }

        let debug = crate::debug::enabled();
        let t = debug.then(std::time::Instant::now);
        // Rendered straight into egui's pixel type (see `media::RgbaSink`): this
        // buffer *becomes* the region texture's `ColorImage`, so there is no
        // conversion pass between the tone map and the upload. Only the inline
        // path reaches here, and it is taken only for outputs below
        // `ASYNC_RENDER_PIXELS`, so the allocation is small by construction.
        let mut pixels = Vec::new();
        let lut = &mut self.panes[idx].tex.lut;
        crate::cpu::install(|| match tone.palette {
            Some(pal) => frame.render_cmap(lo, hi, region, pal, lut, &mut pixels),
            None => frame.render_lut(lo, hi, region, lut, &mut pixels),
        });
        if let Some(t) = t {
            self.metrics.lut.record(t.elapsed());
        }
        let img = ColorImage {
            size: region.out,
            pixels,
        };
        self.upload_region(ctx, key, img);
        true
    }

    /// Put a finished region image in the cache under `key`, reusing this pane's
    /// existing texture handle when the size matches. Reuse matters most exactly
    /// where the cache is least useful — during playback every frame needs a new
    /// region, and allocating a texture per pane per frame is the churn the base
    /// path's own handle reuse (`set_cached_tex`) exists to avoid.
    pub(super) fn upload_region(&mut self, ctx: &egui::Context, key: RegionKey, img: ColorImage) {
        let t = crate::debug::enabled().then(std::time::Instant::now);
        let protect = self.live_regions();
        self.regions.insert(ctx, key, img, self.clock, &protect);
        if let Some(t) = t {
            self.metrics.upload.record(t.elapsed());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roi_step_is_prev_power_of_two() {
        for (s, want) in [(1, 1), (2, 2), (3, 2), (4, 4), (7, 4), (8, 8), (13, 8)] {
            assert_eq!(roi_step(s), want, "step {s}");
        }
        assert_eq!(roi_step(0), 1); // degenerate input clamps
    }

    /// The base step honours the BASE_MAX cap, and for an operator pane lands on
    /// a power of two so the operator's base-input size only changes at band
    /// crossings.
    #[test]
    fn base_step_caps_and_pow2s_for_ops() {
        // 25000² over a 16384 backend: fit at BASE_MAX(256) is ceil(25000/256)=98.
        assert_eq!(base_step(1, [25000, 25000], 16384, false), 98);
        assert_eq!(base_step(1, [25000, 25000], 16384, true), 128);
        // The zoom's own step only wins once it exceeds the cap's.
        assert_eq!(base_step(20, [25000, 25000], 16384, true), 128);
        assert_eq!(base_step(200, [25000, 25000], 16384, true), 256);
        // Even a modest image decimates for the base — it is occluded by the
        // region, and this is the per-frame cost during playback.
        assert_eq!(base_step(1, [1000, 1000], 16384, true), 4);
        assert_eq!(base_step(1, [1000, 1000], 16384, false), 4);
    }

    /// Region dims are QUANTUM multiples covering COVER viewports (plus the
    /// snap allowance), clamped to the image; origin snaps to the QUANTUM grid
    /// and slides at the far edge instead of shrinking.
    #[test]
    fn region_geometry_snaps_and_clamps() {
        let img = [10000, 8000];
        // A 1000×500 image-px span, paused (COVER 2), step 1:
        // 2000+256 → ceil to 128s = 2304; 1000+256 → 1280.
        let dims = region_dims(Vec2::new(1000.0, 500.0), COVER_PAUSED, 1, img);
        assert_eq!(dims, [2304, 1280]);
        for d in dims {
            assert_eq!(d % QUANTUM, 0);
        }

        // Centre in the middle: origin on the QUANTUM grid, roughly centred.
        let o = region_origin(Vec2::new(5000.0, 4000.0), dims, img, 1);
        assert_eq!([o[0] % QUANTUM, o[1] % QUANTUM], [0, 0]);
        assert!(o[0] <= 5000 - dims[0] / 2 && o[0] + dims[0] >= 5000 + dims[0] / 2 - QUANTUM);

        // Centre at the far corner: the region slides to stay inside, keeping
        // its dims (may leave the QUANTUM grid — the edge clamp wins).
        let o = region_origin(Vec2::new(9990.0, 7990.0), dims, img, 1);
        assert_eq!(o, [10000 - dims[0], 8000 - dims[1]]);

        // Centre at the origin corner: clamps to zero.
        assert_eq!(region_origin(Vec2::new(3.0, 4.0), dims, img, 1), [0, 0]);

        // A viewport larger than the image: dims clamp to the whole image.
        let dims = region_dims(Vec2::new(1000.0, 500.0), COVER_PAUSED, 1, [600, 300]);
        assert_eq!(dims, [600, 300]);
        assert_eq!(
            region_origin(Vec2::new(300.0, 150.0), dims, [600, 300], 1),
            [0, 0]
        );

        // Decimated: dims/origin are in the step-2 out space.
        let img = [10000, 8000];
        let dims = region_dims(Vec2::new(2000.0, 1000.0), COVER_PAUSED, 2, img);
        assert_eq!(dims, [2304, 1280]); // 2000×1000 image px → /2 → the same texels
        let o = region_origin(Vec2::new(5000.0, 4000.0), dims, img, 2);
        assert!(o[0] + dims[0] <= 5000 && o[1] + dims[1] <= 4000);

        // Playing halves the cover: the same span asks for far fewer texels,
        // which is the point of the mode during playback.
        let play = region_dims(Vec2::new(1000.0, 500.0), COVER_PLAYING, 1, img);
        assert!(texels(play) * 2 < texels(dims), "{play:?} vs {dims:?}");
    }

    /// The invariant the geometry exists to uphold: whatever the zoom, pane
    /// size, step, or (clamped) velocity bias, the snapped region covers the
    /// whole visible span. Includes the reported regression: at zoom ~5.4 in a
    /// grid cell the visible span is only ~91 image px, and with a
    /// single-`QUANTUM` snap allowance the region's bottom edge (1536) landed
    /// right at the view centre (1535) — the bottom half of the pane stayed on
    /// the blurry base.
    #[test]
    fn region_covers_the_viewport() {
        let cases = [
            // (img, span, centre, step) — first is the reported bug's setup.
            (
                [2560usize, 2160],
                Vec2::new(118.6, 91.4),
                Vec2::new(764.83, 1535.31),
                1usize,
            ),
            (
                [2560, 2160],
                Vec2::new(1500.0, 900.0),
                Vec2::new(1280.0, 1080.0),
                1,
            ),
            (
                [10000, 8000],
                Vec2::new(600.0, 400.0),
                Vec2::new(2000.0, 1700.0),
                2,
            ),
            (
                [2560, 2160],
                Vec2::new(200.0, 150.0),
                Vec2::new(30.0, 2100.0),
                1,
            ),
            (
                [2560, 2160],
                Vec2::new(93.0, 93.0),
                Vec2::new(2555.0, 5.0),
                1,
            ),
        ];
        for (img, span, center, step) in cases {
            for cover in [COVER_PAUSED, COVER_PLAYING] {
                let dims = region_dims(span, cover, step, img);
                let lim = bias_limit(dims, span, step);
                for bias in [Vec2::ZERO, lim, -lim, Vec2::new(lim.x, -lim.y)] {
                    let o = region_origin(center + bias, dims, img, step);
                    let (rx0, ry0) = ((o[0] * step) as f32, (o[1] * step) as f32);
                    let (rx1, ry1) = (
                        ((o[0] + dims[0]) * step) as f32,
                        ((o[1] + dims[1]) * step) as f32,
                    );
                    // The visible span, clamped to the image (the region can't
                    // cover what isn't there).
                    let vx0 = (center.x - span.x / 2.0).max(0.0);
                    let vy0 = (center.y - span.y / 2.0).max(0.0);
                    let vx1 = (center.x + span.x / 2.0).min(img[0] as f32);
                    let vy1 = (center.y + span.y / 2.0).min(img[1] as f32);
                    assert!(
                    rx0 <= vx0 && ry0 <= vy0 && rx1 >= vx1 && ry1 >= vy1,
                    "img {img:?} span {span:?} centre {center:?} step {step} bias {bias:?}: \
                     region ({rx0},{ry0})..({rx1},{ry1}) misses view ({vx0},{vy0})..({vx1},{vy1})                      at cover {cover}"
                );
                }
            }
        }
    }

    /// The velocity EMA tracks a steady pan, and the bias clamps to the given
    /// maximum instead of following a flick off-screen.
    #[test]
    fn pan_velocity_tracks_and_clamps() {
        let mut pv = PanVel::default();
        let mut vel = Vec2::ZERO;
        // 100 px per 16 ms step rightward → ~6250 px/s once the EMA settles.
        for i in 0..60 {
            let t = i as f64 * 0.016;
            vel = pv.sample(t, Vec2::new(100.0 * i as f32, 0.0));
        }
        assert!((vel.x - 6250.0).abs() < 100.0, "vel {vel:?}");
        assert_eq!(vel.y, 0.0);

        // Bias looks ahead but clamps to the viewport.
        assert_eq!(pv.bias(Vec2::new(800.0, 600.0)).x, 800.0);

        // A long gap resets the estimate (the pane wasn't updating).
        let v = pv.sample(10.0, Vec2::new(6000.0, 0.0));
        assert_eq!(v, Vec2::ZERO);
    }

    /// Inserting a region must never cost a *different* pane its own. This is
    /// the regression that stopped playback dead: recycling texture handles by
    /// popping the least-recently-used entry meant each pane's insert evicted
    /// the previous pane's region, so with more than one pane the cache held a
    /// single entry, every pane missed every frame, and — regions gating the
    /// lock-step commit — the timeline never advanced again.
    #[test]
    fn insert_retires_only_the_pane_s_own_stale_regions() {
        let key = |pane: u64, uid: u64, origin: usize| RegionKey {
            pane,
            uid,
            sig: 7,
            step: 1,
            origin: [origin, 0],
            dims: [256, 256],
        };
        let bytes = 256 * 256 * 4;
        let mut cache = RegionCache::<u32>::default();

        // Six panes each stage a region for the same frame, as one update of a
        // six-pane grid does.
        for pane in 0..6u64 {
            let k = key(pane, 100, 0);
            assert!(
                cache.retire_stale(&k, &[]).is_none(),
                "pane {pane} retired something"
            );
            cache.insert_value(k, pane as u32, 10);
        }
        assert_eq!(cache.resident_bytes(), 6 * bytes);
        for pane in 0..6u64 {
            assert_eq!(
                cache.get(&key(pane, 100, 0)),
                Some(&(pane as u32)),
                "pane {pane} lost its region to another pane's insert"
            );
        }

        // Paused panning: same frame, new origins — every one is kept, because
        // that trail *is* the pan-back cache.
        for (n, origin) in [256usize, 512, 768].iter().enumerate() {
            let k = key(0, 100, *origin);
            assert!(cache.retire_stale(&k, &[]).is_none());
            cache.insert_value(k, 90 + n as u32, 11);
        }
        assert_eq!(cache.get(&key(0, 100, 512)), Some(&91));
        assert_eq!(cache.get(&key(0, 100, 0)), Some(&0));

        // Playback steps pane 0 to a new frame: its four stale regions go (one
        // handed back to recycle), every other pane is untouched.
        let next = key(0, 101, 0);
        assert!(
            cache.retire_stale(&next, &[]).is_some(),
            "a same-sized value should be reclaimable"
        );
        cache.insert_value(next, 42, 12);
        assert_eq!(cache.get(&next), Some(&42));
        assert!(cache.get(&key(0, 100, 0)).is_none());
        assert!(cache.get(&key(0, 100, 512)).is_none());
        for pane in 1..6u64 {
            assert_eq!(cache.get(&key(pane, 100, 0)), Some(&(pane as u32)));
        }
        // Bytes track the retirement exactly: 5 other panes + pane 0's new one.
        assert_eq!(cache.resident_bytes(), 6 * bytes);

        // A tone change retires the pane's regions just as a frame change does.
        let toned = RegionKey { sig: 8, ..next };
        assert!(cache.retire_stale(&toned, &[]).is_some());
        assert!(cache.get(&next).is_none());
    }

    /// A region a pane is showing survives both retirement and eviction. This
    /// is the flicker regression: staging runs ahead of the commit, so an async
    /// region landing for the next frame retired the one still on screen, and
    /// the pane dropped to its heavily decimated base until the group flipped —
    /// low res, high res, low res, wherever regions were big enough to render
    /// off-thread (roughly the 1.0–1.5× band).
    #[test]
    fn a_region_being_shown_is_never_dropped() {
        let key = |uid: u64| RegionKey {
            pane: 3,
            uid,
            sig: 1,
            step: 1,
            origin: [0, 0],
            dims: [256, 256],
        };
        let bytes = 256 * 256 * 4;
        let mut cache = RegionCache::<u32>::default();
        cache.insert_value(key(10), 10, 1); // committed, on screen
        let shown = [key(10)];

        // The next frame's region lands before the commit: the shown one stays.
        assert!(cache.retire_stale(&key(11), &shown).is_none());
        cache.insert_value(key(11), 11, 2);
        assert_eq!(cache.get(&key(10)), Some(&10), "shown region was retired");
        assert_eq!(cache.get(&key(11)), Some(&11));

        // Nor may the budget evict it, however far over that budget we are.
        cache.enforce(0, &shown);
        assert_eq!(cache.get(&key(10)), Some(&10), "shown region was evicted");
        assert!(
            cache.get(&key(11)).is_none(),
            "unprotected region should go"
        );
        assert_eq!(cache.resident_bytes(), bytes);

        // Once the commit moves on, the old one is retired normally.
        let shown = [key(11)];
        cache.insert_value(key(11), 11, 3);
        assert!(cache.retire_stale(&key(11), &shown).is_some());
        assert!(cache.get(&key(10)).is_none());
    }

    /// The activation trade-off, evaluated the way `roi_plan` does but without
    /// a live `CimApp`: adaptive rendering must engage across the target range
    /// (~1000–4000 px sequences zoomed into part of the image) and must decline
    /// whenever it wouldn't pay for itself. The predicate is the whole reason
    /// the mode can't be a pessimisation, so it is pinned here rather than left
    /// to the sizes that happen to be on hand.
    #[test]
    fn activation_engages_only_when_it_saves_work() {
        // `roi_plan` minus the `CimApp`: the same geometry feeding the very same
        // `gain`, so this pins the shipped arithmetic rather than a copy of it.
        fn weigh(img: [usize; 2], cell: Vec2, zoom: f32, playing: bool) -> (f64, [usize; 2]) {
            let stage_step = if zoom >= 1.0 {
                1
            } else {
                (1.0 / zoom).floor().max(1.0) as usize
            };
            let step = roi_step(stage_step);
            let cover = if playing { COVER_PLAYING } else { COVER_PAUSED };
            let span = visible_span(cell, zoom, 0.0);
            let dims = region_dims(span, cover, step, img);
            let base = base_step(stage_step, img, 16384, false);
            (gain(img, stage_step, dims, base), dims)
        }
        let cell = Vec2::new(640.0, 500.0);
        let engages = |g: f64| g >= MIN_GAIN as f64;

        // The target case: zoomed into a sequence, playing. Every size in the
        // stated range must engage, including the ≤2048 half that the original
        // sharpness-based predicate could never reach.
        for img in [
            [1000usize, 1000],
            [1500, 1500],
            [2048, 2048],
            [3000, 2000],
            [4000, 3000],
        ] {
            let (gain, dims) = weigh(img, cell, 2.0, true);
            assert!(
                engages(gain),
                "{img:?} @ zoom 2 playing: gain {gain:.2} with region {dims:?}"
            );
        }
        // …and the win must be large where it matters most, not marginal.
        assert!(weigh([4000, 3000], cell, 2.0, true).0 > 15.0);
        assert!(weigh([2560, 1706], cell, 4.0, true).0 > 8.0);

        // Zoomed out to roughly fit, the region *is* the image: no work saved,
        // so the mode must stand aside and leave the classic path alone.
        for img in [[1500usize, 1500], [2048, 2048], [4000, 3000]] {
            let fit = cell.x / img[0] as f32;
            assert!(
                !engages(weigh(img, cell, fit, false).0),
                "{img:?} at fit zoom should decline"
            );
        }

        // A small image barely zoomed saves too little to be worth two layers
        // and two uploads; zooming further tips it over.
        assert!(!engages(weigh([1000, 1000], cell, 1.0, false).0));
        assert!(engages(weigh([1000, 1000], cell, 4.0, false).0));
    }

    /// A rotated pane centres its region on the image point actually under the
    /// middle of the cell — **not** on the view centre.
    ///
    /// The reported bug: a pane rotates about the *image* centre's screen
    /// position, so once panned away from that centre the view centre is no
    /// longer what the middle of the pane shows. Centring the region on it left
    /// the real viewport partly off the region, and since the displacement turns
    /// with the angle, the blurry `BASE_MAX` patches moved as the image was
    /// rotated. `view_center` undoes the rotation, which is exactly what
    /// `paint_rotated_about` re-applies when it draws the region.
    #[test]
    fn rotated_pane_centres_on_what_it_shows() {
        let disp = [2000usize, 1600];
        let cell = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(640.0, 500.0));
        // Panned well away from the image centre (1000, 800), where the error is
        // largest.
        let v = crate::view::ViewTransform {
            center: Vec2::new(1600.0, 1300.0),
            zoom: 4.0,
            needs_fit: false,
        };

        // Unrotated, the view centre *is* what the middle of the pane shows.
        assert_eq!(view_center(&v, disp, cell, 0.0), v.center);

        for deg in [15.0f32, 90.0, 180.0, -45.0] {
            let theta = deg.to_radians();
            let c = view_center(&v, disp, cell, theta);

            // It is the view centre turned by -theta about the image centre …
            let img_c = Vec2::new(1000.0, 800.0);
            let (sin, cos) = (-theta).sin_cos();
            let d = v.center - img_c;
            let want = img_c + Vec2::new(d.x * cos - d.y * sin, d.x * sin + d.y * cos);
            assert!((c - want).length() < 1e-2, "{deg}°: {c:?} vs {want:?}");

            // … and it is genuinely different from the view centre, by enough to
            // push a region right off the viewport.
            assert!(
                (c - v.center).length() > 100.0,
                "{deg}°: the bug would be invisible here"
            );

            // The painter's own mapping agrees: the region's centre lands back
            // under the middle of the cell.
            let back = canvas::rotate_img_to_screen(&v, disp, cell, c, theta);
            assert!(
                (back - cell.center()).length() < 1e-2,
                "{deg}°: round trip {back:?} vs {:?}",
                cell.center()
            );
        }
    }

    /// The region cache accounts bytes, evicts oldest-first, and refreshes
    /// recency on touch — the `SeqCache` contract, over region keys.
    #[test]
    fn region_cache_lru_and_budget() {
        let key = |n: u64| RegionKey {
            pane: 1,
            uid: n,
            sig: 0,
            step: 1,
            origin: [0, 0],
            dims: [256, 256],
        };
        let bytes = 256 * 256 * 4;
        let mut cache = RegionCache::<u32>::default();
        cache.insert_value(key(1), 1, 10);
        cache.insert_value(key(2), 2, 11);
        cache.insert_value(key(3), 3, 12);
        assert_eq!(cache.resident_bytes(), 3 * bytes);

        // Touch the oldest so it outlives the others.
        cache.touch(&key(1), 13);
        cache.enforce(2 * bytes, &[]);
        assert!(
            cache.get(&key(2)).is_none(),
            "oldest untouched entry evicted"
        );
        assert_eq!(cache.get(&key(1)), Some(&1));
        assert_eq!(cache.get(&key(3)), Some(&3));
        assert_eq!(cache.resident_bytes(), 2 * bytes);

        // Replacing a key re-accounts rather than double-counting.
        cache.insert_value(key(1), 10, 14);
        assert_eq!(cache.resident_bytes(), 2 * bytes);
        assert_eq!(cache.get(&key(1)), Some(&10));

        cache.clear();
        assert_eq!(cache.resident_bytes(), 0);
        assert!(cache.get(&key(1)).is_none());
    }
}
