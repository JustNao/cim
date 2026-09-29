//! Which decoder an input file goes through, decided once from its extension.

use std::path::Path;

/// Still and multi-page image extensions the app opens. Shared by the file
/// dialog, the completion filter and [`Format::of`], so they can't drift apart.
pub const LOADABLE_EXTS: &[&str] = &[
    "tif", "tiff", "png", "jpg", "jpeg", "bmp", "webp", "jp2", "j2k", "j2c", "jpc",
];

/// Video containers. Each opens as one pane of its own, never grouped into a
/// numbered run or a folder concatenation: a video already is a timeline.
pub const VIDEO_EXTS: &[&str] = &["mp4", "avi"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// Multi-page TIFF, through the `tiff` crate (page by page).
    Tiff,
    /// JPEG 2000, through `media::jp2` (possibly at a reduced level).
    Jp2,
    /// Any other still, through the `image` crate.
    Raster,
    /// mp4 / avi, through the ffmpeg CLI.
    Video,
}

impl Format {
    /// The format `path`'s extension names, or `None` when cim can't open it.
    pub fn of(path: &Path) -> Option<Format> {
        let ext = path.extension()?.to_string_lossy().to_lowercase();
        let ext = ext.as_str();
        if VIDEO_EXTS.contains(&ext) {
            Some(Format::Video)
        } else if !LOADABLE_EXTS.contains(&ext) {
            None
        } else if ext == "tif" || ext == "tiff" {
            Some(Format::Tiff)
        } else if super::jp2::EXTS.contains(&ext) {
            Some(Format::Jp2)
        } else {
            Some(Format::Raster)
        }
    }

    /// The decoder to try for `path`: an unknown extension goes to the `image`
    /// crate, which reports the error.
    pub fn decoder_for(path: &Path) -> Format {
        Format::of(path).unwrap_or(Format::Raster)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_map_to_their_decoder() {
        let f = |s: &str| Format::of(Path::new(s));
        assert_eq!(f("a.TIF"), Some(Format::Tiff));
        assert_eq!(f("a.tiff"), Some(Format::Tiff));
        assert_eq!(f("a.j2k"), Some(Format::Jp2));
        assert_eq!(f("a.Png"), Some(Format::Raster));
        assert_eq!(f("a.mp4"), Some(Format::Video));
        assert_eq!(f("a.txt"), None);
        assert_eq!(f("noext"), None);
    }

    #[test]
    fn every_jp2_extension_is_loadable() {
        for ext in super::super::jp2::EXTS {
            assert!(LOADABLE_EXTS.contains(ext), "{ext}");
        }
    }
}
