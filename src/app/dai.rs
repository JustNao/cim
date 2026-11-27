//! A pane's DAI (auxiliary per-image information, [`crate::dai`]): requesting it
//! off the UI thread when the media opens or reloads, applying what lands, and
//! the one use of it so far — the camera rotation added to the display rotation.

use super::*;

impl CimApp {
    /// Look for pane `i`'s DAI files in the background (`crate::dai::DaiLoader`)
    /// — or read the one the user named for it (`Pane.dai_file`, `--dai`).
    /// A Compute pane has no files and so none. The pane keeps whatever DAI it
    /// already had until the new lookup lands, so a reload doesn't flick the
    /// rotation off for a frame.
    pub(super) fn request_dai(&mut self, i: usize) {
        let Some(p) = self.panes.get_mut(i) else {
            return;
        };
        let paths = match &p.source {
            Source::File(path) => vec![path.clone()],
            Source::Sequence { files, .. } => files.clone(),
            Source::Computed => return,
        };
        self.work.dai_gen += 1;
        let gen = self.work.dai_gen;
        p.dai_req = Some(gen);
        self.work
            .dai
            .request(p.id, gen, paths, p.media.is_sequence(), p.dai_file.clone());
    }

    /// Apply finished DAI lookups to their still-matching pane. A file that was
    /// found but couldn't be read is worth a status note; a media with no DAI at
    /// all is the ordinary case and says nothing.
    pub(super) fn pump_dai(&mut self) {
        for done in self.work.dai.drain() {
            let Some(i) = self.panes.iter().position(|p| p.id == done.id) else {
                continue; // pane closed
            };
            if self.panes[i].dai_req != Some(done.gen) {
                continue; // superseded by a reload
            }
            self.panes[i].dai_req = None;
            self.panes[i].dai = done.dai;
            if let Some(err) = done.errors.first() {
                self.status.set(t!("status.dai_read_failed", err = err));
            }
        }
    }

    /// Whether pane `i` has DAI carrying a camera rotation (which is what shows
    /// the Transformations panel's **with DAI** toggle).
    pub(super) fn has_dai_rotation(&self, i: usize) -> bool {
        self.panes[i].dai.as_ref().is_some_and(|d| d.has_rotation())
    }

    /// The DAI camera rotation of pane `i`'s frame `frame`, in degrees (0 when
    /// it has none).
    pub(super) fn dai_rotation_at(&self, i: usize, frame: usize) -> f32 {
        let p = &self.panes[i];
        p.dai
            .as_ref()
            .and_then(|d| d.rotation_deg(&p.media, frame))
            .unwrap_or(0.0)
    }

    /// The DAI rotation pane `i` is drawn with right now, in degrees: 0 when the
    /// **with DAI** toggle is off. Read for the frame the committed texture shows
    /// (as `disp_size` is), so the angle turns with the image rather than ahead
    /// of it while the next frame is still rendering.
    pub(super) fn dai_rotation(&self, i: usize) -> f32 {
        let p = &self.panes[i];
        if !p.dai_rotate || p.dai.is_none() {
            return 0.0;
        }
        let frame = p
            .tex
            .front
            .as_ref()
            .map_or_else(|| self.frame_disp(i), |t| t.shown);
        self.dai_rotation_at(i, frame)
    }

    /// Pane `i`'s DAI rotation per source frame, in **radians**, for the export
    /// (`ExportPane::dai_rotation`) — empty when it adds none, so the export
    /// keeps its fixed-rotation path.
    pub(super) fn dai_rotation_track(&self, i: usize) -> Vec<f32> {
        let p = &self.panes[i];
        if !p.dai_rotate || !self.has_dai_rotation(i) {
            return Vec::new();
        }
        (0..p.media.frame_count())
            .map(|f| self.dai_rotation_at(i, f).to_radians())
            .collect()
    }
}
