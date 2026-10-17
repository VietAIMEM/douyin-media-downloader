//! Small preview images for the media on disk.
//!
//! # What this is for
//!
//! The Dashboard shows what is really in the download folder. A list of file
//! names answers "what is there"; it does not answer "which one is this". A
//! thumbnail does, and for a folder it is the difference between a row of names
//! and something a person can recognise at a glance.
//!
//! # The rules this follows
//!
//! * **The user's media is never touched.** A thumbnail is read from the source
//!   and written to this application's own data directory. Nothing is written
//!   beside the original, and the original is never opened for writing.
//! * **Bounded work.** A video is seeked to a point and one frame is decoded —
//!   `-ss` before `-i`, so the seek is an index jump rather than a decode of
//!   everything before it. No file is ever decoded end to end to make a picture.
//! * **Cached by identity, not by name.** The cache key is the path together
//!   with the file's size and modification time, so editing or replacing a file
//!   produces a different key and the stale picture is never shown. Nothing has
//!   to be invalidated by hand, and no clock or cleanup schedule is involved.
//! * **Never fatal.** No FFmpeg, an unreadable file, a format this cannot decode:
//!   all of them mean "no thumbnail", and the caller shows a placeholder. The
//!   Dashboard must not break because a picture could not be made.
//! * **No console window.** FFmpeg is a console program, and a GUI application
//!   that starts one without `CREATE_NO_WINDOW` gets a black rectangle flashing
//!   over the screen. It goes through the same [`NoConsoleWindow`] trait as every
//!   other child process this application launches.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::process::NoConsoleWindow;

/// Width of a generated thumbnail, in pixels.
///
/// A folder of these is drawn at roughly 200 pixels wide, so 320 leaves room for
/// a high-density display without making the file big. The height follows from
/// the aspect ratio, which is why the scale filter takes `-2` rather than a
/// number: an odd height cannot be encoded as JPEG.
const THUMBNAIL_WIDTH: u32 = 320;

/// JPEG quality. 4 is ffmpeg's `-q:v` scale for "good enough for a card".
const JPEG_QUALITY: &str = "4";

/// The muxer to write, named explicitly rather than left to the extension.
const OUTPUT_FORMAT: &str = "image2";

/// Where a video is seeked to before its frame is taken.
///
/// Not zero: the first frames of a great many videos are a fade from black or a
/// solid title card, which makes every thumbnail in a folder look the same. One
/// second in is past almost all of that and is still recognisably the start. A
/// video shorter than this is retried at the beginning rather than being left
/// without a picture.
const SEEK_SECONDS: &str = "1";

/// How many frames the picker chooses between, once the seek has landed.
///
/// Seeking past a black opening is not enough on its own — measured on a real
/// download, the frame at one second was still pure black (`mean luma 0.5`) and
/// the picture only appeared by two. `thumbnail` decodes this many frames and
/// keeps the one most like the rest, which lands on content rather than on the
/// tail of a fade. It is bounded work: at 25 fps this is under two seconds of
/// video, decoded *after* the frame has been scaled down, and it cost 29
/// milliseconds on the file that exposed the problem.
///
/// It is not a fix for a video that is black throughout its opening — nothing
/// chosen from the frames available can be — but it is the difference between a
/// folder of black squares and a folder of recognisable pictures.
const FRAMES_TO_CHOOSE_FROM: u32 = 40;

/// How long one extraction is allowed to take before it is given up on.
///
/// Generous, because a cold start of ffmpeg on a large file can take a few
/// seconds; bounded, because a stuck child would otherwise hold one of the
/// blocking threads for the life of the process.
const EXTRACTION_TIMEOUT: Duration = Duration::from_secs(15);

/// Most thumbnails kept on disk before the oldest are dropped.
///
/// Every entry is small — a few tens of kilobytes — so this is a disk-space
/// bound of roughly a hundred megabytes at the very worst, in the application's
/// own data directory. It exists because nothing else would ever remove them:
/// the key changes when a file changes, so old entries are never overwritten,
/// and a library that churns would otherwise grow without limit.
const MAX_CACHED: usize = 4000;

/// What kind of source a thumbnail is being asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A still picture: scaled, never re-encoded from a decoded video.
    Image,
    /// A video: one frame is taken, never the whole file.
    Video,
    /// Something this application has no way to draw — audio, text, anything
    /// else. There is no picture to make, and inventing one would be a lie.
    Unsupported,
}

/// Makes and caches preview images.
///
/// Cheap to clone in the sense that matters: it holds a directory and an
/// optional executable path, and all state lives on disk.
#[derive(Debug, Clone)]
pub struct Thumbnailer {
    dir: PathBuf,
    /// `None` when no FFmpeg was found. Everything still works; there are simply
    /// no thumbnails, which is a placeholder rather than a failure.
    ffmpeg: Option<PathBuf>,
}

impl Thumbnailer {
    /// Find FFmpeg the same way the muxer does, and use `dir` as the cache.
    pub fn discover(dir: PathBuf) -> Self {
        let ffmpeg = crate::media::ffmpeg::discover_tool("ffmpeg", "DOUYIN_FFMPEG");
        if ffmpeg.is_none() {
            tracing::info!(
                target: "thumbnails",
                "no ffmpeg found; the Dashboard will show placeholders instead of previews"
            );
        }
        Self { dir, ffmpeg }
    }

    /// A thumbnailer with a stated executable — `None` meaning "as if FFmpeg
    /// were not installed", which is what the tests for the degraded path need.
    pub fn with_ffmpeg(dir: PathBuf, ffmpeg: Option<PathBuf>) -> Self {
        Self { dir, ffmpeg }
    }

    /// Whether previews can be generated at all.
    pub fn is_available(&self) -> bool {
        self.ffmpeg.is_some()
    }

    /// The cache directory. The window is allowed to read this one and nothing
    /// else outside the download folder.
    pub fn cache_dir(&self) -> &Path {
        &self.dir
    }

    /// The cached thumbnail for `source`, without generating anything.
    pub fn cached(&self, source: &Path) -> Option<PathBuf> {
        let path = self.cache_path(source)?;
        path.is_file().then_some(path)
    }

    /// The thumbnail for `source`, generating it if it is not already cached.
    ///
    /// Blocking: callers run it on a blocking thread. Returns `None` whenever a
    /// picture cannot be made, which is an ordinary outcome and not an error —
    /// the caller draws a placeholder.
    pub fn ensure(&self, source: &Path, kind: Source) -> Option<PathBuf> {
        if kind == Source::Unsupported {
            return None;
        }
        if let Some(hit) = self.cached(source) {
            return Some(hit);
        }
        let executable = self.ffmpeg.as_ref()?;
        let output = self.cache_path(source)?;
        if std::fs::create_dir_all(&self.dir).is_err() {
            return None;
        }

        // Written to a neighbouring temporary name and moved into place, so a
        // half-written JPEG is never left where a later run would take it for a
        // finished one. The rename is within one directory, so it is atomic.
        let staging = output.with_extension("jpg.part");
        let _ = std::fs::remove_file(&staging);

        let made = self.extract(executable, source, &staging, kind);
        let ok = made.is_some_and(|()| staging.is_file() && size_of(&staging) > 0);
        if !ok {
            let _ = std::fs::remove_file(&staging);
            tracing::debug!(
                target: "thumbnails",
                source = %source.display(),
                "no thumbnail could be made; the card will show a placeholder"
            );
            return None;
        }
        std::fs::rename(&staging, &output).ok()?;
        self.enforce_cap();
        Some(output)
    }

    /// Run ffmpeg once, into `staging`.
    fn extract(&self, executable: &Path, source: &Path, staging: &Path, kind: Source) -> Option<()> {
        match kind {
            Source::Image => self.run(executable, image_arguments(source, staging))?,
            Source::Video => {
                let mut arguments = video_arguments(source, staging, SEEK_SECONDS);
                if self.run(executable, arguments.clone()).is_none() {
                    // Shorter than the seek point: take the first frame instead.
                    arguments = video_arguments(source, staging, "0");
                    self.run(executable, arguments)?;
                }
            }
            Source::Unsupported => return None,
        }
        Some(())
    }

    /// Start ffmpeg and wait for it, with a bound on how long that may take.
    fn run(&self, executable: &Path, arguments: Vec<String>) -> Option<()> {
        let mut command = std::process::Command::new(executable);
        command
            .args(&arguments)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // The same treatment as every other child: no console, no black flash.
        command.no_console_window();

        let mut child = command.spawn().ok()?;
        let deadline = Instant::now() + EXTRACTION_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success().then_some(()),
                Ok(None) if Instant::now() >= deadline => {
                    // Given up on, and said so: a silent kill would look like a
                    // corrupt file rather than a slow one.
                    tracing::warn!(
                        target: "thumbnails",
                        source = %self.redacted(executable),
                        "ffmpeg did not finish in time; the extraction was stopped"
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(40)),
                Err(_) => return None,
            }
        }
    }

    /// The executable's name only — a full path is nobody's business in a log.
    fn redacted(&self, executable: &Path) -> String {
        executable
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "ffmpeg".into())
    }

    /// Drop the oldest thumbnails once the cache is over its bound.
    ///
    /// Only ever removes files this module wrote — every name in here is a
    /// generated key with a `.jpg` extension. A failure is not worth reporting:
    /// the worst case is a slightly larger cache.
    fn enforce_cap(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut thumbnails: Vec<(SystemTime, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jpg"))
            .filter_map(|entry| {
                let modified = entry.metadata().ok()?.modified().ok()?;
                Some((modified, entry.path()))
            })
            .collect();
        if thumbnails.len() <= MAX_CACHED {
            return;
        }
        // Oldest first, so the ones removed are the least likely to be wanted.
        thumbnails.sort_by_key(|(modified, _)| *modified);
        for (_, path) in thumbnails.iter().take(thumbnails.len() - MAX_CACHED) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Where the thumbnail for `source` belongs, if it can have one.
    fn cache_path(&self, source: &Path) -> Option<PathBuf> {
        Some(self.dir.join(format!("{}.jpg", cache_key(source)?)))
    }
}

/// Arguments for scaling a still picture.
fn image_arguments(source: &Path, output: &Path) -> Vec<String> {
    vec![
        "-v".into(),
        "error".into(),
        "-y".into(),
        "-i".into(),
        source.to_string_lossy().into_owned(),
        "-vf".into(),
        scale_filter(),
        "-frames:v".into(),
        "1".into(),
        "-q:v".into(),
        JPEG_QUALITY.into(),
        // Stated rather than inferred. The output is written under a staging
        // name ending in `.part` — so a half-written file can never be mistaken
        // for a finished one — and ffmpeg picks its muxer from the extension,
        // which it would not recognise.
        "-f".into(),
        OUTPUT_FORMAT.into(),
        output.to_string_lossy().into_owned(),
    ]
}

/// Arguments for taking one frame from a video, seeked to `at`.
///
/// `-ss` comes **before** `-i` deliberately. After it, ffmpeg decodes every
/// frame up to that point to arrive at it; before it, ffmpeg seeks the index and
/// starts there. On a long video that is the difference between a thumbnail in
/// milliseconds and one that reads the whole file.
fn video_arguments(source: &Path, output: &Path, at: &str) -> Vec<String> {
    vec![
        "-v".into(),
        "error".into(),
        "-y".into(),
        "-ss".into(),
        at.into(),
        "-i".into(),
        source.to_string_lossy().into_owned(),
        "-vf".into(),
        video_filter(),
        "-frames:v".into(),
        "1".into(),
        "-q:v".into(),
        JPEG_QUALITY.into(),
        "-f".into(),
        OUTPUT_FORMAT.into(),
        output.to_string_lossy().into_owned(),
    ]
}

/// Fit inside `THUMBNAIL_WIDTH`, keeping the aspect ratio.
///
/// `-2` rather than `-1` for the height: an odd number of rows cannot be encoded
/// as JPEG, and ffmpeg fails the whole extraction rather than rounding.
fn scale_filter() -> String {
    format!("scale='min({THUMBNAIL_WIDTH},iw)':-2")
}

/// The chain a video goes through: scaled down first, then one frame chosen.
///
/// The order is deliberate and was measured. Scaling first means the picker
/// holds small frames rather than full-size ones, and it was the faster of the
/// two arrangements on the file that prompted this.
fn video_filter() -> String {
    format!("{},thumbnail=n={FRAMES_TO_CHOOSE_FROM}", scale_filter())
}

/// A stable name for one version of one file.
///
/// Built from the path **as the filesystem spells it**, its size, and its
/// modification time. Two consequences worth stating: a file whose contents
/// change gets a different key, so the old picture is never served for the new
/// file; and a file that is merely looked at twice gets the same key, so the
/// picture is made once.
///
/// Two independent FNV-1a passes give 128 bits. Not for security — nothing here
/// is adversarial — but because a collision would show one file's picture on
/// another file's card, which is worse than showing none. Two 64-bit passes make
/// that possibility negligible without pulling in a hashing crate.
fn cache_key(source: &Path) -> Option<String> {
    let metadata = std::fs::metadata(source).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_nanos())
        .unwrap_or(0);

    // The path is taken as a *string* and, on Windows, folded to lower case, so
    // the same file reached by two spellings shares one thumbnail.
    let normalized = crate::storage::filenames::comparison_key(source);
    let material = format!("{normalized}\u{1}{}\u{1}{modified}", metadata.len());

    Some(format!(
        "{:016x}{:016x}",
        fnv1a(material.as_bytes(), 0xcbf2_9ce4_8422_2325),
        fnv1a(material.as_bytes(), 0x9e37_79b9_7f4a_7c15),
    ))
}

/// FNV-1a, 64-bit, over `bytes` starting from `seed`.
fn fnv1a(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A file's size, or zero when it cannot be read.
fn size_of(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    /// A real ffmpeg, if this machine has one. The tests that need it say so.
    fn real_ffmpeg() -> Option<PathBuf> {
        crate::media::ffmpeg::discover_tool("ffmpeg", "DOUYIN_FFMPEG")
    }

    /// Render a small image with ffmpeg, so the fixtures are real files rather
    /// than bytes that only look like one.
    fn write_image(ffmpeg: &Path, at: &Path, size: &str) {
        let status = std::process::Command::new(ffmpeg)
            .args([
                "-v", "error", "-y", "-f", "lavfi", "-i",
                &format!("testsrc=size={size}:duration=1"),
                "-frames:v", "1", &at.to_string_lossy(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("ffmpeg starts");
        assert!(status.success(), "the fixture image must be rendered");
    }

    fn write_video(ffmpeg: &Path, at: &Path, seconds: u32) {
        let status = std::process::Command::new(ffmpeg)
            .args([
                "-v", "error", "-y", "-f", "lavfi", "-i",
                &format!("testsrc=size=160x120:rate=10:duration={seconds}"),
                "-c:v", "libx264", "-pix_fmt", "yuv420p",
                &at.to_string_lossy(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("ffmpeg starts");
        assert!(status.success(), "the fixture video must be rendered");
    }

    // --- the key ------------------------------------------------------------

    #[test]
    fn the_key_follows_the_path_the_size_and_the_time() {
        let temp = scratch();
        let file = temp.path().join("clip.jpg");
        std::fs::write(&file, b"one").expect("write");
        let first = cache_key(&file).expect("a key");

        // Same file, same key: this is what makes the cache a cache.
        assert_eq!(cache_key(&file).expect("a key"), first);

        // Different size, different key.
        std::fs::write(&file, b"much longer contents").expect("write");
        let after_size = cache_key(&file).expect("a key");
        assert_ne!(after_size, first, "a changed file must not reuse the old picture");

        // Same size, different content: still a different key, because the
        // modification time moved. Without this, replacing a file with one of
        // exactly the same length would keep showing the old picture.
        let before = std::fs::metadata(&file).expect("meta").modified().expect("mtime");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&file, b"eleven bytes").expect("write");
        let after = std::fs::metadata(&file).expect("meta").modified().expect("mtime");
        if after != before {
            assert_ne!(cache_key(&file).expect("a key"), after_size);
        }
    }

    #[test]
    fn a_key_is_not_something_a_file_could_be_confused_by() {
        let temp = scratch();
        let a = temp.path().join("a.jpg");
        let b = temp.path().join("b.jpg");
        std::fs::write(&a, b"same").expect("write");
        std::fs::write(&b, b"same").expect("write");
        assert_ne!(
            cache_key(&a).expect("a key"),
            cache_key(&b).expect("a key"),
            "two files of the same size are still two files"
        );
    }

    #[test]
    fn a_file_that_is_not_there_has_no_key() {
        let temp = scratch();
        assert!(cache_key(&temp.path().join("absent.jpg")).is_none());
    }

    #[test]
    fn the_key_is_the_same_for_two_spellings_of_one_path() {
        // Windows ignores case and takes either separator, so the same file can
        // be reached two ways. A cache that missed on the second spelling would
        // regenerate a picture it already had.
        let temp = scratch();
        let file = temp.path().join("Clip.JPG");
        std::fs::write(&file, b"x").expect("write");
        #[cfg(windows)]
        {
            let shouted = PathBuf::from(file.to_string_lossy().to_uppercase());
            assert_eq!(cache_key(&file), cache_key(&shouted));
        }
        #[cfg(not(windows))]
        assert_eq!(cache_key(&file), cache_key(&file));
    }

    // --- the degraded path --------------------------------------------------

    #[test]
    fn without_ffmpeg_there_is_simply_no_thumbnail() {
        // The whole point of the degradation: a machine with no FFmpeg shows a
        // placeholder, and nothing anywhere reports an error.
        let temp = scratch();
        let source = temp.path().join("clip.jpg");
        std::fs::write(&source, b"not really a picture").expect("write");
        let thumbnailer = Thumbnailer::with_ffmpeg(temp.path().join("cache"), None);

        assert!(!thumbnailer.is_available());
        assert!(thumbnailer.ensure(&source, Source::Image).is_none());
    }

    #[test]
    fn a_source_that_cannot_be_read_is_not_an_error() {
        let temp = scratch();
        let thumbnailer = Thumbnailer::with_ffmpeg(temp.path().join("cache"), real_ffmpeg());
        let absent = temp.path().join("gone.jpg");
        assert!(thumbnailer.ensure(&absent, Source::Image).is_none());
    }

    #[test]
    fn something_with_no_picture_is_never_given_one() {
        // Audio and text are not pictures and must not be dressed as any. This
        // holds even when ffmpeg is present and could be coaxed into producing
        // *some* frame.
        let temp = scratch();
        let source = temp.path().join("track.mp3");
        std::fs::write(&source, b"\0\0\0").expect("write");
        let thumbnailer = Thumbnailer::with_ffmpeg(temp.path().join("cache"), real_ffmpeg());
        assert!(thumbnailer.ensure(&source, Source::Unsupported).is_none());
        assert!(thumbnailer.cached(&source).is_none());
    }

    #[test]
    fn a_file_that_is_not_media_leaves_no_half_written_thumbnail() {
        // ffmpeg is handed something it cannot decode. The staged file must not
        // be left behind, or a later run would take it for a finished one.
        let Some(ffmpeg) = real_ffmpeg() else {
            println!("skipped: no ffmpeg on this machine");
            return;
        };
        let temp = scratch();
        let cache = temp.path().join("cache");
        let source = temp.path().join("claimed.jpg");
        std::fs::write(&source, b"this is not a jpeg at all").expect("write");
        let thumbnailer = Thumbnailer::with_ffmpeg(cache.clone(), Some(ffmpeg));

        assert!(thumbnailer.ensure(&source, Source::Image).is_none());
        let leftovers: Vec<String> = std::fs::read_dir(&cache)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    // --- the real thing -----------------------------------------------------

    #[test]
    fn a_picture_becomes_a_thumbnail_that_keeps_its_shape() {
        let Some(ffmpeg) = real_ffmpeg() else {
            println!("skipped: no ffmpeg on this machine");
            return;
        };
        let temp = scratch();
        let source = temp.path().join("wide.png");
        write_image(&ffmpeg, &source, "1200x400");

        let thumbnailer = Thumbnailer::with_ffmpeg(temp.path().join("cache"), Some(ffmpeg));
        let made = thumbnailer
            .ensure(&source, Source::Image)
            .expect("a thumbnail");
        assert!(made.is_file());
        assert_eq!(made.extension().and_then(|e| e.to_str()), Some("jpg"));

        // Scaled down, not stretched: 1200x400 is three to one, so a 320-wide
        // thumbnail is 320x107 — and 107 is odd, which is why the filter asks
        // ffmpeg for the nearest even number rather than guessing.
        let (width, height) = jpeg_size(&made).expect("the thumbnail is a JPEG");
        assert!(width <= THUMBNAIL_WIDTH, "got {width}");
        assert!(
            (2..=4).contains(&ratio(width, height)),
            "aspect ratio was not kept: {width}x{height}"
        );
    }

    #[test]
    fn a_video_yields_one_real_frame() {
        let Some(ffmpeg) = real_ffmpeg() else {
            println!("skipped: no ffmpeg on this machine");
            return;
        };
        let temp = scratch();
        let source = temp.path().join("clip.mp4");
        write_video(&ffmpeg, &source, 4);

        let thumbnailer = Thumbnailer::with_ffmpeg(temp.path().join("cache"), Some(ffmpeg));
        let made = thumbnailer
            .ensure(&source, Source::Video)
            .expect("a thumbnail");
        let (width, height) = jpeg_size(&made).expect("the thumbnail is a JPEG");
        assert!(width > 0 && height > 0, "the frame is not empty");
        assert!(width <= THUMBNAIL_WIDTH);
    }

    #[test]
    fn a_video_too_short_for_the_seek_point_still_gets_a_picture() {
        // Half a second, seeked at one: without the retry at zero this would
        // come back with nothing and a perfectly good video would be shown as a
        // placeholder beside its neighbours.
        let Some(ffmpeg) = real_ffmpeg() else {
            println!("skipped: no ffmpeg on this machine");
            return;
        };
        let temp = scratch();
        let source = temp.path().join("blink.mp4");
        write_video(&ffmpeg, &source, 1);

        let thumbnailer = Thumbnailer::with_ffmpeg(temp.path().join("cache"), Some(ffmpeg));
        assert!(
            thumbnailer.ensure(&source, Source::Video).is_some(),
            "a short video is still a video"
        );
    }

    #[test]
    fn the_work_is_done_once_and_then_reused() {
        let Some(ffmpeg) = real_ffmpeg() else {
            println!("skipped: no ffmpeg on this machine");
            return;
        };
        let temp = scratch();
        let cache = temp.path().join("cache");
        let source = temp.path().join("picture.png");
        write_image(&ffmpeg, &source, "300x200");

        let thumbnailer = Thumbnailer::with_ffmpeg(cache.clone(), Some(ffmpeg));
        let first = thumbnailer.ensure(&source, Source::Image).expect("made");
        let stamp = std::fs::metadata(&first).expect("meta").modified().expect("mtime");

        // Asking again must hand back the same file without rewriting it.
        let second = thumbnailer.ensure(&source, Source::Image).expect("reused");
        assert_eq!(first, second);
        assert_eq!(
            std::fs::metadata(&second).expect("meta").modified().expect("mtime"),
            stamp,
            "the cached thumbnail was regenerated"
        );
        assert_eq!(std::fs::read_dir(&cache).expect("cache").count(), 1);
    }

    #[test]
    fn changing_the_source_produces_a_new_thumbnail() {
        let Some(ffmpeg) = real_ffmpeg() else {
            println!("skipped: no ffmpeg on this machine");
            return;
        };
        let temp = scratch();
        let cache = temp.path().join("cache");
        let source = temp.path().join("picture.png");
        write_image(&ffmpeg, &source, "300x200");

        let thumbnailer = Thumbnailer::with_ffmpeg(cache.clone(), Some(ffmpeg.clone()));
        let first = thumbnailer.ensure(&source, Source::Image).expect("made");

        // Replace it with something a different shape.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_image(&ffmpeg, &source, "200x300");

        let second = thumbnailer.ensure(&source, Source::Image).expect("made again");
        assert_ne!(first, second, "the old picture must not be served for the new file");
        let (width, height) = jpeg_size(&second).expect("the thumbnail is a JPEG");
        assert!(height > width, "the new thumbnail should be portrait: {width}x{height}");
    }

    #[test]
    fn the_cache_is_bounded() {
        // Filled past the bound and checked, without running ffmpeg: the bound
        // is about what is on disk, not about how it got there.
        let temp = scratch();
        let cache = temp.path().join("cache");
        std::fs::create_dir_all(&cache).expect("cache");
        for index in 0..(MAX_CACHED + 20) {
            std::fs::write(cache.join(format!("{index:016x}.jpg")), b"x").expect("write");
        }
        let thumbnailer = Thumbnailer::with_ffmpeg(cache.clone(), None);
        thumbnailer.enforce_cap();
        assert_eq!(
            std::fs::read_dir(&cache).expect("cache").count(),
            MAX_CACHED,
            "the cache is bounded"
        );
    }

    #[test]
    fn the_bound_never_touches_anything_but_thumbnails() {
        // Only names this module wrote are removed. Anything else in the
        // directory — a stray file, a directory — is left alone.
        let temp = scratch();
        let cache = temp.path().join("cache");
        std::fs::create_dir_all(&cache).expect("cache");
        std::fs::write(cache.join("notes.txt"), b"keep me").expect("write");
        std::fs::create_dir_all(cache.join("subdir")).expect("dir");
        for index in 0..(MAX_CACHED + 5) {
            std::fs::write(cache.join(format!("{index:016x}.jpg")), b"x").expect("write");
        }
        let thumbnailer = Thumbnailer::with_ffmpeg(cache.clone(), None);
        thumbnailer.enforce_cap();

        assert!(cache.join("notes.txt").is_file(), "a non-thumbnail was removed");
        assert!(cache.join("subdir").is_dir(), "a directory was removed");
    }

    #[test]
    fn a_video_that_opens_on_black_is_not_thumbnailed_as_black() {
        // The case a real download produced: two seconds of black, then the
        // picture. Seeking one second in — which is what this did at first —
        // lands squarely in the black, and a whole folder of these reads as a
        // wall of black squares that say nothing about what is in it.
        let Some(ffmpeg) = real_ffmpeg() else {
            println!("skipped: no ffmpeg on this machine");
            return;
        };
        let temp = scratch();
        let source = temp.path().join("fades-in.mp4");
        write_black_then_bright(&ffmpeg, &source);

        let thumbnailer = Thumbnailer::with_ffmpeg(temp.path().join("cache"), Some(ffmpeg.clone()));
        let made = thumbnailer.ensure(&source, Source::Video).expect("a thumbnail");

        let brightest = brightest_pixel(&ffmpeg, &made);
        assert!(
            brightest > 20,
            "the thumbnail is a black frame: brightest pixel {brightest}"
        );
    }

    #[test]
    fn the_seek_and_the_picker_do_not_read_the_whole_file() {
        // Bounded work is the requirement, and the way to see it is time: a
        // picker that decoded everything would take as long as the video is
        // long. A ten-second clip must not cost ten seconds.
        let Some(ffmpeg) = real_ffmpeg() else {
            println!("skipped: no ffmpeg on this machine");
            return;
        };
        let temp = scratch();
        let source = temp.path().join("long.mp4");
        write_video(&ffmpeg, &source, 10);

        let thumbnailer = Thumbnailer::with_ffmpeg(temp.path().join("cache"), Some(ffmpeg));
        let started = Instant::now();
        assert!(thumbnailer.ensure(&source, Source::Video).is_some());
        let took = started.elapsed();
        assert!(
            took < Duration::from_secs(5),
            "ten seconds of video took {took:?} to thumbnail"
        );
    }

    // --- helpers ------------------------------------------------------------

    /// Two seconds of black, then three seconds of a moving pattern.
    fn write_black_then_bright(ffmpeg: &Path, at: &Path) {
        let status = std::process::Command::new(ffmpeg)
            .args([
                "-v", "error", "-y",
                "-f", "lavfi", "-i", "color=black:size=160x120:rate=10:duration=2",
                "-f", "lavfi", "-i", "testsrc=size=160x120:rate=10:duration=3",
                "-filter_complex", "[0:v][1:v]concat=n=2:v=1[v]",
                "-map", "[v]", "-c:v", "libx264", "-pix_fmt", "yuv420p",
                &at.to_string_lossy(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("ffmpeg starts");
        assert!(status.success(), "the black-opening fixture must be rendered");
    }

    /// The brightest pixel in a picture, read back through ffmpeg.
    ///
    /// Asking ffmpeg to describe the file it just wrote is the only way to check
    /// what is actually in it without decoding JPEG by hand.
    fn brightest_pixel(ffmpeg: &Path, picture: &Path) -> u8 {
        let output = std::process::Command::new(ffmpeg)
            .args([
                "-v", "error", "-i",
                &picture.to_string_lossy(),
                "-vf", "scale=32:-2,format=gray",
                "-f", "rawvideo", "-",
            ])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .expect("ffmpeg starts");
        output.stdout.iter().copied().max().unwrap_or(0)
    }

    /// The width and height in a JPEG's start-of-frame marker.
    ///
    /// Read from the file rather than asked of ffmpeg: this is checking what was
    /// actually written, and asking the tool that wrote it would be asking it to
    /// mark its own work.
    fn jpeg_size(path: &Path) -> Option<(u32, u32)> {
        let bytes = std::fs::read(path).ok()?;
        let mut index = 2; // past the SOI marker
        while index + 9 < bytes.len() {
            if bytes[index] != 0xFF {
                index += 1;
                continue;
            }
            let marker = bytes[index + 1];
            let length = usize::from(u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]));
            // Any of the start-of-frame markers; there are several for the
            // different sampling modes and any of them carries the size.
            if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
                let height = u32::from(u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]));
                let width = u32::from(u16::from_be_bytes([bytes[index + 7], bytes[index + 8]]));
                return Some((width, height));
            }
            index += 2 + length;
        }
        None
    }

    /// Width divided by height, as a whole number, for a tolerant comparison.
    fn ratio(width: u32, height: u32) -> u32 {
        if height == 0 {
            return 0;
        }
        width / height
    }
}
