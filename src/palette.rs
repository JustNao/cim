//! Colour palettes for the **Colormap** tone mode (`settings::ContrastMode`).
//!
//! A palette maps a toned 8-bit value (0..=255) to an RGB triple: two perceptual
//! ramps (viridis, turbo) for general mono imagery, and a blue–white–red
//! **diverging** ramp for signed data (a Diff Compute pane), where mid-grey reads
//! as zero and the sign shows as hue. Each 256-entry table is built once (lazily)
//! from a handful of anchor stops by linear interpolation, then cached — cheap,
//! deterministic, and used as a plain look-up in the render path.

use std::sync::OnceLock;

/// The selectable colour palettes for the Colormap tone. Part of `ToneOptions`,
/// so it rides the Transformations sync and the `--tone colormap:<name>` CLI.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Palette {
    /// Perceptually uniform blue→green→yellow (matplotlib viridis). Default.
    #[default]
    Viridis,
    /// High-contrast rainbow (Google turbo) — more detail, less uniform.
    Turbo,
    /// Diverging blue→white→red for signed data (zero at the white midpoint).
    Diverging,
}

impl Palette {
    /// The palettes in dropdown / CLI order.
    pub const ORDER: [Palette; 3] = [Palette::Viridis, Palette::Turbo, Palette::Diverging];

    /// Human label for the picker.
    pub fn label(self) -> &'static str {
        match self {
            Palette::Viridis => "Viridis",
            Palette::Turbo => "Turbo",
            Palette::Diverging => "Diverging",
        }
    }

    /// Stable small id used to key the render cache (`ToneLut`).
    pub fn id(self) -> u8 {
        match self {
            Palette::Viridis => 0,
            Palette::Turbo => 1,
            Palette::Diverging => 2,
        }
    }

    /// Lower-case token used in the `--tone colormap:<name>` view command.
    pub fn token(self) -> &'static str {
        match self {
            Palette::Viridis => "viridis",
            Palette::Turbo => "turbo",
            Palette::Diverging => "diverging",
        }
    }

    /// Parse a `--tone colormap:<name>` palette token (case-insensitive).
    pub fn from_token(s: &str) -> Option<Palette> {
        match s.trim().to_ascii_lowercase().as_str() {
            "viridis" => Some(Palette::Viridis),
            "turbo" => Some(Palette::Turbo),
            "diverging" | "diff" | "bwr" => Some(Palette::Diverging),
            _ => None,
        }
    }

    /// The 256-entry RGB look-up table, built once and cached.
    pub fn table(self) -> &'static [[u8; 3]; 256] {
        match self {
            Palette::Viridis => {
                static T: OnceLock<[[u8; 3]; 256]> = OnceLock::new();
                T.get_or_init(|| build(VIRIDIS_STOPS))
            }
            Palette::Turbo => {
                static T: OnceLock<[[u8; 3]; 256]> = OnceLock::new();
                T.get_or_init(|| build(TURBO_STOPS))
            }
            Palette::Diverging => {
                static T: OnceLock<[[u8; 3]; 256]> = OnceLock::new();
                T.get_or_init(|| build(DIVERGING_STOPS))
            }
        }
    }
}

/// `(position 0..=1, RGB)` control points, ascending by position.
type Stops = &'static [(f32, [u8; 3])];

// Approximate anchor stops (interpolated to 256 entries). Enough control points
// to reproduce each ramp's character without a hand-typed 256-row table.
const VIRIDIS_STOPS: Stops = &[
    (0.0, [68, 1, 84]),
    (0.25, [59, 82, 139]),
    (0.5, [33, 145, 140]),
    (0.75, [94, 201, 98]),
    (1.0, [253, 231, 37]),
];

const TURBO_STOPS: Stops = &[
    (0.0, [48, 18, 59]),
    (0.25, [31, 150, 225]),
    (0.5, [120, 209, 84]),
    (0.75, [249, 158, 45]),
    (1.0, [122, 4, 3]),
];

const DIVERGING_STOPS: Stops = &[
    (0.0, [59, 76, 192]),
    (0.5, [242, 242, 242]),
    (1.0, [180, 4, 38]),
];

/// Build a 256-entry RGB table by piecewise-linear interpolation across `stops`.
fn build(stops: Stops) -> [[u8; 3]; 256] {
    let mut table = [[0u8; 3]; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let t = i as f32 / 255.0;
        // Find the segment [a, b] containing t (stops are sorted, endpoints clamp).
        let mut a = &stops[0];
        let mut b = &stops[stops.len() - 1];
        for w in stops.windows(2) {
            if t >= w[0].0 && t <= w[1].0 {
                a = &w[0];
                b = &w[1];
                break;
            }
        }
        let span = (b.0 - a.0).max(f32::MIN_POSITIVE);
        let f = ((t - a.0) / span).clamp(0.0, 1.0);
        for (c, e) in entry.iter_mut().enumerate() {
            let v = a.1[c] as f32 + (b.1[c] as f32 - a.1[c] as f32) * f;
            *e = v.round().clamp(0.0, 255.0) as u8;
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A built table hits its anchor colours exactly at the endpoints and stays
    /// in range throughout.
    #[test]
    fn palette_endpoints_and_range() {
        for p in Palette::ORDER {
            let t = p.table();
            assert_eq!(t.len(), 256);
            // Endpoints match the first/last stop (0.0 and 1.0 are always anchors).
            let stops = match p {
                Palette::Viridis => VIRIDIS_STOPS,
                Palette::Turbo => TURBO_STOPS,
                Palette::Diverging => DIVERGING_STOPS,
            };
            assert_eq!(t[0], stops[0].1);
            assert_eq!(t[255], stops[stops.len() - 1].1);
        }
    }

    /// The diverging palette is near-white at its centre (the zero point).
    #[test]
    fn diverging_center_is_light() {
        let mid = Palette::Diverging.table()[128];
        assert!(mid.iter().all(|&c| c > 220), "center should be light: {mid:?}");
    }

    /// Tokens round-trip through `from_token`.
    #[test]
    fn palette_token_roundtrip() {
        for p in Palette::ORDER {
            assert_eq!(Palette::from_token(p.token()), Some(p));
        }
    }
}
