//! Small stateless helpers shared across the app modules: pane-vec index
//! remapping for reorder, drag drop-target hit-testing, wheel/zoom input
//! reading, and string ellipsizing.

use super::*;

/// Update an index after `panes.swap(src, dst)`.
pub(super) fn remap(v: &mut usize, src: usize, dst: usize) {
    if *v == src {
        *v = dst;
    } else if *v == dst {
        *v = src;
    }
}

/// Update an index after moving the pane at `from` to index `to`
/// (`remove(from)` then `insert(to)`) — the reorder used by the Media manager.
pub(super) fn remap_move(v: &mut usize, from: usize, to: usize) {
    if *v == from {
        *v = to;
    } else if from < *v && *v <= to {
        *v -= 1;
    } else if to <= *v && *v < from {
        *v += 1;
    }
}

/// The Media-manager row (its pane vec index) that a drop at screen-`y` targets:
/// the row directly under the cursor, else the nearest one (so drops in the gaps
/// or past either end still resolve).
pub(super) fn drop_target(rows: &[(usize, egui::Rangef)], y: f32) -> Option<usize> {
    if let Some(&(idx, _)) = rows.iter().find(|(_, band)| band.contains(y)) {
        return Some(idx);
    }
    rows.iter()
        .min_by(|a, b| {
            (a.1.center() - y)
                .abs()
                .total_cmp(&(b.1.center() - y).abs())
        })
        .map(|&(idx, _)| idx)
}

/// Zoom sensitivity per scroll unit; Shift doubles it.
pub(super) fn zoom_speed(ctx: &egui::Context) -> f32 {
    if ctx.input(|i| i.modifiers.shift) {
        0.003
    } else {
        0.0015
    }
}

/// Effective wheel delta for zooming. While Shift is held the platform remaps
/// the mouse wheel to the horizontal axis, leaving `raw_scroll_delta.y` at 0 —
/// so fall back to the `x` component when `y` is zero.
pub(super) fn wheel_delta(ctx: &egui::Context) -> f32 {
    ctx.input(|i| {
        let s = i.raw_scroll_delta;
        if s.y != 0.0 {
            s.y
        } else {
            s.x
        }
    })
}

/// Best-effort absolute form of a path, for the filename hover and the reopen
/// command. Resolves a relative path against the current working directory
/// **lexically** (`std::path::absolute` — no filesystem access, so no symlink
/// resolution and, on Windows, no `\\?\` verbatim prefix); an already-absolute
/// path is normalised. Falls back to the path unchanged if it can't be resolved
/// (e.g. the CWD is unavailable).
pub(super) fn absolute_path(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// `name` prefixed by the last `n` folders `path` sits in, joined with the
/// platform separator (`a/b/name` for `n = 2`) — the pane header's title. Stops
/// early at the root, so a large `n` on a shallow path just shows every folder.
pub(super) fn with_parent_dirs(path: &Path, name: &str, n: usize) -> String {
    let mut dirs: Vec<String> = path
        .parent()
        .into_iter()
        .flat_map(|p| p.components().rev())
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .take(n)
        .collect();
    dirs.reverse();
    dirs.push(name.to_owned());
    dirs.join(std::path::MAIN_SEPARATOR_STR)
}

pub(super) fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_parent_dirs_prefixes_the_last_folders() {
        let sep = std::path::MAIN_SEPARATOR_STR;
        let p = Path::new("root").join("run1").join("cam").join("a.tif");
        assert_eq!(with_parent_dirs(&p, "a.tif", 0), "a.tif");
        assert_eq!(with_parent_dirs(&p, "a.tif", 1), format!("cam{sep}a.tif"));
        assert_eq!(
            with_parent_dirs(&p, "a.tif", 2),
            format!("run1{sep}cam{sep}a.tif")
        );
        // More folders than the path has: every one of them, no root/prefix.
        assert_eq!(
            with_parent_dirs(&p, "a.tif", 9),
            format!("root{sep}run1{sep}cam{sep}a.tif")
        );
        assert_eq!(with_parent_dirs(Path::new("a.tif"), "a.tif", 3), "a.tif");
    }
}
