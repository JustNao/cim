//! Video (mp4/avi) media, decoded through the **ffmpeg CLI** — the same
//! external tool export already uses to encode. `ffprobe` reads the stream
//! metadata up front (so a video's length is always known — no lazy discovery),
//! and a persistent [`VideoReader`] streams rawvideo frames from a long-lived
//! `ffmpeg` child process: sequential decodes just read the next frame off the
//! pipe; a non-sequential index respawns the child with an accurate `-ss` seek.
//! Small reorderings — which the decode pool produces routinely, since several
//! workers race for this one reader — are absorbed instead of seeking: a request
//! a few frames ahead reads forward to it, and one a few frames behind is served
//! from the frames just read (see [`VideoReader::decode`]).
//!
//! Frames are always 8-bit (`rgb24`, or `gray` for grayscale sources — mono
//! keeps the Colormap tone usable); higher-bit-depth sources are tone-mapped
//! down by ffmpeg. Frame ↔ time mapping assumes **constant frame rate** (via
//! the stream's average rate), so a variable-frame-rate file may land ±1 frame
//! on seeks — a documented limitation.

use rust_i18n::t;
use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};

use super::source::{Media, SeqCache, VideoSeq};
use super::{FrameData, Samples};

/// Stream metadata from `ffprobe`: enough to size the cache and seek.
pub struct VideoMeta {
    pub size: [usize; 2],
    /// Total frame count. Usually exact (`nb_frames` / a packet count); as a
    /// last resort estimated from duration × fps, so the stream may end a frame
    /// or two early — `VideoReader::decode` treats a clean EOF as the real end.
    pub frame_count: usize,
    /// Average frame rate, assumed constant (see module docs).
    pub fps: f64,
    /// Source pixel format is grayscale → decode mono (`gray`) not `rgb24`.
    pub gray: bool,
}

/// The fields `parse_ffprobe_output` can extract; `probe_video` fills the
/// frame-count gap with its fallback chain.
struct PartialMeta {
    size: [usize; 2],
    fps: f64,
    gray: bool,
    nb_frames: Option<usize>,
    duration: Option<f64>,
}

/// Hint appended to a "tool not found" error; built per call so it follows the
/// UI language (a `const` would freeze it at compile time).
fn ffmpeg_hint() -> String {
    t!("error.ffmpeg_hint").into_owned()
}

fn ffprobe_stream(path: &Path, entries: &str) -> Result<String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg(entries)
        .args(["-of", "default=noprint_wrappers=1"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => anyhow!(t!(
                "error.tool_not_found",
                tool = "ffprobe",
                hint = ffmpeg_hint()
            )
            .into_owned()),
            _ => anyhow!(t!("error.tool_failed", tool = "ffprobe", err = e).into_owned()),
        })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(t!(
            "error.ffprobe_failed",
            path = path.display(),
            err = err.trim()
        )
        .into_owned()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Read the video stream's metadata with `ffprobe`. The cheap header pass
/// usually has everything; when it can't state a frame count (`nb_frames` is
/// absent/`N/A` — some AVIs), fall back to an exact packet count (reads the
/// whole file, demux only), then to duration × fps.
pub fn probe_video(path: &Path) -> Result<VideoMeta> {
    let text = ffprobe_stream(
        path,
        "stream=width,height,pix_fmt,avg_frame_rate,r_frame_rate,nb_frames,duration",
    )?;
    let meta = parse_ffprobe_output(&text)
        .with_context(|| format!("unsupported video stream in {}", path.display()))?;

    let frame_count = match meta.nb_frames {
        Some(n) => n,
        None => count_packets(path)
            .or_else(|| meta.duration.map(|d| (d * meta.fps).round() as usize))
            .filter(|&n| n > 0)
            .ok_or_else(|| {
                anyhow!(t!("error.no_frame_count", path = path.display()).into_owned())
            })?,
    };
    Ok(VideoMeta {
        size: meta.size,
        frame_count,
        fps: meta.fps,
        gray: meta.gray,
    })
}

/// Exact frame count by demuxing every packet of the video stream (no decode).
fn count_packets(path: &Path) -> Option<usize> {
    let text = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-count_packets"])
        .args(["-show_entries", "stream=nb_read_packets"])
        .args(["-of", "default=noprint_wrappers=1"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())?;
    field(&text, "nb_read_packets").and_then(|v| v.parse().ok())
}

fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty() && *v != "N/A")
}

/// Parse a `num/den` rational (ffprobe's frame-rate form, e.g. `30000/1001`),
/// or a plain number. Zero / undefined (`0/0`) → `None`.
fn parse_rate(v: &str) -> Option<f64> {
    let rate = match v.split_once('/') {
        Some((num, den)) => num.parse::<f64>().ok()? / den.parse::<f64>().ok()?,
        None => v.parse::<f64>().ok()?,
    };
    (rate.is_finite() && rate > 0.0).then_some(rate)
}

/// Extract the metadata from ffprobe's `key=value` lines. Pure, so the parsing
/// is unit-testable without ffprobe installed.
fn parse_ffprobe_output(text: &str) -> Result<PartialMeta> {
    let dim = |key| {
        field(text, key)
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .ok_or_else(|| anyhow!("missing {key}"))
    };
    let fps = field(text, "avg_frame_rate")
        .and_then(parse_rate)
        .or_else(|| field(text, "r_frame_rate").and_then(parse_rate))
        .ok_or_else(|| anyhow!("missing frame rate"))?;
    Ok(PartialMeta {
        size: [dim("width")?, dim("height")?],
        fps,
        gray: field(text, "pix_fmt").is_some_and(|p| p.starts_with("gray")),
        nb_frames: field(text, "nb_frames").and_then(|v| v.parse().ok()),
        duration: field(text, "duration").and_then(|v| v.parse().ok()),
    })
}

/// The `-ss` seek time that lands exactly on frame `idx` under CFR: the
/// midpoint of the preceding frame interval, so frame `idx-1` (pts
/// `(idx-1)/fps`) is before it and frame `idx` (pts `idx/fps`) at/after it —
/// robust to rounding in either direction.
fn seek_seconds(idx: usize, fps: f64) -> f64 {
    ((idx as f64) - 0.5).max(0.0) / fps
}

/// How far past the stream's position a request may be and still be reached by
/// reading forward rather than respawning with a seek. A seek is a process
/// spawn plus a decode from the previous keyframe — up to a whole GOP of 4K
/// frames — so reading a few frames through is the cheaper way there.
const READ_FORWARD: usize = 16;

/// Frames kept after they are read, so a request that arrives a little late
/// (its worker lost the race for this reader to one asking for a later frame)
/// is answered without seeking back. Matches the deepest playback prefetch, the
/// widest spread of video jobs that can be queued at once.
const RECENT: usize = 8;

/// Persistent streaming decoder for one video file: a long-lived `ffmpeg`
/// child writing rawvideo frames to stdout. Owned behind the decode pool's
/// per-pane reader mutex (like a TIFF's `SeqReader`), or per export pane.
pub struct VideoReader {
    path: PathBuf,
    meta: VideoMeta,
    child: Option<Child>,
    stdout: Option<ChildStdout>,
    /// The frame index the stream will yield next.
    next: usize,
    /// The last [`RECENT`] frames read off the stream, oldest first. Shared
    /// (`Arc`) with whoever received them, so holding them costs nothing while
    /// the frame cache has them too.
    recent: VecDeque<(usize, Arc<FrameData>)>,
    /// How many times the child was (re)spawned — the expensive event; tests
    /// assert small reorderings don't cause one.
    spawns: usize,
}

impl VideoReader {
    /// Probe the file's metadata; the ffmpeg child is spawned lazily on the
    /// first decode.
    pub fn open(path: &Path) -> Result<Self> {
        let meta = probe_video(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            meta,
            child: None,
            stdout: None,
            next: 0,
            recent: VecDeque::with_capacity(RECENT),
            spawns: 0,
        })
    }

    fn kill_child(&mut self) {
        self.stdout = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// (Re)spawn the streaming child so its next output frame is `idx`.
    fn spawn_at(&mut self, idx: usize) -> Result<()> {
        self.kill_child();
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error", "-nostdin"]);
        if idx > 0 {
            cmd.args(["-ss", &format!("{:.6}", seek_seconds(idx, self.meta.fps))]);
        }
        // `-v error` + discarded stderr: ffmpeg can never block on an unread
        // stderr pipe; a failure surfaces as a short read + exit status below.
        cmd.arg("-i")
            .arg(&self.path)
            .args(["-f", "rawvideo", "-pix_fmt"])
            .arg(if self.meta.gray { "gray" } else { "rgb24" })
            .arg("pipe:1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => anyhow!(t!(
                "error.tool_not_found",
                tool = "ffmpeg",
                hint = ffmpeg_hint()
            )
            .into_owned()),
            _ => anyhow!(t!("error.tool_failed", tool = "ffmpeg", err = e).into_owned()),
        })?;
        self.stdout = child.stdout.take();
        self.child = Some(child);
        self.next = idx;
        self.spawns += 1;
        Ok(())
    }

    /// Decode frame `idx`; `Ok(None)` = past the last frame (also when the
    /// stream ends a little before an *estimated* frame count).
    ///
    /// Only a real jump seeks. The decode pool hands this reader's frames to
    /// several workers, which then queue on its mutex in no particular order,
    /// so playback's prefetch of frames `f+1, f+2, …` routinely arrives as
    /// `f+3, f+1, f+2`. Seeking on each of those restarted ffmpeg and decoded
    /// from the previous keyframe every time — on a long-GOP 4K file, a stall
    /// that grew through each GOP and reset at the next keyframe. Instead:
    /// - a frame just read is returned from [`RECENT`] memory;
    /// - a frame up to [`READ_FORWARD`] ahead is reached by reading on (the
    ///   frames passed are kept, so their own requests hit memory next);
    /// - anything else respawns the child with a seek, as before.
    pub fn decode(&mut self, idx: usize) -> Result<Option<Arc<FrameData>>> {
        if idx >= self.meta.frame_count {
            return Ok(None);
        }
        if let Some((_, f)) = self.recent.iter().find(|(i, _)| *i == idx) {
            return Ok(Some(Arc::clone(f)));
        }
        if !(self.next..=self.next + READ_FORWARD).contains(&idx) {
            self.spawn_at(idx)?;
        } else if self.stdout.is_none() {
            // No stream yet (or it ended): start it where we are rather than
            // at `idx`, so the frames just before `idx` — likely requested
            // next, by the workers that lost the race — are read on the way.
            self.spawn_at(self.next)?;
        }
        loop {
            match self.read_next()? {
                None => return Ok(None),
                Some(f) if self.next == idx + 1 => return Ok(Some(f)),
                Some(_) => {} // passed on the way; kept in `recent`
            }
        }
    }

    /// Read the stream's next frame (index `self.next`) and remember it.
    fn read_next(&mut self) -> Result<Option<Arc<FrameData>>> {
        let idx = self.next;
        let [w, h] = self.meta.size;
        let channels = if self.meta.gray { 1 } else { 3 };
        let mut buf = vec![0u8; w * h * channels];
        match self.stdout.as_mut().unwrap().read_exact(&mut buf) {
            Ok(()) => {
                self.next += 1;
                let f = Arc::new(FrameData::new([w, h], channels, Samples::U8(buf)));
                if self.recent.len() == RECENT {
                    self.recent.pop_front();
                }
                self.recent.push_back((idx, Arc::clone(&f)));
                Ok(Some(f))
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // The stream ended cleanly before the declared count (an
                // estimate, or a truncated file): treat it as the real end.
                let status = self.child.as_mut().and_then(|c| c.wait().ok());
                self.kill_child();
                match status {
                    Some(s) if s.success() => Ok(None),
                    s => Err(anyhow!(t!(
                        "error.ffmpeg_stopped",
                        n = idx,
                        path = self.path.display(),
                        status = s.map(|s| format!(" ({s})")).unwrap_or_default()
                    )
                    .into_owned())),
                }
            }
            Err(e) => {
                self.kill_child();
                Err(anyhow!(t!(
                    "error.ffmpeg_read",
                    n = idx,
                    path = self.path.display(),
                    err = e
                )
                .into_owned()))
            }
        }
    }
}

impl Drop for VideoReader {
    fn drop(&mut self) {
        self.kill_child();
    }
}

/// Open a video file as a `Media`: probe its length/size up front (so it is
/// always "at end" — no lazy discovery, no probes) and eagerly decode frame 0
/// with a short-lived reader so the pane shows something immediately (this
/// also validates that `ffmpeg` itself — not just ffprobe — is present; a
/// failure here is tolerated and left to the pool to report per-frame).
pub(super) fn open_video(path: &Path, name: String) -> Result<Media> {
    let meta = probe_video(path)?;
    let mut frames = SeqCache::new(meta.frame_count);
    if let Ok(mut reader) = VideoReader::open(path) {
        if let Ok(Some(first)) = reader.decode(0) {
            frames.insert(0, first);
        }
    }
    Ok(Media::Video(VideoSeq {
        name,
        path: path.to_path_buf(),
        size: meta.size,
        fps: meta.fps,
        frames,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MP4_PROBE: &str = "\
width=1920
height=1080
pix_fmt=yuv420p
r_frame_rate=25/1
avg_frame_rate=25/1
duration=4.000000
nb_frames=100
";

    #[test]
    fn parses_full_mp4_output() {
        let m = parse_ffprobe_output(MP4_PROBE).unwrap();
        assert_eq!(m.size, [1920, 1080]);
        assert_eq!(m.fps, 25.0);
        assert!(!m.gray);
        assert_eq!(m.nb_frames, Some(100));
        assert_eq!(m.duration, Some(4.0));
    }

    #[test]
    fn missing_nb_frames_falls_back() {
        let text = "width=64\nheight=48\npix_fmt=yuv420p\navg_frame_rate=10/1\n\
                    r_frame_rate=10/1\nnb_frames=N/A\nduration=N/A\n";
        let m = parse_ffprobe_output(text).unwrap();
        assert_eq!(m.nb_frames, None);
        assert_eq!(m.duration, None);
    }

    #[test]
    fn parses_ntsc_rate_and_gray() {
        let text = "width=720\nheight=480\npix_fmt=gray\navg_frame_rate=30000/1001\nduration=1\n";
        let m = parse_ffprobe_output(text).unwrap();
        assert!((m.fps - 29.97).abs() < 0.01);
        assert!(m.gray);
    }

    #[test]
    fn falls_back_to_r_frame_rate() {
        let text = "width=8\nheight=8\navg_frame_rate=0/0\nr_frame_rate=24/1\n";
        assert_eq!(parse_ffprobe_output(text).unwrap().fps, 24.0);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_ffprobe_output("no video stream here").is_err());
        assert!(parse_ffprobe_output("width=0\nheight=8\navg_frame_rate=25/1\n").is_err());
        assert!(parse_ffprobe_output("width=8\nheight=8\navg_frame_rate=0/0\n").is_err());
    }

    #[test]
    fn seek_lands_inside_the_right_interval() {
        assert_eq!(seek_seconds(0, 25.0), 0.0);
        for &fps in &[10.0, 25.0, 30000.0 / 1001.0, 60.0] {
            for idx in 1..200usize {
                let t = seek_seconds(idx, fps);
                assert!((idx as f64 - 1.0) / fps < t, "idx {idx} fps {fps}");
                assert!(t < idx as f64 / fps, "idx {idx} fps {fps}");
            }
        }
    }

    // ---- integration tests (need the ffmpeg CLI; skip gracefully) --------

    /// Generate a 10-frame 64×48 test video with ffmpeg's `testsrc` pattern,
    /// or `None` when ffmpeg isn't installed (the ffmpeg-dependent tests then
    /// skip, like the export encode test).
    fn fixture_video(ext: &str) -> Option<PathBuf> {
        let path = crate::testutil::fixture_dir("video").join(format!("clip.{ext}"));
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=64x48:rate=10:duration=1")
            .args(["-pix_fmt", "yuv420p"])
            .arg(&path)
            .stdin(Stdio::null())
            .status()
            .ok()?;
        status.success().then_some(path)
    }

    fn frame_bytes(f: &FrameData) -> &[u8] {
        match &f.samples {
            Samples::U8(v) => v,
            _ => panic!("expected u8 samples"),
        }
    }

    #[test]
    fn probes_and_loads_a_video() {
        for ext in ["mp4", "avi"] {
            let Some(path) = fixture_video(ext) else {
                return; // ffmpeg not installed
            };
            let meta = probe_video(&path).expect("probe");
            assert_eq!(meta.size, [64, 48], "{ext}");
            assert_eq!(meta.frame_count, 10, "{ext}");
            assert!((meta.fps - 10.0).abs() < 0.01, "{ext}");
            assert!(!meta.gray, "{ext}");

            let media = crate::media::load(&path).expect("load");
            assert!(matches!(media, Media::Video(_)), "{ext}");
            assert_eq!(media.frame_count(), 10, "{ext}");
            assert!(media.at_end(), "{ext}");
            assert!(media.is_sequence(), "{ext}");
            assert!(!media.hi_depth(), "{ext}");
            assert!(media.resident(0).is_some(), "{ext}: frame 0 eager");
            assert!(media.probe_job(1).is_none(), "{ext}: never probed");
            assert!(media.decode_job(1).is_some(), "{ext}");
            assert!(media.decode_job(10).is_none(), "{ext}: past the end");
        }
    }

    #[test]
    fn reader_streams_and_seeks_exactly() {
        let Some(path) = fixture_video("mp4") else {
            return; // ffmpeg not installed
        };
        let mut reader = VideoReader::open(&path).expect("open");
        // Sequential walk: every frame decodes, at the right size/depth.
        let mut frames = Vec::new();
        for idx in 0..10 {
            let f = reader.decode(idx).expect("decode").expect("frame");
            assert_eq!(f.size, [64, 48]);
            assert_eq!(f.channels, 3);
            frames.push(f);
        }
        assert!(reader.decode(10).expect("past end").is_none());
        // testsrc frames really differ — otherwise the seek checks prove nothing.
        assert_ne!(frame_bytes(&frames[3]), frame_bytes(&frames[4]));
        assert_eq!(reader.spawns, 1, "a sequential walk never respawns");
        // A frame just read comes back from memory, no seek.
        let f7 = reader.decode(7).expect("recent").expect("frame 7");
        assert_eq!(frame_bytes(&f7), frame_bytes(&frames[7]), "recent 7");
        assert_eq!(reader.spawns, 1);
        // Further back than that respawns ffmpeg with `-ss`; the landed frame
        // must be byte-identical to the sequential walk's (testsrc frames
        // differ, so an off-by-one seek would show here).
        let f1 = reader.decode(1).expect("seek back").expect("frame 1");
        assert_eq!(frame_bytes(&f1), frame_bytes(&frames[1]), "seek to 1");
        assert_eq!(reader.spawns, 2);
        // And the stream continues sequentially from a seek landing.
        let f2 = reader.decode(2).expect("resume").expect("frame 2");
        assert_eq!(frame_bytes(&f2), frame_bytes(&frames[2]), "resume at 2");
        assert_eq!(reader.spawns, 2);
    }

    /// The decode pool's workers race for one video reader, so playback's
    /// prefetch reaches it out of order. Small reorderings must be absorbed —
    /// read forward or served from the recent frames — with **no** respawn
    /// (each one was a seek from the last keyframe: the periodic 4K stalls),
    /// and every frame must still be the right one. Only a real jump seeks.
    #[test]
    fn reader_absorbs_out_of_order_requests_without_seeking() {
        let path = crate::testutil::fixture_dir("video").join("long.mp4");
        let made = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=64x48:rate=10:duration=6")
            .args(["-pix_fmt", "yuv420p", "-g", "30"])
            .arg(&path)
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            return; // ffmpeg not installed
        }
        let mut reference = VideoReader::open(&path).expect("open");
        let want: Vec<Vec<u8>> = (0..60)
            .map(|i| frame_bytes(&reference.decode(i).unwrap().unwrap()).to_vec())
            .collect();

        let mut reader = VideoReader::open(&path).expect("open");
        // Each window of four prefetched frames lands shuffled, as four
        // workers winning the reader's lock in arbitrary order would.
        let order: Vec<usize> = (0..40)
            .step_by(4)
            .flat_map(|b| [b + 2, b, b + 3, b + 1])
            .collect();
        for &i in &order {
            let f = reader.decode(i).expect("decode").expect("frame");
            assert_eq!(frame_bytes(&f), want[i], "frame {i}");
        }
        assert_eq!(
            reader.spawns, 1,
            "reordering within the prefetch window never seeks"
        );

        // A jump past the read-forward window does seek, and lands exactly.
        let far = reader.next + READ_FORWARD + 1;
        let f = reader.decode(far).expect("jump").expect("frame");
        assert_eq!(frame_bytes(&f), want[far], "jump to {far}");
        assert_eq!(reader.spawns, 2);
    }
}
