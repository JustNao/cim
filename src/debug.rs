//! Optional pipeline profiling, gated on the `CIM_DEBUG=1` environment variable.
//!
//! When enabled, each stage a frame passes through on its way to the screen —
//! background **read + decode**, the **LUT / tone render**, the proprietary
//! **operators** (LUT_ALPHA / details), the **texture upload**, and the whole
//! **update** (CPU frame) — records its duration into a small ring buffer. The
//! debug window (`app::panels::draw_debug`, reachable from the toolbar's
//! **Debug** button) reports last / average / min / max per stage so bottlenecks
//! are easy to spot. When the variable is unset the recording sites are no-ops
//! and the button is hidden, so there is zero cost in a normal run.

use std::collections::VecDeque;
use std::time::Duration;

/// Whether debug measuring / UI is on. Read once from `CIM_DEBUG` at first use
/// and cached, so every later check is a cheap atomic-ish load.
pub fn enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("CIM_DEBUG").as_deref() == Ok("1"))
}

/// Recent timing samples for one pipeline stage (a bounded ring buffer, in
/// milliseconds), plus a lifetime count.
#[derive(Default)]
pub struct Stage {
    samples: VecDeque<f64>,
    count: u64,
}

impl Stage {
    /// How many recent samples the rolling last/avg/min/max are computed over.
    const CAP: usize = 120;

    /// Record one occurrence of this stage.
    pub fn record(&mut self, d: Duration) {
        if self.samples.len() == Self::CAP {
            self.samples.pop_front();
        }
        self.samples.push_back(d.as_secs_f64() * 1e3);
        self.count = self.count.wrapping_add(1);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn last(&self) -> Option<f64> {
        self.samples.back().copied()
    }

    pub fn avg(&self) -> Option<f64> {
        if self.samples.is_empty() {
            None
        } else {
            Some(self.samples.iter().sum::<f64>() / self.samples.len() as f64)
        }
    }

    pub fn min(&self) -> Option<f64> {
        self.samples.iter().copied().reduce(f64::min)
    }

    pub fn max(&self) -> Option<f64> {
        self.samples.iter().copied().reduce(f64::max)
    }
}

/// Timings for every measured stage of the read → display pipeline.
#[derive(Default)]
pub struct Metrics {
    /// Background read + decode of one frame (off the UI thread).
    pub decode: Stage,
    /// LUT / tone map to display RGBA (the synchronous path and the async gray
    /// render both feed this).
    pub lut: Stage,
    /// The proprietary operators (LUT_ALPHA / DETAILS_ENHANCED) `apply` call.
    pub operators: Stage,
    /// Building the `ColorImage` and uploading it as a GPU texture.
    pub upload: Stage,
    /// The whole `update` call: input, decode/render bookkeeping, and building
    /// the egui UI (CPU-side frame cost; excludes the GPU paint eframe does after).
    pub frame: Stage,
}
