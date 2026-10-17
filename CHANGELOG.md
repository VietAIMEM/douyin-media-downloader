# Changelog

All notable changes to this project are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> **A note on completeness.** This repository publishes a single squashed history,
> so the change-by-change log between the pre-publication builds is not recorded
> here. The entries below describe the state each version shipped, taken from the
> version metadata, the published releases and the project's own documentation —
> not from a diff against an unavailable earlier tree. Where a detail could not be
> verified, it is left out rather than guessed. The detailed notes for the 1.0.0
> build are retained in [`code/docs/RELEASE_NOTES.md`](code/docs/RELEASE_NOTES.md).

## [1.0.1] — 2026-10-17

The first public release. Published on GitHub with a per-user NSIS installer for
Windows x64.

### Added

**Workflows**

- **Single URL Downloader** — paste a Douyin post or share link, analyse it,
  preview the media it exposes, and choose what to download.
- **Profile Downloader** — load a creator's public profile one page of posts at a
  time, filter by media type, select across pages, and submit the selection to a
  single queue.

**Downloading**

- A centralised queue with a configurable concurrency limit, per-task progress
  with measured speed and estimated time remaining, and pause, resume, cancel,
  retry and queue recovery after a restart.
- Retries are bounded, and are only attempted where retrying could help.
- A download is reported complete only after the file has been read back and the
  task and its history row written in one transaction.
- When Douyin serves a post's audio as a separate track, FFmpeg stream-copies the
  two together into one playable file. The copy does not transcode and does not
  alter the media.
- Files are written to a configurable download folder with configurable name and
  folder templates.

**Library, history and downloads folder**

- **Library** — search, filter by media type and state, browse discovered media
  with thumbnails, and export metadata to JSON or CSV.
- **History** — every outcome, with time, size and destination.
- **Download Folder** — browse the configured download directory directly from
  disk, including files placed there outside the application.

**Interface**

- Grouped navigation (Download, Library, App) with light and dark themes and a
  consistent set of controls.
- Error messages that state what went wrong and what to try next.
- The downloader inputs start empty. Developer fixture URLs are no longer
  prefilled into the Single URL and Profile Downloader fields.

### Known limitations

- **The installers are not code-signed.** Windows will not show a publisher name
  and SmartScreen may warn on first run.
- **The Profile Downloader performs a login-status check when the page is
  opened.** If a supported browser is installed, that check starts the
  application's own browser profile and loads Douyin's home page to ask whether
  it recognises a signed-in session. It is not gated by the saved-login setting,
  which controls only whether *profile loads* use the session.
- **Availability is not guaranteed.** Some posts and profiles may be unavailable,
  region-restricted, require an existing session, or change their response format.
- The application does not bypass access controls, solve challenges, or retrieve
  private content, and it does not remove watermarks.

## [1.0.0] — 2026-10-12

An internal personal-use build. **Not published and not distributed** — no
release, no installer uploaded, no announcement. It was built from this
repository for use on the machine that built it, and is recorded here only
because it is the version the 1.0.1 release followed.

Its own release notes, including the failure-handling and privacy detail that
carried forward into 1.0.1, are in
[`code/docs/RELEASE_NOTES.md`](code/docs/RELEASE_NOTES.md).

[1.0.1]: https://github.com/VietAIMEM/douyin-media-downloader/releases/tag/v1.0.1
