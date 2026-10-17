//! Media processing that needs a tool the application does not implement itself.
//!
//! Douyin serves a video and its audio as two separate renditions. Producing one
//! playable file therefore means combining them, and combining media correctly —
//! containers, codec parameters, timestamps — is exactly the kind of job that
//! should not be hand-rolled. FFmpeg is used for it, which is the one thing the
//! project brief sanctions it for.
//!
//! This module is the seam: [`MediaMuxer`] is what the download layer asks for,
//! and [`ffmpeg::FfmpegMuxer`] is the only implementation. Nothing above knows a
//! process is started, and nothing below knows what a download is.
//!
//! # Copy, never re-encode
//!
//! Muxing copies the streams (`-c copy`). The tracks the source provided are the
//! tracks the file contains — the same bytes, the same quality. Nothing here
//! transcodes, and nothing here alters the media itself.

pub mod ffmpeg;

pub use ffmpeg::FfmpegMuxer;

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;

use crate::errors::{AppError, AppResult};
use crate::network::CancellationHandle;

/// Default budget for one mux.
///
/// Copying two tracks is I/O-bound and finishes in well under this; the ceiling
/// exists so a wedged process cannot hold a download open forever.
pub const DEFAULT_MUX_TIMEOUT: Duration = Duration::from_secs(180);

/// What a finished media file actually contains.
///
/// A task must never be marked complete because a file exists. This is the
/// evidence that says the file is a readable container holding the streams that
/// were asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaProbe {
    pub has_video: bool,
    pub has_audio: bool,
    /// Whole seconds, when the container reports a duration.
    pub duration_seconds: Option<u64>,
    pub size_bytes: u64,
}

impl MediaProbe {
    /// Whether the file has everything the caller required.
    pub fn satisfies(&self, want_audio: bool) -> bool {
        self.has_video && (!want_audio || self.has_audio) && self.size_bytes > 0
    }

    /// Whether the file is a media file at all.
    ///
    /// The check for a download that had nothing to combine. Such a file is
    /// whatever the source served — a video track, an image, or a sound file —
    /// so requiring a *video* stream would refuse a legitimate audio download.
    /// What must be true is that the container holds something playable, which
    /// is exactly what a probe that finds no stream at all disproves.
    pub fn satisfies_any_media(&self) -> bool {
        (self.has_video || self.has_audio) && self.size_bytes > 0
    }

    /// A short description for logs and error detail.
    pub fn describe(&self) -> String {
        format!(
            "video={} audio={} duration={:?}s bytes={}",
            self.has_video, self.has_audio, self.duration_seconds, self.size_bytes
        )
    }
}

/// One mux request.
pub struct MuxRequest<'a> {
    /// The video track, already downloaded to a temporary file.
    pub video: &'a Path,
    /// The audio track, already downloaded to a temporary file.
    pub audio: &'a Path,
    /// Where the combined file is written. Always a temporary path: the storage
    /// layer decides the final name.
    pub output: &'a Path,
    /// Polled while the tool runs, so cancelling a download cancels the mux.
    pub cancel: CancellationHandle,
    pub timeout: Duration,
}

impl<'a> MuxRequest<'a> {
    pub fn new(video: &'a Path, audio: &'a Path, output: &'a Path) -> Self {
        Self {
            video,
            audio,
            output,
            cancel: CancellationHandle::new(),
            timeout: DEFAULT_MUX_TIMEOUT,
        }
    }
}

/// Combines separate media tracks and inspects the result.
#[async_trait]
pub trait MediaMuxer: Send + Sync {
    /// Short identifier used in diagnostics and logs.
    fn name(&self) -> &'static str;

    /// Whether the tool this muxer needs is installed.
    ///
    /// Checked before a download that needs one is started, so a missing tool is
    /// reported as such rather than discovered halfway through.
    fn is_available(&self) -> bool;

    /// Combine `request.video` and `request.audio` into `request.output`.
    async fn mux(&self, request: &MuxRequest<'_>) -> AppResult<()>;

    /// Inspect a finished file.
    async fn probe(&self, path: &Path) -> AppResult<MediaProbe>;
}

/// A [`MediaMuxer`] that is never available.
///
/// Used when the setting is off, when no tool was found, and as the default in
/// tests, so no caller has to branch on an `Option`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopMuxer;

#[async_trait]
impl MediaMuxer for NoopMuxer {
    fn name(&self) -> &'static str {
        "none"
    }

    fn is_available(&self) -> bool {
        false
    }

    async fn mux(&self, _request: &MuxRequest<'_>) -> AppResult<()> {
        Err(AppError::muxer_unavailable(
            "no media muxer is configured",
        ))
    }

    async fn probe(&self, _path: &Path) -> AppResult<MediaProbe> {
        Err(AppError::muxer_unavailable("no media muxer is configured"))
    }
}

/// The size of a file, or zero when it cannot be read.
pub fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// A failure a [`MockMuxer`] should produce.
///
/// Stored in pieces rather than as an [`AppError`] because an error is not
/// cloneable and a mock may be asked to fail more than once.
#[derive(Debug, Clone)]
pub struct MockFailure {
    pub kind: crate::errors::AppErrorKind,
    pub message: String,
    pub retryable: bool,
}

impl MockFailure {
    pub fn new(kind: crate::errors::AppErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable: false,
        }
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    fn build(&self) -> AppError {
        AppError::new(self.kind, self.message.clone()).with_retryable(self.retryable)
    }
}

/// Deterministic test double for [`MediaMuxer`].
///
/// Available outside tests so the download layer's two-track path can be driven
/// without FFmpeg being installed — which is what keeps those tests runnable on
/// any machine rather than only on one that happens to have the tool.
pub struct MockMuxer {
    available: bool,
    /// What `probe` reports. `None` means a plausible complete file.
    probe: std::sync::Mutex<Option<MediaProbe>>,
    /// A failure `mux` should return, if any.
    failure: std::sync::Mutex<Option<MockFailure>>,
    /// Whether `mux` writes an output file at all.
    writes_output: bool,
    mux_calls: std::sync::atomic::AtomicUsize,
    probe_calls: std::sync::atomic::AtomicUsize,
    /// The output path of the most recent mux, so a test can assert what it was
    /// handed. The muxer will replace whatever path it is given, and that is
    /// only safe while the caller hands it a file of its own.
    last_output: std::sync::Mutex<Option<std::path::PathBuf>>,
}

impl MockMuxer {
    /// A muxer that produces a file and reports the given layout.
    pub fn producing(has_video: bool, has_audio: bool) -> Self {
        Self {
            available: true,
            probe: std::sync::Mutex::new(Some(MediaProbe {
                has_video,
                has_audio,
                duration_seconds: Some(72),
                size_bytes: 0,
            })),
            failure: std::sync::Mutex::new(None),
            writes_output: true,
            mux_calls: std::sync::atomic::AtomicUsize::new(0),
            probe_calls: std::sync::atomic::AtomicUsize::new(0),
            last_output: std::sync::Mutex::new(None),
        }
    }

    /// A muxer that reports itself unavailable, as if FFmpeg were not installed.
    pub fn unavailable() -> Self {
        Self {
            available: false,
            ..Self::producing(true, true)
        }
    }

    /// A muxer whose `mux` fails.
    pub fn failing(failure: MockFailure) -> Self {
        let muxer = Self::producing(true, true);
        *muxer.failure.lock().expect("lock") = Some(failure);
        muxer
    }

    /// A muxer that reports success but writes no file.
    pub fn writing_nothing() -> Self {
        Self {
            writes_output: false,
            ..Self::producing(true, true)
        }
    }

    /// How many times `mux` was called.
    pub fn mux_calls(&self) -> usize {
        self.mux_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The output path the most recent mux was given, if one ran.
    pub fn last_output(&self) -> Option<std::path::PathBuf> {
        self.last_output.lock().expect("lock").clone()
    }

    /// How many times `probe` was called.
    pub fn probe_calls(&self) -> usize {
        self.probe_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl MediaMuxer for MockMuxer {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn is_available(&self) -> bool {
        self.available
    }

    async fn mux(&self, request: &MuxRequest<'_>) -> AppResult<()> {
        self.mux_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.last_output.lock().expect("lock") = Some(request.output.to_path_buf());

        if let Some(failure) = self.failure.lock().expect("lock").clone() {
            return Err(failure.build());
        }
        if self.writes_output {
            tokio::fs::write(request.output, b"mock-combined")
                .await
                .map_err(|err| {
                    AppError::download("the mock muxer could not write its output")
                        .with_technical_detail(err.to_string())
                })?;
        }
        Ok(())
    }

    async fn probe(&self, path: &Path) -> AppResult<MediaProbe> {
        self.probe_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let mut probe = self
            .probe
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or(MediaProbe {
                has_video: true,
                has_audio: true,
                duration_seconds: Some(72),
                size_bytes: 0,
            });
        probe.size_bytes = file_size(path);
        Ok(probe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_satisfies_only_when_it_has_what_was_asked_for() {
        let both = MediaProbe {
            has_video: true,
            has_audio: true,
            duration_seconds: Some(72),
            size_bytes: 5_911_418,
        };
        assert!(both.satisfies(true));
        assert!(both.satisfies(false));

        let video_only = MediaProbe {
            has_audio: false,
            ..both.clone()
        };
        assert!(video_only.satisfies(false));
        assert!(!video_only.satisfies(true), "audio was asked for");

        let empty = MediaProbe {
            size_bytes: 0,
            ..both
        };
        assert!(!empty.satisfies(false), "an empty file is not a result");
    }

    #[tokio::test]
    async fn the_noop_muxer_is_never_available() {
        let muxer = NoopMuxer;
        assert!(!muxer.is_available());
        let request = MuxRequest::new(
            Path::new("v.mp4"),
            Path::new("a.mp4"),
            Path::new("out.mp4"),
        );
        let err = muxer.mux(&request).await.unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::MuxerUnavailable);
    }
}
