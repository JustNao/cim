//! The instance's CPU worker-thread budget, and the rayon pool sized from it.
//!
//! cim runs several thread pools at once (§15): the background decode pool, the
//! rayon pool behind the parallel render / composite / analytic scans, and one
//! render worker per pane. On a **shared multi-user server or VNC host** the
//! rayon pool is the dangerous one — left to itself it takes one thread per
//! core, so a 64-core box gives a single instance 64 threads no matter how
//! carefully the rest is capped.
//!
//! So the two *shared* pools draw from one budget: [`Config::cpu_budget`] is the
//! **total** worker threads this instance may run, split by [`split`]. The
//! number in Settings is then the number of threads you actually get, which is
//! the only form a shared-host budget is useful in. Per-pane render workers are
//! deliberately outside it — there is one per open pane, it is idle unless that
//! pane is re-rendering, and tying it to the budget would make opening a pane
//! silently slow down decoding.
//!
//! [`Config::cpu_budget`]: crate::settings::Config::cpu_budget

use std::sync::{Arc, OnceLock, RwLock};

/// Budget bounds, as offered by the Settings slider. The floor keeps the split
/// meaningful (2 decode + 2 rayon); the ceiling is a sanity limit well past any
/// core count we expect to run on.
pub const MIN: usize = 4;
pub const MAX: usize = 64;

/// Default total worker threads: enough to keep a workstation busy, small
/// enough that several instances can share a big server without fighting.
pub const DEFAULT: usize = 16;

/// Split a total budget into `(decode, rayon)` worker counts.
///
/// Decode takes a quarter, floored at 2 (a single decode thread serialises
/// playback prefetch behind the frame being shown) and capped at 8 (decoding is
/// as much I/O as CPU — past a handful of threads a sequence is limited by the
/// file/mount, and on a shared mount extra readers crowd out other users).
/// Rayon takes the rest, so the two never exceed the budget.
pub fn split(budget: usize) -> (usize, usize) {
    let budget = budget.clamp(MIN, MAX);
    let decode = (budget / 4).clamp(2, 8);
    (decode, budget - decode)
}

/// The live rayon pool, rebuilt when the budget changes. `RwLock` because reads
/// (every parallel job) vastly outnumber writes (a Settings edit), and `Arc` so
/// a job holds its pool alive across a rebuild: the old pool is dropped only
/// once the last job using it finishes.
static POOL: OnceLock<RwLock<Arc<rayon::ThreadPool>>> = OnceLock::new();

fn build(threads: usize) -> Arc<rayon::ThreadPool> {
    Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("cim-cpu-{i}"))
            .build()
            .expect("build cim cpu pool"),
    )
}

fn cell() -> &'static RwLock<Arc<rayon::ThreadPool>> {
    POOL.get_or_init(|| RwLock::new(build(split(DEFAULT).1)))
}

/// Resize the pool to the rayon share of `budget`. Called at startup and
/// whenever the Settings budget changes, so a cap applies without a restart.
///
/// The replaced pool keeps running until its in-flight jobs finish — dropping
/// the last `Arc` joins its threads — so this is safe to call mid-render.
pub fn set_budget(budget: usize) {
    let want = split(budget).1;
    let cell = cell();
    if cell.read().unwrap().current_num_threads() == want {
        return; // unchanged: keep the pool (and its warm threads)
    }
    *cell.write().unwrap() = build(want);
}

/// Run `f` on the budgeted pool.
///
/// **Install at thread boundaries, not at individual `par_iter` call sites.**
/// Rayon resolves nested parallelism against the pool the caller is running in,
/// so wrapping a worker's whole body captures every parallel call underneath it
/// — including ones added later, which is what keeps the cap from quietly
/// leaking. Without this, a bare `par_iter` runs on rayon's *global* pool, which
/// sizes itself to the machine and ignores the budget entirely.
///
/// Sizing helpers that read [`rayon::current_num_threads`] (e.g.
/// `media::scan_band`) see the budgeted count inside here, so they adapt to the
/// cap on their own.
pub fn install<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    let pool = Arc::clone(&*cell().read().unwrap());
    pool.install(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pool is process-global, so the tests that resize it must not run
    /// concurrently with each other — `cargo test` threads them by default, and
    /// one test's `set_budget` would otherwise fail another's assertion.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Hold the serial lock for a resizing test, ignoring poisoning so one
    /// failure doesn't cascade into spurious failures in the others.
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The split is a **total**: whatever the budget, the two shared pools
    /// together never exceed it, and neither is starved to zero. This is the
    /// whole promise of the setting — the number in Settings is the number of
    /// threads the instance runs.
    #[test]
    fn the_split_never_exceeds_the_budget() {
        for budget in MIN..=MAX {
            let (decode, rayon) = split(budget);
            assert_eq!(decode + rayon, budget, "budget {budget}");
            assert!(decode >= 2, "decode starved at {budget}");
            assert!(rayon >= 1, "rayon starved at {budget}");
            assert!(decode <= 8, "decode over cap at {budget}");
        }
    }

    /// Out-of-range budgets (a hand-edited config) clamp instead of panicking or
    /// underflowing the subtraction.
    #[test]
    fn an_out_of_range_budget_clamps() {
        assert_eq!(split(0), split(MIN));
        assert_eq!(split(1_000_000), split(MAX));
        // The documented defaults, pinned so a change to the formula is noticed.
        assert_eq!(split(DEFAULT), (4, 12));
        assert_eq!(split(MIN), (2, 2));
        assert_eq!(split(MAX), (8, 56));
    }

    /// `install` runs on the budgeted pool, not rayon's machine-sized global
    /// one — the property the whole cap depends on.
    #[test]
    fn install_runs_on_the_budgeted_pool() {
        let _serial = exclusive();
        set_budget(8); // -> 2 decode, 6 rayon
        assert_eq!(install(rayon::current_num_threads), 6);
        set_budget(MAX);
        assert_eq!(install(rayon::current_num_threads), split(MAX).1);
    }

    /// A **nested** parallel iterator — the shape every real call site has, none
    /// of which mention the pool — spreads over no more than the budgeted
    /// threads. `current_num_threads` alone wouldn't catch a job that escaped to
    /// the global pool, so this counts the distinct workers that actually ran.
    #[test]
    fn nested_parallel_work_stays_within_the_budget() {
        let _serial = exclusive();
        use rayon::prelude::*;
        use std::collections::HashSet;
        use std::sync::Mutex;

        set_budget(8); // -> 6 rayon threads
        let seen = Mutex::new(HashSet::new());
        install(|| {
            // Enough items, each slow enough, that rayon has every reason to
            // spread them over as many threads as its pool allows.
            (0..2000u64).into_par_iter().for_each(|i| {
                seen.lock().unwrap().insert(std::thread::current().id());
                std::hint::black_box((0..400).fold(i, |a, b| a.wrapping_add(b)));
            });
        });
        let used = seen.into_inner().unwrap().len();
        // `install` also lends the calling thread to the pool, so the worker set
        // can include it — hence `+ 1`.
        assert!(used <= 7, "{used} threads used, budget allows 6 (+ caller)");
    }

    /// Resizing while a job is running must not deadlock: `install` releases the
    /// read lock before running `f`, so the `set_budget` write lock isn't blocked
    /// by a long render. Times out rather than hanging the suite on regression.
    #[test]
    fn resizing_during_a_job_does_not_deadlock() {
        let _serial = exclusive();
        use std::sync::mpsc;
        use std::time::Duration;

        set_budget(DEFAULT);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // A budget change landing while a job holds a pool `Arc` — exactly
            // what a Settings edit mid-export does.
            install(|| {
                set_budget(MAX);
                set_budget(MIN);
            });
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "resize during an installed job deadlocked"
        );
        set_budget(DEFAULT);
    }
}
