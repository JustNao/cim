//! The background workers and the UI-side record of what each has in flight.
//!
//! Every pool here reports back by pane `id` and wakes the UI when a job lands.
//! Grouped so the "a pane's contents changed" teardown lives in one place
//! ([`Workers::forget_pane`]): a stranded in-flight guard blocks that pane's
//! next job forever, and with renders gating the lock-step commit that stalls
//! the whole timeline.

use std::collections::{HashMap, HashSet};

use eframe::egui;

use super::roi;
use crate::decoder::BackgroundDecoder;

pub(super) struct Workers {
    pub decoder: BackgroundDecoder,
    /// `(pane id, frame)` decodes and probes queued on `decoder`.
    pub inflight: HashSet<(u64, usize)>,
    /// One render thread per pane (see [`crate::renderer`]).
    pub renderer: crate::renderer::RenderPool,
    /// Pane ids with a base render in flight: at most one per pane, so rapid
    /// tone / frame changes coalesce.
    pub render_inflight: HashSet<u64>,
    /// Finished adaptive-render regions, LRU'd across panes (see [`roi`]).
    pub regions: roi::RegionCache,
    /// The region render in flight per pane, with the key it will land under.
    /// Keyed by pane, not region: during playback every frame wants a new
    /// region, so a region key would never dedupe.
    pub roi_inflight: HashMap<u64, roi::RegionKey>,
    /// Page counts of fast-scannable sequences, measured off the UI thread.
    pub scanner: crate::offsets::OffsetScanner,
    /// Tags each scan: pane ids survive a reload, so a scan of the old contents
    /// must be told apart from one of the new.
    pub offset_gen: u64,
    /// Source-file signatures for auto-reload.
    pub watcher: crate::watcher::FileWatcher,
    pub watch_gen: u64,
    /// Rate limit on signing requests, independent of the repaint rate.
    pub watch_polled_at: f64,
    /// Timeline hover-preview thumbnails. A pool of its own because the
    /// per-pane renderer's results gate the lock-step commit.
    pub thumbs: crate::thumbs::ThumbPool,
    pub thumb_cache: crate::thumbs::ThumbCache,
}

impl Workers {
    pub fn new(decode_threads: usize, ctx: &egui::Context) -> Self {
        Self {
            decoder: BackgroundDecoder::new(decode_threads, ctx.clone()),
            inflight: HashSet::new(),
            renderer: crate::renderer::RenderPool::new(ctx.clone()),
            render_inflight: HashSet::new(),
            regions: roi::RegionCache::default(),
            roi_inflight: HashMap::new(),
            scanner: crate::offsets::OffsetScanner::new(ctx.clone()),
            offset_gen: 0,
            watcher: crate::watcher::FileWatcher::new(ctx.clone()),
            watch_gen: 0,
            watch_polled_at: f64::NEG_INFINITY,
            thumbs: crate::thumbs::ThumbPool::new(ctx.clone()),
            thumb_cache: crate::thumbs::ThumbCache::default(),
        }
    }

    /// Replace the decode pool (a CPU budget change). Jobs queued on the old
    /// pool never land on the new one, so they are forgotten to be re-requested.
    pub fn rebuild_decoder(&mut self, threads: usize, ctx: &egui::Context) {
        self.decoder = BackgroundDecoder::new(threads, ctx.clone());
        self.inflight.clear();
    }

    /// Drop everything held for pane `id`, because it was closed or its frames
    /// changed (reload, new Scale target): its persistent readers, its render
    /// thread and operator instances, its in-flight guards, and the regions and
    /// thumbnails rendered from the old frames.
    pub fn forget_pane(&mut self, id: u64) {
        self.decoder.forget(id);
        self.inflight.retain(|(pid, _)| *pid != id);
        self.renderer.forget(id);
        self.render_inflight.remove(&id);
        self.roi_inflight.remove(&id);
        self.regions.forget_pane(id);
        self.thumb_cache.forget_pane(id);
    }

    /// Whether a decode or a render is still due to land.
    pub fn busy(&self) -> bool {
        !self.inflight.is_empty() || !self.render_inflight.is_empty()
    }
}
