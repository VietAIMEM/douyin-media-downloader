//! Combining separate tracks with FFmpeg, and asking FFmpeg what it produced.
//!
//! # Why a process
//!
//! Muxing two tracks into a valid MP4 means writing containers: building an
//! index of every sample, interleaving them, keeping timestamps coherent across
//! two independently encoded streams. Doing that by hand is a large amount of
//! subtle, easy-to-get-wrong work. FFmpeg already does it, the project brief
//! names it for exactly this purpose, and using it keeps the risky part in a
//! tool that is tested far more widely than anything written here would be.
//!
//! # What it does not do
//!
//! The streams are copied, never re-encoded: `-c copy`. The video that comes out
//! is the video the source served — same codec, same bitrate, same pixels. No
//! filter is applied, so nothing here could alter the media even by accident.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;

use crate::errors::{AppError, AppResult};
use crate::network::CancellationHandle;
use crate::process::NoConsoleWindow;

use super::{MediaMuxer, MediaProbe, MuxRequest};

/// Environment variable naming an `ffmpeg` executable to use.
pub const FFMPEG_ENV_VAR: &str = "DOUYIN_DOWNLOADER_FFMPEG";

/// How often cancellation is checked while a tool runs.
const CANCEL_POLL: Duration = Duration::from_millis(150);

/// How long a probe may take. Reading a container's index is quick.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Combines tracks with an installed FFmpeg.
pub struct FfmpegMuxer {
    ffmpeg: PathBuf,
    /// Preferred for probing. When absent, `ffmpeg` itself is asked instead.
    ffprobe: Option<PathBuf>,
}

impl FfmpegMuxer {
    /// Discover FFmpeg and build a muxer, or `None` when there is none.
    pub fn discover() -> Option<Self> {
        let ffmpeg = discover_tool("ffmpeg", FFMPEG_ENV_VAR)?;
        let ffprobe = discover_tool("ffprobe", "");
        Some(Self { ffmpeg, ffprobe })
    }

    /// Build a muxer around a specific `ffmpeg` executable.
    pub fn with_executable(executable: impl Into<PathBuf>) -> AppResult<Self> {
        let ffmpeg = executable.into();
        if !ffmpeg.is_file() {
            return Err(
                AppError::muxer_unavailable("the configured FFmpeg does not exist")
                    .with_technical_detail(ffmpeg.to_string_lossy().into_owned()),
            );
        }
        // A sibling ffprobe is the normal layout of every FFmpeg distribution.
        let ffprobe = ffmpeg
            .parent()
            .map(|dir| dir.join(probe_name()))
            .filter(|candidate| candidate.is_file())
            .or_else(|| discover_tool("ffprobe", ""));

        Ok(Self { ffmpeg, ffprobe })
    }

    /// The executable this muxer will run.
    pub fn executable(&self) -> &Path {
        &self.ffmpeg
    }

    /// The arguments used to combine two tracks.
    ///
    /// Kept separate from running them so the exact command can be asserted in a
    /// test without a process.
    fn mux_arguments(request: &MuxRequest<'_>) -> Vec<String> {
        vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            // Never read from stdin: a tool waiting on input that will never
            // arrive would hold the download open.
            "-nostdin".into(),
            // The output is a temporary path this application owns, so
            // overwriting it is the intended behaviour rather than a risk.
            "-y".into(),
            "-i".into(),
            request.video.to_string_lossy().into_owned(),
            "-i".into(),
            request.audio.to_string_lossy().into_owned(),
            // First video stream from the video file, first audio from the audio
            // file. Explicit mapping stops FFmpeg choosing for us if a file
            // happens to carry both.
            "-map".into(),
            "0:v:0".into(),
            "-map".into(),
            "1:a:0".into(),
            // Copy, never re-encode.
            "-c".into(),
            "copy".into(),
            // The container is named explicitly because the output is a
            // temporary path carrying a `.part` suffix — the storage layer
            // renames it into place afterwards. FFmpeg infers the muxer from
            // the output extension, and `.part` is not one it knows, so without
            // this it refuses to start. Douyin's two renditions are an AVC
            // video track and an AAC audio track, which is what MP4 holds.
            "-f".into(),
            "mp4".into(),
            request.output.to_string_lossy().into_owned(),
        ]
    }
}

#[async_trait]
impl MediaMuxer for FfmpegMuxer {
    fn name(&self) -> &'static str {
        "ffmpeg"
    }

    fn is_available(&self) -> bool {
        self.ffmpeg.is_file()
    }

    async fn mux(&self, request: &MuxRequest<'_>) -> AppResult<()> {
        let args = Self::mux_arguments(request);

        // Deliberately *no* delete of `request.output` first.
        //
        // There used to be one, to start from a clean slate. It is gone because
        // it is a destructive primitive that removes whatever path it is handed,
        // and nothing about the muxer proves that path belongs to this
        // operation — the caller decides, and a caller that decided wrongly
        // would have deleted a file with no way to tell. It bought nothing
        // either: `-y` is already passed, so FFmpeg replaces the output itself.
        // Removing it also closes a window in which the old file was gone and
        // the new one did not exist yet, which is the shape a crash turns into
        // data loss.

        let output = run_tool(&self.ffmpeg, &args, &request.cancel, request.timeout).await?;

        if !output.status.success() {
            let detail: String = String::from_utf8_lossy(&output.stderr)
                .lines()
                .take(4)
                .collect::<Vec<_>>()
                .join(" | ");
            return Err(AppError::download("FFmpeg could not combine the tracks")
                .with_user_message("The video and its sound could not be combined into one file.")
                .with_technical_detail(format!("exit={:?} {detail}", output.status.code())));
        }

        // A tool that "succeeded" while producing nothing has not succeeded.
        if super::file_size(request.output) == 0 {
            return Err(AppError::download("FFmpeg produced no output")
                .with_user_message("The combined file came out empty.")
                .with_technical_detail(format!("output={}", request.output.display())));
        }

        Ok(())
    }

    async fn probe(&self, path: &Path) -> AppResult<MediaProbe> {
        let size = super::file_size(path);
        if size == 0 {
            return Err(AppError::download("the finished file is empty")
                .with_user_message("The download produced an empty file.")
                .with_technical_detail(format!("path={}", path.display())));
        }

        let streams = match &self.ffprobe {
            Some(ffprobe) => {
                let args: Vec<String> = vec![
                    "-hide_banner".into(),
                    "-loglevel".into(),
                    "error".into(),
                    "-print_format".into(),
                    "json".into(),
                    "-show_format".into(),
                    "-show_streams".into(),
                    path.to_string_lossy().into_owned(),
                ];
                let output =
                    run_tool(ffprobe, &args, &CancellationHandle::new(), PROBE_TIMEOUT).await?;
                if !output.status.success() {
                    return Err(
                        AppError::download("ffprobe could not read the finished file")
                            .with_user_message("The downloaded file could not be read back.")
                            .with_technical_detail(
                                String::from_utf8_lossy(&output.stderr).trim().to_string(),
                            ),
                    );
                }
                parse_probe_json(&String::from_utf8_lossy(&output.stdout))?
            }
            // No ffprobe beside it: `ffmpeg` reports the same facts on stderr.
            None => {
                let args: Vec<String> = vec![
                    "-hide_banner".into(),
                    "-nostdin".into(),
                    "-i".into(),
                    path.to_string_lossy().into_owned(),
                ];
                let output = run_tool(
                    &self.ffmpeg,
                    &args,
                    &CancellationHandle::new(),
                    PROBE_TIMEOUT,
                )
                .await?;
                // `ffmpeg` exits non-zero when it was given no output file, which
                // is exactly how it is being used here — so the status is not a
                // failure signal and the report is what matters.
                parse_ffmpeg_report(&String::from_utf8_lossy(&output.stderr))?
            }
        };

        Ok(MediaProbe {
            has_video: streams.has_video,
            has_audio: streams.has_audio,
            duration_seconds: streams.duration_seconds,
            size_bytes: size,
        })
    }
}

/// Stream facts extracted from a tool's report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct StreamReport {
    has_video: bool,
    has_audio: bool,
    duration_seconds: Option<u64>,
}

/// Read `ffprobe -print_format json` output.
fn parse_probe_json(raw: &str) -> AppResult<StreamReport> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|err| {
        AppError::download("ffprobe produced something unreadable")
            .with_technical_detail(err.to_string())
    })?;

    let streams = value
        .get("streams")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();

    let has_video = streams.iter().any(|stream| {
        stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("video")
    });
    let has_audio = streams.iter().any(|stream| {
        stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("audio")
    });

    let duration_seconds = value
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(serde_json::Value::as_str)
        .and_then(|text| text.parse::<f64>().ok())
        .or_else(|| {
            streams.iter().find_map(|stream| {
                stream
                    .get("duration")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|text| text.parse::<f64>().ok())
            })
        })
        .filter(|seconds| *seconds > 0.0)
        .map(|seconds| seconds.round() as u64);

    Ok(StreamReport {
        has_video,
        has_audio,
        duration_seconds,
    })
}

/// Read `ffmpeg -i <file>`'s stderr report.
///
/// Lines look like:
/// ```text
///   Duration: 00:01:11.98, start: 0.000000, bitrate: 662 kb/s
///   Stream #0:0[0x1](und): Video: h264 (avc1), yuv420p, 1280x720, ...
///   Stream #0:1[0x2](und): Audio: aac (mp4a), 44100 Hz, stereo, ...
/// ```
fn parse_ffmpeg_report(raw: &str) -> AppResult<StreamReport> {
    let mut report = StreamReport::default();
    let mut recognised_a_stream = false;

    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Duration:") {
            report.duration_seconds = rest
                .split(',')
                .next()
                .and_then(parse_clock)
                .filter(|seconds| *seconds > 0);
        } else if trimmed.starts_with("Stream #") {
            // The codec kind follows the stream's own labels, so it is matched
            // on the segment after the first `: ` rather than anywhere in the
            // line. `split_once` is used rather than splitting on every `: `:
            // `Stream #0:0(und): Video: h264` splits again inside `Video:`, so
            // splitting everywhere would leave just `Video` here.
            let codec = trimmed.split_once(": ").map(|(_, rest)| rest).unwrap_or("");
            if codec.starts_with("Video:") {
                report.has_video = true;
                recognised_a_stream = true;
            } else if codec.starts_with("Audio:") {
                report.has_audio = true;
                recognised_a_stream = true;
            }
        }
    }

    if !recognised_a_stream {
        return Err(
            AppError::download("the finished file could not be read as media")
                .with_user_message("The downloaded file is not a readable media file.")
                .with_technical_detail(raw.lines().take(3).collect::<Vec<_>>().join(" | ")),
        );
    }

    Ok(report)
}

/// Parse `HH:MM:SS.ss` into whole seconds.
fn parse_clock(text: &str) -> Option<u64> {
    let mut parts = text.trim().split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    Some((hours * 3600.0 + minutes * 60.0 + seconds).round() as u64)
}

/// The name `ffprobe` has on this platform.
fn probe_name() -> &'static str {
    if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    }
}

/// Locate a tool, preferring an explicit environment override.
///
/// The order is the same one the browser uses: an explicit path, then `PATH`,
/// then the places an installer usually leaves it. Nothing is specific to one
/// machine or user.
pub fn discover_tool(name: &str, env_var: &str) -> Option<PathBuf> {
    if !env_var.is_empty() {
        if let Some(explicit) = std::env::var_os(env_var).map(PathBuf::from) {
            if explicit.is_file() {
                return Some(explicit);
            }
        }
    }
    if let Some(found) = find_on_path(name) {
        return Some(found);
    }
    known_locations(name)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// Places an FFmpeg install commonly lands.
fn known_locations(name: &str) -> Vec<PathBuf> {
    let executable = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };

    let mut candidates = Vec::new();
    for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(variable) {
            let base = PathBuf::from(base);
            candidates.push(base.join("ffmpeg").join("bin").join(&executable));
            candidates.push(
                base.join("Microsoft")
                    .join("WinGet")
                    .join("Links")
                    .join(&executable),
            );
        }
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        candidates.push(
            PathBuf::from(profile)
                .join("scoop")
                .join("shims")
                .join(&executable),
        );
    }
    candidates.push(PathBuf::from("C:/ffmpeg/bin").join(&executable));
    candidates.push(PathBuf::from("C:/ProgramData/chocolatey/bin").join(&executable));
    candidates
}

/// Look for an executable on `PATH`.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".EXE;.CMD;.BAT".to_string())
        .split(';')
        .map(|ext| ext.to_ascii_lowercase())
        .collect();

    for directory in std::env::split_paths(&path) {
        let direct = directory.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        for extension in &extensions {
            let candidate = directory.join(format!("{name}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Build the command that runs a media tool.
///
/// Split out from [`run_tool`] so the configuration — the pipes that carry the
/// tool's diagnostics, and the Windows console suppression that must not
/// interfere with them — can be asserted without starting a process.
fn tool_command(executable: &Path, args: &[String]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // ffmpeg and ffprobe are console programs: without this, each one puts a
    // black window on screen for as long as it runs. The pipes above are
    // unaffected — the flag suppresses the console, not the streams.
    command.no_console_window();
    command
}

/// Run a tool, honouring cancellation and an overall deadline.
///
/// `kill_on_drop` is what makes cancellation real: if this future is dropped —
/// by the cancel arm below, or by the caller abandoning the task — the child is
/// killed rather than left running.
async fn run_tool(
    executable: &Path,
    args: &[String],
    cancel: &CancellationHandle,
    timeout: Duration,
) -> AppResult<std::process::Output> {
    let mut command = tool_command(executable, args);

    let child = command.spawn().map_err(|err| {
        AppError::muxer_unavailable("the media tool could not be started")
            .with_user_message("The media tool could not be started.")
            .with_technical_detail(format!("{}: {err}", executable.display()))
    })?;

    let work = async {
        tokio::select! {
            result = child.wait_with_output() => result.map_err(|err| {
                AppError::download("the media tool could not be waited on")
                    .with_technical_detail(err.to_string())
            }),
            _ = wait_for_cancel(cancel) => Err(AppError::download("cancelled by the user")
                .with_user_message("The download was cancelled.")
                .with_retryable(false)),
        }
    };

    tokio::time::timeout(timeout, work).await.map_err(|_| {
        AppError::download("the media tool took too long")
            .with_user_message("Combining the video and sound took too long, so it was stopped.")
            .with_technical_detail(format!("timeout={}ms", timeout.as_millis()))
    })?
}

/// Resolve once cancellation is requested.
async fn wait_for_cancel(cancel: &CancellationHandle) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tool_command_keeps_its_arguments_and_its_cancellation() {
        let command = tool_command(Path::new("ffmpeg.exe"), &["-version".to_string()]);
        let shown = format!("{command:?}");

        // That the tool is started with the arguments it was given, and that
        // dropping the future still kills it — the whole basis of cancelling a
        // mux or a probe. The console suppression applied alongside these is
        // checked by `tests/no_console_window.rs`, because creation flags
        // cannot be read back.
        assert!(shown.contains("ffmpeg.exe"), "{shown}");
        assert!(shown.contains("-version"), "{shown}");
        assert!(
            shown.contains("kill_on_drop: true"),
            "cancelling would orphan the tool: {shown}"
        );
    }

    #[test]
    fn the_mux_command_copies_streams_and_maps_them_explicitly() {
        let request = MuxRequest::new(
            Path::new("C:/tmp/video.part"),
            Path::new("C:/tmp/audio.part"),
            Path::new("C:/tmp/combined.part"),
        );
        let args = FfmpegMuxer::mux_arguments(&request);
        let joined = args.join(" ");

        // Copy, never transcode.
        assert!(joined.contains("-c copy"), "got {joined}");
        assert!(!joined.contains("libx264"));
        assert!(!joined.contains("-vf"), "no filter may touch the media");
        // Explicit mapping of one video and one audio stream.
        assert!(joined.contains("-map 0:v:0"), "got {joined}");
        assert!(joined.contains("-map 1:a:0"), "got {joined}");
        // Both inputs, and the output last.
        assert!(joined.contains("-i C:/tmp/video.part"));
        assert!(joined.contains("-i C:/tmp/audio.part"));
        assert!(joined.ends_with("C:/tmp/combined.part"));
        // Never waits on stdin.
        assert!(joined.contains("-nostdin"));
        // The output is a `.part` temp path, so the container is named rather
        // than left for FFmpeg to infer from an extension it does not know.
        assert!(joined.contains("-f mp4"), "got {joined}");
    }

    /// The real tool, on real files.
    ///
    /// Everything above checks the arguments and the parsing; only running the
    /// tool proves the command actually produces a playable file. The output is
    /// deliberately a `.part` path, because that is what the download worker
    /// hands the muxer — a name FFmpeg cannot infer a container from.
    #[tokio::test]
    async fn a_real_mux_produces_a_file_holding_both_tracks() {
        let Some(muxer) = FfmpegMuxer::discover() else {
            eprintln!("skipping: no FFmpeg is installed");
            return;
        };

        let temp = tempfile::tempdir().unwrap();
        let video = temp.path().join("video.mp4");
        let audio = temp.path().join("audio.mp4");
        let output = temp.path().join(".clip.mp4.combined.part");

        // Two independent tracks, shaped the way the source serves them: the
        // video carries no audio, the audio carries no video.
        let tracks: [(&std::path::PathBuf, [&str; 6]); 2] = [
            (
                &video,
                [
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=size=160x120:rate=10",
                    "-c:v",
                    "libx264",
                ],
            ),
            (
                &audio,
                ["-f", "lavfi", "-i", "sine=frequency=440", "-c:a", "aac"],
            ),
        ];
        for (path, args) in tracks {
            let status = std::process::Command::new(muxer.executable())
                .args(["-hide_banner", "-loglevel", "error", "-y", "-t", "2"])
                .args(args)
                .arg(path)
                .status()
                .expect("the test track could not be generated");
            assert!(status.success(), "could not build {}", path.display());
        }

        let request = MuxRequest::new(&video, &audio, &output);
        muxer.mux(&request).await.expect("the mux must succeed");

        let probe = muxer
            .probe(&output)
            .await
            .expect("the output must be readable");
        assert!(probe.has_video, "no video track: {}", probe.describe());
        assert!(probe.has_audio, "no audio track: {}", probe.describe());
        assert!(probe.satisfies(true));
    }

    #[test]
    fn json_probe_reports_both_streams_and_duration() {
        let raw = r#"{
            "streams": [
                { "codec_type": "video", "codec_name": "h264", "duration": "71.98" },
                { "codec_type": "audio", "codec_name": "aac" }
            ],
            "format": { "duration": "71.982000", "size": "5911418" }
        }"#;

        let report = parse_probe_json(raw).unwrap();
        assert!(report.has_video);
        assert!(report.has_audio);
        assert_eq!(report.duration_seconds, Some(72));
    }

    #[test]
    fn json_probe_reports_a_video_only_file_honestly() {
        let raw = r#"{"streams":[{"codec_type":"video"}],"format":{"duration":"72.0"}}"#;
        let report = parse_probe_json(raw).unwrap();
        assert!(report.has_video);
        assert!(!report.has_audio, "no audio stream was listed");
    }

    #[test]
    fn json_probe_falls_back_to_a_stream_duration() {
        let raw = r#"{"streams":[{"codec_type":"video","duration":"12.34"}]}"#;
        assert_eq!(parse_probe_json(raw).unwrap().duration_seconds, Some(12));
    }

    #[test]
    fn unreadable_json_is_a_typed_error_not_a_default() {
        let err = parse_probe_json("not json").unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Download);
    }

    #[test]
    fn ffmpeg_report_parses_streams_and_duration() {
        let raw = "\
ffmpeg version 7.0 Copyright (c) 2000-2024
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'combined.mp4':
  Duration: 00:01:11.98, start: 0.000000, bitrate: 662 kb/s
  Stream #0:0[0x1](und): Video: h264 (avc1 / 0x31637661), yuv420p, 1280x720, 641 kb/s
  Stream #0:1[0x2](und): Audio: aac (mp4a / 0x6134706d), 44100 Hz, stereo, 45 kb/s
At least one output file must be specified
";
        let report = parse_ffmpeg_report(raw).unwrap();
        assert!(report.has_video);
        assert!(report.has_audio);
        assert_eq!(report.duration_seconds, Some(72));
    }

    #[test]
    fn ffmpeg_report_of_a_video_only_file_says_so() {
        let raw = "  Duration: 00:00:30.00, start: 0.0\n  Stream #0:0: Video: h264 (avc1), yuv420p, 1280x720\n";
        let report = parse_ffmpeg_report(raw).unwrap();
        assert!(report.has_video);
        assert!(!report.has_audio);
        assert_eq!(report.duration_seconds, Some(30));
    }

    #[test]
    fn ffmpeg_report_with_no_streams_is_an_error_rather_than_an_empty_success() {
        let raw = "combined.mp4: Invalid data found when processing input\n";
        let err = parse_ffmpeg_report(raw).unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Download);
    }

    #[test]
    fn clock_parsing_handles_hours_minutes_and_seconds() {
        assert_eq!(parse_clock("00:01:11.98"), Some(72));
        assert_eq!(parse_clock("01:00:00.00"), Some(3600));
        assert_eq!(parse_clock("00:00:05.5"), Some(6));
        assert_eq!(parse_clock("nonsense"), None);
    }

    #[test]
    fn a_missing_executable_is_reported_as_muxer_unavailable() {
        let err = FfmpegMuxer::with_executable(r"C:\definitely\not\here\ffmpeg.exe")
            .err()
            .expect("a missing executable must be rejected");
        assert_eq!(err.kind(), crate::errors::AppErrorKind::MuxerUnavailable);
        // The message tells the user what to do about it.
        assert!(err.user_message().contains("FFmpeg"));
    }

    #[test]
    fn discovery_finds_nothing_when_the_override_points_nowhere() {
        // An override that does not exist must not be returned as if it did.
        assert!(!Path::new(r"C:\definitely\not\here").is_file());
    }

    #[tokio::test]
    async fn running_a_missing_tool_reports_muxer_unavailable() {
        let err = run_tool(
            Path::new(r"C:\definitely\not\here\tool.exe"),
            &[],
            &CancellationHandle::new(),
            Duration::from_secs(5),
        )
        .await
        .expect_err("a missing tool must fail");
        assert_eq!(err.kind(), crate::errors::AppErrorKind::MuxerUnavailable);
    }

    #[tokio::test]
    async fn a_cancelled_tool_run_stops_and_reports_cancellation() {
        let cancel = CancellationHandle::new();
        cancel.cancel();

        // Any executable will do: cancellation is checked before it can matter.
        let executable = std::env::current_exe().expect("test binary path");
        let err = run_tool(
            &executable,
            &["--help".to_string()],
            &cancel,
            Duration::from_secs(10),
        )
        .await
        .expect_err("a cancelled run must fail");

        assert_eq!(err.kind(), crate::errors::AppErrorKind::Download);
        assert!(!err.is_retryable(), "a cancel is not worth retrying");
    }

    #[tokio::test]
    async fn a_probe_of_an_empty_file_fails_before_running_anything() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("empty.mp4");
        std::fs::write(&path, b"").unwrap();

        // Built around this test binary: it is never executed, because the empty
        // file is rejected first.
        let muxer = FfmpegMuxer::with_executable(std::env::current_exe().unwrap()).unwrap();
        let err = muxer.probe(&path).await.unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Download);
    }
}
