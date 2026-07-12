//! The `Media` model: the source kinds (still, multi-page TIFF, numbered file
//! run, concatenated TIFF run) behind one interface — length discovery,
//! residency / LRU bookkeeping, and how each frame is decoded (`DecodeReq`).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use super::FrameData;

/// One media source shown in a pane.
pub enum Media {
    Still(Still),
    TiffSeq(TiffSeq),
    FileSeq(FileSeq),
    ConcatSeq(ConcatSeq),
}

/// A concatenation's frame files and its discovered `frame → (file, page)` map.
pub type ConcatLayout = (Vec<PathBuf>, Vec<(usize, usize)>);

/// How the background pool should decode one frame of a sequence.
pub enum DecodeReq {
    /// Seek `page` of the multi-page TIFF at `path`, via a reader keyed by
    /// (pane id, `file`). A lone `TiffSeq` uses `file = 0` and `page = frame`;
    /// a `ConcatSeq` uses `file` to pick which TIFF in the run and `page` the
    /// page within it.
    ///
    /// `probe` = a **metadata-only** frontier probe: seek to the page and read
    /// just whether it exists (its IFD), without decoding pixels. Used to
    /// fast-forward lazy length discovery during a seek so the intervening pages
    /// aren't decompressed — only the landed target frame is.
    Tiff {
        file: usize,
        page: usize,
        path: PathBuf,
        probe: bool,
    },
    /// Decode this standalone file — one frame of a numbered still sequence.
    File(PathBuf),
}

pub struct Still {
    pub(super) name: String,
    pub(super) frame: Arc<FrameData>,
    pub(super) hi_depth: bool,
}

/// Frame residency plus LRU / memory-budget bookkeeping, shared by both
/// sequence kinds (multi-page TIFF and numbered image files). Slots may be
/// evicted back to `None` to stay within the cache budget without changing the
/// known length.
pub(super) struct SeqCache {
    /// One slot per known frame; `None` = not resident (never decoded or evicted).
    cache: Vec<Option<Arc<FrameData>>>,
    /// Recency tick per frame, parallel to `cache`; the budget evicts the
    /// least-recently-used resident frames first.
    last_used: Vec<u64>,
    /// Running total of resident sample bytes (sum of `byte_len` over `Some`s).
    resident_bytes: usize,
    /// `(last_used, frame)` for every **resident** frame, ordered by recency, so
    /// the budget can peek the least-recently-used one in O(log n) instead of
    /// scanning + sorting the whole cache each over-budget tick. Kept in sync by
    /// `insert` / `touch` / `evict`; non-resident slots are never in the set.
    lru: BTreeSet<(u64, usize)>,
}

impl SeqCache {
    /// A cache of `len` not-yet-resident frames.
    pub(super) fn new(len: usize) -> Self {
        Self {
            cache: vec![None; len],
            last_used: vec![0; len],
            resident_bytes: 0,
            lru: BTreeSet::new(),
        }
    }

    fn len(&self) -> usize {
        self.cache.len()
    }

    fn resident(&self, idx: usize) -> Option<Arc<FrameData>> {
        self.cache.get(idx).and_then(|slot| slot.clone())
    }

    /// Store a decoded frame. Replacing an existing slot re-accounts its bytes;
    /// inserting exactly at the end (`idx == len`) grows the known length by one
    /// (how a TIFF frontier probe discovers the next page). Out-of-range inserts
    /// are ignored.
    pub(super) fn insert(&mut self, idx: usize, frame: Arc<FrameData>) {
        if idx < self.cache.len() {
            if let Some(old) = &self.cache[idx] {
                self.resident_bytes -= old.byte_len(); // already in `lru` at its tick
            } else {
                self.lru.insert((self.last_used[idx], idx)); // newly resident
            }
            self.resident_bytes += frame.byte_len();
            self.cache[idx] = Some(frame);
        } else if idx == self.cache.len() {
            self.resident_bytes += frame.byte_len();
            self.cache.push(Some(frame));
            self.last_used.push(0);
            self.lru.insert((0, idx));
        }
    }

    /// Grow the known length by one **empty** (non-resident) slot — how a
    /// metadata-only frontier probe records that a page exists without decoding
    /// it. Only advances at the frontier (`idx == len`); anything else is
    /// ignored (a real decode fills interior slots via `insert`).
    fn note_len(&mut self, idx: usize) {
        if idx == self.cache.len() {
            self.cache.push(None);
            self.last_used.push(0);
        }
    }

    fn touch(&mut self, idx: usize, clock: u64) {
        let Some(&old) = self.last_used.get(idx) else {
            return;
        };
        if old == clock {
            return;
        }
        // A resident frame moves within the recency order; keep `lru` in sync.
        if self.cache[idx].is_some() {
            self.lru.remove(&(old, idx));
            self.lru.insert((clock, idx));
        }
        self.last_used[idx] = clock;
    }

    fn evict(&mut self, idx: usize) {
        if let Some(Some(old)) = self.cache.get_mut(idx).map(|s| s.take()) {
            self.resident_bytes -= old.byte_len();
            self.lru.remove(&(self.last_used[idx], idx));
        }
    }

    fn resident_frames(&self) -> Vec<(usize, u64, usize)> {
        self.cache
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|f| (i, self.last_used[i], f.byte_len())))
            .collect()
    }

    /// The least-recently-used **resident** frame that isn't `protect` (the shown
    /// frame, never evicted), as `(last_used tick, frame, byte size)` — peeked
    /// from the recency order in O(log n). `None` when nothing else is resident.
    fn lru_evictable(&self, protect: usize) -> Option<(u64, usize, usize)> {
        self.lru.iter().find_map(|&(tick, frame)| {
            (frame != protect).then(|| {
                let bytes = self.cache[frame].as_ref().map_or(0, |f| f.byte_len());
                (tick, frame, bytes)
            })
        })
    }

    fn resident_count(&self) -> usize {
        self.cache.iter().filter(|f| f.is_some()).count()
    }
}

pub struct TiffSeq {
    pub(super) name: String,
    pub(super) path: PathBuf,
    pub(super) size: [usize; 2],
    pub(super) hi_depth: bool,
    /// Page 0 is 1-bit bilevel → this is a boolean-mask sequence.
    pub(super) is_mask: bool,
    /// Frames known so far. Grows lazily as later pages are decoded — we never
    /// walk the whole file to learn its length.
    pub(super) frames: SeqCache,
    /// Set once a probe past `frames.len()` found no more pages: the real end.
    pub(super) at_end: bool,
}

/// A sequence whose frames are individual numbered image files (one file per
/// frame) — e.g. `frame_000.png … frame_011.png`, given on the command line as
/// a compact `PREFIX%0Xu SUFFIX,START,END` token. Unlike a TIFF its length is
/// known up front (the file list), so there is no lazy discovery and it is
/// always "at end".
pub struct FileSeq {
    pub(super) name: String,
    pub(super) paths: Vec<PathBuf>,
    pub(super) size: [usize; 2],
    pub(super) hi_depth: bool,
    pub(super) frames: SeqCache,
}

/// Several multi-page TIFFs presented as **one** continuous timeline: when
/// `movie_000.tif` runs out of pages the timeline rolls straight into
/// `movie_001.tif`, and so on. Opened from a compact `PREFIX%0Xu.tif,…` token.
///
/// Page counts per file aren't known up front (a TIFF's length is discovered
/// lazily, §4), so the global length grows one page at a time: the frontier
/// probe walks pages within the current file, and when a probe finds no page it
/// rolls over to the next file rather than ending the sequence — only the last
/// file's end is the real end.
pub struct ConcatSeq {
    pub(super) name: String,
    pub(super) files: Vec<PathBuf>,
    pub(super) size: [usize; 2],
    pub(super) hi_depth: bool,
    pub(super) frames: SeqCache,
    /// Global frame → (file index, page within that file). `map.len()` always
    /// equals `frames.len()` (the known length).
    pub(super) map: Vec<(usize, usize)>,
    /// The next (file, page) the frontier probe will try — not yet in `map`.
    pub(super) disc_file: usize,
    pub(super) disc_page: usize,
    /// Set once the *last* file has been exhausted: the real end.
    pub(super) at_end: bool,
}

impl Media {
    /// Wrap an in-memory frame as an always-resident still (e.g. a computed
    /// image). Not backed by a file.
    pub fn still(name: String, frame: FrameData) -> Media {
        let hi_depth = frame.hi_depth();
        Media::Still(Still {
            name,
            frame: Arc::new(frame),
            hi_depth,
        })
    }

    pub fn name(&self) -> &str {
        match self {
            Media::Still(s) => &s.name,
            Media::TiffSeq(t) => &t.name,
            Media::FileSeq(f) => &f.name,
            Media::ConcatSeq(c) => &c.name,
        }
    }

    pub fn frame_count(&self) -> usize {
        match self {
            Media::Still(_) => 1,
            Media::TiffSeq(t) => t.frames.len(),
            Media::FileSeq(f) => f.frames.len(),
            Media::ConcatSeq(c) => c.frames.len(),
        }
    }

    /// Whether this is a multi-frame sequence (not a single still). A multi-page
    /// TIFF counts even before its length is discovered (`frame_count` starts at
    /// 1), since it decodes and plays like a sequence.
    pub fn is_sequence(&self) -> bool {
        !matches!(self, Media::Still(_))
    }

    pub fn size(&self) -> [usize; 2] {
        match self {
            Media::Still(s) => s.frame.size,
            Media::TiffSeq(t) => t.size,
            Media::FileSeq(f) => f.size,
            Media::ConcatSeq(c) => c.size,
        }
    }

    /// More than 8 bits per sample → clip-on-load is a sensible default.
    pub fn hi_depth(&self) -> bool {
        match self {
            Media::Still(s) => s.hi_depth,
            Media::TiffSeq(t) => t.hi_depth,
            Media::FileSeq(f) => f.hi_depth,
            Media::ConcatSeq(c) => c.hi_depth,
        }
    }

    /// A boolean-mask media (1-bit bilevel TIFF): rendered black/white and
    /// available to tint another pane as an overlay. Only TIFFs are masks.
    pub fn is_mask(&self) -> bool {
        matches!(self, Media::TiffSeq(t) if t.is_mask)
    }

    pub fn resident(&self, idx: usize) -> Option<Arc<FrameData>> {
        match self {
            Media::Still(s) => Some(s.frame.clone()),
            Media::TiffSeq(t) => t.frames.resident(idx),
            Media::FileSeq(f) => f.frames.resident(idx),
            Media::ConcatSeq(c) => c.frames.resident(idx),
        }
    }

    /// How the pool should decode frame `idx`, or `None` for an always-resident
    /// still. A TIFF page seeks in the persistent per-id reader; a numbered
    /// still sequence decodes that frame's own file; a concatenation maps the
    /// global frame to (file, page) — or, at the frontier, probes the next page.
    pub fn decode_job(&self, idx: usize) -> Option<DecodeReq> {
        self.job(idx, false)
    }

    /// A **metadata-only** frontier probe for frame `idx` (TIFF-backed media
    /// only): confirms the page exists without decoding its pixels, so a seek
    /// can pass it cheaply. `None` for stills / numbered still runs (a still
    /// run's length is known up front, so it never needs probing).
    pub fn probe_job(&self, idx: usize) -> Option<DecodeReq> {
        self.job(idx, true)
    }

    fn job(&self, idx: usize, probe: bool) -> Option<DecodeReq> {
        match self {
            Media::Still(_) => None,
            Media::TiffSeq(t) => Some(DecodeReq::Tiff {
                file: 0,
                page: idx,
                path: t.path.clone(),
                probe,
            }),
            // A numbered still run knows its length up front, so it is only ever
            // decoded, never probed.
            Media::FileSeq(f) if !probe => f.paths.get(idx).cloned().map(DecodeReq::File),
            Media::FileSeq(_) => None,
            Media::ConcatSeq(c) => c.job(idx, probe),
        }
    }

    pub fn insert(&mut self, idx: usize, frame: Arc<FrameData>) {
        match self {
            Media::Still(_) => {}
            Media::TiffSeq(t) => t.frames.insert(idx, frame),
            Media::FileSeq(f) => f.frames.insert(idx, frame),
            Media::ConcatSeq(c) => c.insert(idx, frame),
        }
    }

    /// Mark frame `idx` as used at `clock`, so the budget evicts it last.
    pub fn touch(&mut self, idx: usize, clock: u64) {
        match self {
            Media::Still(_) => {}
            Media::TiffSeq(t) => t.frames.touch(idx, clock),
            Media::FileSeq(f) => f.frames.touch(idx, clock),
            Media::ConcatSeq(c) => c.frames.touch(idx, clock),
        }
    }

    /// Drop a resident frame to reclaim memory. The known length is unchanged;
    /// the frame simply re-decodes on demand if shown again.
    pub fn evict(&mut self, idx: usize) {
        match self {
            Media::Still(_) => {}
            Media::TiffSeq(t) => t.frames.evict(idx),
            Media::FileSeq(f) => f.frames.evict(idx),
            Media::ConcatSeq(c) => c.frames.evict(idx),
        }
    }

    /// Sample bytes currently resident, for the memory budget.
    pub fn resident_bytes(&self) -> usize {
        match self {
            Media::Still(s) => s.frame.byte_len(),
            Media::TiffSeq(t) => t.frames.resident_bytes,
            Media::FileSeq(f) => f.frames.resident_bytes,
            Media::ConcatSeq(c) => c.frames.resident_bytes,
        }
    }

    /// Resident frames as `(frame index, last-used tick, byte size)`. Stills
    /// return empty — their single frame is always needed and never evicted.
    pub fn resident_frames(&self) -> Vec<(usize, u64, usize)> {
        match self {
            Media::Still(_) => Vec::new(),
            Media::TiffSeq(t) => t.frames.resident_frames(),
            Media::FileSeq(f) => f.frames.resident_frames(),
            Media::ConcatSeq(c) => c.frames.resident_frames(),
        }
    }

    /// The least-recently-used evictable frame as `(tick, frame, bytes)`, never
    /// `protect` (the shown frame). Peeked from the recency order in O(log n) so
    /// the budget doesn't scan every slot. `None` for a still (never evicts) or
    /// when nothing else is resident.
    pub fn lru_evictable(&self, protect: usize) -> Option<(u64, usize, usize)> {
        match self {
            Media::Still(_) => None,
            Media::TiffSeq(t) => t.frames.lru_evictable(protect),
            Media::FileSeq(f) => f.frames.lru_evictable(protect),
            Media::ConcatSeq(c) => c.frames.lru_evictable(protect),
        }
    }

    /// True once we've confirmed there is no page beyond what we already know.
    /// Always true for a still or a numbered still sequence (length is known);
    /// discovered lazily for a TIFF or a concatenation.
    pub fn at_end(&self) -> bool {
        match self {
            Media::Still(_) | Media::FileSeq(_) => true,
            Media::TiffSeq(t) => t.at_end,
            Media::ConcatSeq(c) => c.at_end,
        }
    }

    /// A frontier probe found no page where one was expected. A `TiffSeq` has
    /// reached its real end; a `ConcatSeq` has finished the current file and
    /// rolls over to the next (only the last file's end is the real end).
    pub fn frontier_ended(&mut self) {
        match self {
            Media::TiffSeq(t) => t.at_end = true,
            Media::ConcatSeq(c) => c.roll_to_next_file(),
            _ => {}
        }
    }

    /// A **metadata-only** frontier probe confirmed a page exists at `idx`
    /// without decoding it: grow the known length by one empty (non-resident)
    /// slot so a seek can pass it. The frame decodes on demand when actually
    /// shown. Only advances at the frontier (`idx == len`).
    pub fn note_frontier(&mut self, idx: usize) {
        match self {
            Media::TiffSeq(t) => t.frames.note_len(idx),
            Media::ConcatSeq(c) => c.note_frontier(idx),
            _ => {}
        }
    }

    pub fn resident_count(&self) -> usize {
        match self {
            Media::Still(_) => 1,
            Media::TiffSeq(t) => t.frames.resident_count(),
            Media::FileSeq(f) => f.frames.resident_count(),
            Media::ConcatSeq(c) => c.frames.resident_count(),
        }
    }

    /// For a concatenation, the file list plus the discovered global→(file,page)
    /// map, so an export can snapshot it. `None` for other media.
    pub fn concat_layout(&self) -> Option<ConcatLayout> {
        match self {
            Media::ConcatSeq(c) => Some((c.files.clone(), c.map.clone())),
            _ => None,
        }
    }
}

impl ConcatSeq {
    /// Map global frame `idx` to a decode (or, with `probe`, a metadata-only)
    /// request. Frames already in `map` resolve directly; the frontier
    /// (`idx == map.len()`) probes the next (file, page) to discover.
    fn job(&self, idx: usize, probe: bool) -> Option<DecodeReq> {
        let (file, page) = if idx < self.map.len() {
            self.map[idx]
        } else if idx == self.map.len() {
            (self.disc_file, self.disc_page)
        } else {
            return None;
        };
        self.files.get(file).map(|path| DecodeReq::Tiff {
            file,
            page,
            path: path.clone(),
            probe,
        })
    }

    fn insert(&mut self, idx: usize, frame: Arc<FrameData>) {
        if idx == self.frames.len() {
            self.advance_map();
        }
        self.frames.insert(idx, frame);
    }

    /// A metadata-only frontier probe confirmed the next global frame exists
    /// (still within the current file): record where it lives and grow the
    /// known length by one empty slot, without decoding its pixels.
    fn note_frontier(&mut self, idx: usize) {
        if idx == self.frames.len() {
            self.advance_map();
            self.frames.note_len(idx);
        }
    }

    /// Frontier confirmed: record where this global frame lives and step the
    /// probe to the next page of the same file.
    fn advance_map(&mut self) {
        self.map.push((self.disc_file, self.disc_page));
        self.disc_page += 1;
    }

    /// The current file has no more pages: continue the timeline at the start of
    /// the next file, or mark the real end if this was the last file.
    fn roll_to_next_file(&mut self) {
        self.disc_file += 1;
        self.disc_page = 0;
        if self.disc_file >= self.files.len() {
            self.at_end = true;
        }
    }
}
