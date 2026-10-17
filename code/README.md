# Douyin Downloader

A Windows desktop application for downloading publicly accessible Douyin media,
built with Tauri 2 (Rust core + React/TypeScript UI).

**Version 1.0.0 — Windows x64.**

This is a **personal-use build**: built from this repository, installed on the
machine that built it, and not published or distributed. There is no download
page, no update feed and no code-signing certificate, so Windows will warn about
the publisher. See [Installing and keeping this build up to date](docs/PERSONAL_USE.md)
for installation, manual upgrades, backup, and what that warning means.

## Using the application

| | |
| --- | --- |
| [Personal use](docs/PERSONAL_USE.md) | Installing, upgrading by hand, backing up your data, the publisher warning |
| [User guide](docs/USER_GUIDE.md) | Every screen, and how to use it |
| [FAQ](docs/FAQ.md) | The questions that come up most |
| [Troubleshooting](docs/TROUBLESHOOTING.md) | What to do about each message the application shows |
| [Privacy](docs/PRIVACY.md) | What is stored, what is sent, and what is not |
| [Release notes](docs/RELEASE_NOTES.md) | What is in this version, and its known limitations |
| [Support](docs/SUPPORT.md) | What to include in a report, and what to remove from a log first |

**Install** by building the installer and running it — see
[personal use](docs/PERSONAL_USE.md). `Douyin Downloader_1.0.0_x64-setup.exe`
installs for the current user, so it needs no administrator. The MSI is the same
application for managed machines and does install for all users.

**You need** Windows x64 and the WebView2 runtime, which is present on Windows 11
and current Windows 10. Chrome or Edge is recommended and FFmpeg is optional —
the Home screen reports whether each was found, and the application says what is
missing rather than failing quietly.

**Your files** go to `%USERPROFILE%\Downloads\Douyin` unless you change it.
Settings, the Library and download history live in
`%APPDATA%\DouyinDownloader\DouyinDownloader\data\`. Uninstalling removes neither.

The rest of this document is about how the application is built and why it works
the way it does.

## How it works

Douyin does not let a plain HTTP request read a post's media: its pages fetch the
post from a signed, session-bound endpoint. The resolver therefore works in two
steps — the ordinary HTTP path first, and, only when the served pages turn out to
contain no post, a real browser rendering the same public page to read the media
it obtains. The browser resolves; the ordinary HTTP pipeline downloads. See
[Browser-assisted resolution](#browser-assisted-resolution).

Douyin also serves a video and its audio **as two separate tracks**. The audio is
not in the player's configuration — it is fetched by the page itself — so it is
found by watching the page's network while it renders, fetched as a second track,
and combined with the video by FFmpeg into one playable MP4. Combining is a
stream copy, never a re-encode, and FFmpeg is not used for anything else. See
[Video and audio](#video-and-audio).

---

## Prerequisites

| Tool | Version used | Notes |
| --- | --- | --- |
| Node.js | 24.x | Frontend build. |
| Rust | 1.77+ (built on 1.99 stable-msvc) | `x86_64-pc-windows-msvc` toolchain. |
| MSVC build tools | VS 2022 C++ | Linker for the Rust core. |
| Windows SDK | 10/11 | Required by Tauri (WebView2, Common Controls). |
| Chrome or Edge | any current release | Optional. Used for browser-assisted resolution; without one, resolution falls back to the HTTP path only and says so. |
| FFmpeg | any current release | Optional. Needed to combine Douyin's separately served video and audio tracks into one playable file; without it, such a download is refused with a message naming FFmpeg. |

WebView2 is already present on Windows 11; on Windows 10 it is installed by the
Tauri bundler at install time.

## Install

```bash
npm install
```

## Run

Development (Vite dev server + Tauri with hot reload):

```bash
npm run tauri:dev
```

Release-style launch (frontend is compiled and embedded into the binary, so no
dev server is needed):

```bash
npm run build && npm run tauri:build
```

The built executable is at
`src-tauri/target/release/douyin-downloader.exe`.

A plain `cargo build` inside `src-tauri/` also produces a runnable binary: the
`custom-protocol` feature is on by default, so the embedded `dist/` is served
without a dev server. Rebuild the frontend (`npm run build`) before compiling if
you changed anything under `src/`.

## Tests

Rust (unit + integration):

```bash
cd src-tauri && cargo test
```

Frontend tests, type check, and production build:

```bash
npm test
```

```bash
npm run typecheck && npm run build
```

The frontend suite uses `vitest` with `jsdom` and Testing Library. It covers the
behaviour a person needs in order to use the shared controls without a mouse or
without sight — that a field's error is attached to the field, that a destructive
button asks before acting and can be backed out of with Escape, and that a
progress bar says what it is measuring. It is not an accessibility certification,
and it does not render whole screens.

Current results: **607 Rust tests pass** (439 library unit tests plus integration
suites for database, errors, settings, URL validation, the real download
pipeline, the library, and the Tauri command bridge, which includes the whole
profile workflow), and **17 frontend tests pass** (the shared controls'
accessibility behaviour). `cargo check --all-targets`, `cargo clippy
--all-targets -- -D warnings`, `tsc --noEmit`, and `vite build` are all clean.

Fifteen further tests are `#[ignore]`d because they need live network access, an
installed browser, or both. Run them deliberately with:

```bash
cd src-tauri && cargo test --test live_douyin -- --ignored --nocapture
```

```bash
cd src-tauri && cargo test --test live_browser -- --ignored --nocapture
```

> Run the live suites **one test at a time** — add `--test-threads=1`. Each of
> them launches a real browser and drives a real page, and `cargo test` runs a
> binary's tests in parallel by default: nine at once starves Chromium's command
> loop, and tests that pass on their own fail with `the browser did not answer
> `Page.enable` in time`. The parallel failures say something about the machine,
> not about the application.

> Windows note: the integration test that drives the real IPC bridge builds a
> webview, which requires the Common Controls v6 application manifest. The
> shipped binary gets it from `tauri-build`; test binaries get an identical copy
> via `src-tauri/tests/common-controls-v6.manifest`, wired up in `build.rs`.
> Without it the loader fails before `main` with `STATUS_ENTRYPOINT_NOT_FOUND`
> (`0xc0000139`). This affects test binaries only — the application binary is
> unchanged.

## Project layout

```
code/
├─ src/                       React + TypeScript frontend
│  ├─ pages/                  One module per nav destination
│  ├─ features/               Reusable feature sections (URL inspector, settings form, diagnostics)
│  ├─ components/             Layout + UI primitives (incl. ProgressBar)
│  ├─ hooks/                  useDownloadEvents — live download progress
│  ├─ stores/                 Zustand stores (settings, backend status)
│  ├─ services/               ipc.ts, api.ts, downloadEvents.ts
│  └─ types/                  Wire types and domain models
├─ src-tauri/                 Rust core (its own Cargo package)
│  ├─ src/commands/           Tauri command layer (the IPC boundary)
│  ├─ src/services/           Composition root + Single URL, Profile and Library workflows + browser policy
│  ├─ src/resolvers/          The three resolver traits, the URL parser, Douyin
│  │  └─ douyin/              classification, payload extraction, player reading, profile pages
│  ├─ src/downloads/          DownloadManager, transfer worker, history recorder
│  ├─ src/media/              MediaMuxer — combining tracks with FFmpeg
│  ├─ src/network/            HttpClient (reqwest), destination policy, guarded resolver
│  │  └─ chromium/            Browser discovery, lifecycle, CDP client, network guard (proxy.rs)
│  ├─ src/storage/            StorageManager (paths, filename templates)
│  ├─ src/settings/           SettingsService
│  ├─ src/database/           Pool, migrations, repositories (incl. the Library's joined read)
│  ├─ src/models/             Domain models
│  ├─ src/errors/             AppError + serialization + command conversion
│  ├─ src/logging.rs          tracing setup
│  └─ tests/                  Integration suites (incl. download_pipeline, live_*)
├─ migrations/                Versioned, forward-only SQL
└─ ARCHITECTURE.md            Layer map and design decisions
```

The brief sketched `src/frontend` + `src/backend`; the delivered layout splits on
the same responsibility lines but follows the Tauri convention, because the Rust
core has to be its own Cargo package. See the layout table in
[ARCHITECTURE.md](ARCHITECTURE.md) for the one-to-one mapping.

## What works

- **Seven screens** — Home, Profile Downloader, Single URL, Downloads, Library,
  History, Settings — with a working sidebar and routing.
- **Real backend communication.** Every value on screen comes from a Tauri
  command. Nothing is faked in TypeScript.
- **Single URL workflow.** Paste a link → analyze → preview → pick a media
  variant → download → find the file in History.
- **Profile workflow.** Paste a creator's profile link → identify the creator →
  browse their posts a page at a time → select individually or in bulk (by type,
  by new, by previously failed) → submit the selection to the *same* download
  pipeline. Previously downloaded posts are recognised from local data, and a
  post whose file is no longer on disk is offered again rather than reported as
  already downloaded.
- **The Library.** Everything discovered or downloaded, in one place: seven
  views (All, Videos, Images, Audio, Downloaded, Failed, New), free-text search
  across creator, caption, post id, URL and filename, filters for creator,
  status, publication date, duration and resolution, five sort orders, paging,
  and a JSON or CSV export of exactly what the view matches. A post counts as
  downloaded only while the file it produced is still on disk, so deleting a
  file returns the post to the New view with an explanation rather than leaving
  a row claiming a file that is gone.
- **Downloads that cannot falsely succeed.** A task reaches `Completed` only
  after the file has been moved into place, read back, and both its task row and
  its history row have been written in one transaction. A source that answers
  with an error page, an empty body, or a file holding no media fails instead of
  producing a plausible-looking file. A cancel or pause that lands while a
  download is finishing wins, so a stopped task is never announced as finished.
- **An outbound destination policy.** Loopback, private, link-local, shared and
  reserved addresses are refused, at the start of a request and at every redirect
  hop, through both the HTTP client and the browser.
- **Sensitive URLs stay out of the logs.** Signed media URLs are logged as their
  host and path only — never with the query string that carries the token.
- **A download is checked before it starts.** The destination is created and
  actually written to, and free space is compared against the expected size when
  the source has stated one.
- **Filesystem containment that sees links.** Destinations are checked on their
  text and again on where they resolve, so a junction or symbolic link inside the
  download folder that leads elsewhere is refused before anything is written
  through it.
- **Douyin URL validation, classification, and short-link expansion**, all
  derived from the parser rather than hard-coded.
- **Two-step resolution**: the ordinary HTTP path, and — only when the served
  pages turn out to hold no post — a real browser rendering the same public page
  to read the media it obtains. The browser resolves; the HTTP pipeline
  downloads.
- **Complete video and audio.** A video whose sound Douyin serves as a separate
  track is fetched as both and combined into one playable MP4 by FFmpeg, with the
  streams copied rather than re-encoded, and the result read back before the task
  is called complete.
- **Post and media resolution** into `Post` / `MediaItem` / `MediaVariant`,
  including the image, carousel, and audio shapes the source exposes, and telling
  a clean rendition apart from a watermarked one without ever rewriting a URL.
- **Real media transfer** with byte-level progress, live cancellation that stops
  the socket, bounded retry with exponential backoff, conflict handling, and
  Windows-safe filenames under the configured download directory.
- **Download management.** Every download — from either screen — goes into one
  persisted queue served by one worker pool. The pool's width comes from the
  `Max concurrent downloads` setting and is read live, so changing it takes
  effect without a restart. Tasks can be paused, resumed, cancelled, retried
  singly or in bulk, removed from the list, and the completed entries cleared —
  all without deleting the files or the history. Progress reports bytes, speed
  and ETA, with the queue's aggregate shown above it. Queue state survives a
  restart: what was queued is picked up, what was mid-flight is returned to the
  queue, and what was paused stays paused.
- **Download history** for completed, failed, *and* cancelled outcomes, with
  open-file / open-folder actions.
- **SQLite** with all six tables, WAL journaling, foreign keys enforced, and a
  recorded, forward-only migration history.
- **Settings** load at startup, are validated before saving, and persist across
  restarts.
- **Structured errors** distinguishing validation / network / resolution /
  download / storage / database / unsupported / unavailable, each carrying a
  developer message and a separate user-safe one.
- **Structured logging** to stdout and a rotating file, at a level driven by the
  persisted setting.

## The Library

The Library is a read over what the other workflows wrote. It adds no table, no
download path, and no resolver: it reads posts, creators, media and downloads
through the same repositories, and submits a selection through the same
`submit_profile_downloads` command the Profile Downloader uses.

### What each view means

| View | What it contains |
| --- | --- |
| **All** | Everything the library holds, whether or not it was ever downloaded. |
| **Videos** | `video` posts. |
| **Images** | `image` **and** `carousel` posts — a carousel is Douyin's multi-image post, so leaving it out would hide every one of them. |
| **Audio** | `audio` posts. |
| **Downloaded** | Posts whose every media resource is present on disk. A carousel with one of three images fetched is *not* here. |
| **Failed** | Posts whose last download attempt failed. |
| **New** | Discovered, with no successful local download. Includes posts whose file was deleted, because those can be downloaded again. |

"Downloaded" and "New" are defined against the *reconciled* status, never against
a raw database column, so they cannot claim a file that is not there.

### Search

One box, matched case-insensitively (ASCII) against:

- the creator's display name, platform id, and handle;
- the post's caption, platform post id, URL, and canonical URL;
- the destination path and file name of the post's newest download;
- the `local_path` of any recorded history entry for the post.

`%` and `_` in your own text are escaped, so searching for `100%` finds the post
containing `100%` rather than matching everything. An empty box filters nothing.

### Filters

Free-text search plus: **creator**, **download status**, **published from /
until**, **minimum duration**, and **minimum width**. The view tabs select the
content type.

Duration and resolution are aggregates over a post's media, and both are **kept
when unknown**: a post nobody measured is not a post that is too short or too
small. Dropping it would silently hide items the source simply did not describe.
Posts with no publication date are likewise excluded by a date range — nothing
says they fall inside it.

### Sorting and paging

Newest, oldest, title A-Z, creator A-Z, and downloaded-first. Every order has a
tie-breaker on `p.id`, so two rows that compare equal never swap places between
pages. An undated post sorts last under both date orders: it is not the newest
thing in the library, it is the one with the least information. Title order uses
the title you are *shown*, fallback included.

Paging is 50 rows at a time. The total and the per-tab counts always describe the
**whole match**, not the page, so a tab never claims a number the listing cannot
produce.

### Duplicates and "already downloaded"

A post is one row however many times it was downloaded; a media resource
downloaded twice counts once, so a carousel cannot look complete because one of
its images was fetched repeatedly. Two posts with identical captions are two
posts and are never collapsed. A file-name collision disambiguated on disk
(`clip.mp4` and `clip (1).mp4`) leaves two rows, each pointing at its own file.

Submitting a selection skips anything already done:

| Local state of the media | What happens |
| --- | --- |
| A transfer is queued, running, or paused | Skipped — counted as `alreadyDownloading`. |
| Finished, and the file is still there | Skipped — counted as `alreadyDownloaded`. |
| Finished, but the file is gone | Downloaded again, and the outcome says so. |
| Never downloaded | A task is created through the central download manager. |

### Downloading the items you do not have

Select rows (or **Select new on this page**) and press **Download selected**. Each
selected post is resolved through the same resolver the Single URL screen uses,
and the task is created through `AppCore::create_download` — there is no second
download engine. The banner afterwards counts what was queued, what was already
downloaded, what was unsupported, and what failed.

The Profile Downloader's **Select new (n)** does the same thing for a whole page
of a creator's posts.

### File and folder naming

The templates are on the Settings screen and were already in place before this
phase; the Library only reports where a file ended up.

| Variable | Value |
| --- | --- |
| `{creator}` | The creator's display name, falling back to their platform id. |
| `{handle}` | The creator's handle, without the `@`. |
| `{id}` | The platform post id. |
| `{date}` | Publication date (`YYYYMMDD`), or the download date when unknown. |
| `{time}` | Publication time (`HHMMSS`). |
| `{type}` | `video`, `image`, `carousel`, or `audio`. |
| `{index}` | The resource's position within the post, for multi-item posts. |
| `{caption}` | The caption, sanitized and truncated. |
| `{ext}` | The file extension, without the dot. |

Defaults are `{creator}/{type}` for the folder and `{date}_{id}` for the file.
Windows-illegal characters are replaced, a path separator inside a value cannot
escape the folder, and a path that would exceed the Windows length budget is
shortened rather than failing. **Template changes apply to new downloads only.**
Files already on disk keep the names they have — the Library reports the real
path, and nothing is moved behind your back.

### Export

**Export JSON** and **Export CSV** write every record the current view, search and
filters match — not only the rows on this page — to
`<download folder>/exports/library-<timestamp>.<ext>`, and report the path. The
folder is created on demand and the write is containment-checked against the
download directory like every other write.

The schema is fixed, with a header row naming each column in order:

| Field | Notes |
| --- | --- |
| `creatorName`, `creatorId`, `creatorHandle` | `null` when unknown. |
| `postId`, `postUrl` | The platform post id and the **post page** URL. |
| `caption`, `mediaType`, `publishedAt` | `publishedAt` is ISO 8601, or `null`. |
| `status`, `downloadState` | The reconciled status and the newest task's state. |
| `localPath`, `downloadedAt`, `filePresent` | Where the file is, and whether it is still there. |
| `mediaItemCount`, `downloadedItemCount` | How many resources the post has, and how many are present. |
| `durationSeconds`, `width`, `height` | `null` when the source did not report them. |
| `error` | Why the status is what it is, when there is something to say. |

Two properties of the export are deliberate:

- **No media URL is ever written.** Post page URLs are pages; media URLs are
  signed, expiring tokens. Only the former belongs in a file you might share.
- **CSV is RFC 4180**, so a caption containing a comma, a quote, or a line break
  cannot break the table, and a value beginning with `=`, `+`, `-`, `@`, a tab or
  a carriage return is prefixed with an apostrophe, so a caption reading
  `=HYPERLINK(...)` is treated as text by Excel and Sheets rather than executed.
  The apostrophe is not part of the value, but it *is* part of the file. This is
  the one place the export changes what it writes, and it trades exact
  round-tripping for not handing a spreadsheet a formula. A numeric column is
  never prefixed or quoted, and an unknown number is an empty cell rather than a
  zero.

### What the Library does not do

- It does not monitor a profile or download anything on a schedule.
- It does not move, rename, or repair files that are already on disk. Moving a
  file yourself makes its post read as `Ready` again, because the path in the
  database no longer points at a file; the row is not rewritten.
- It does not checksum files. A file that exists is treated as present; a
  truncated file is not detected until you open it.
- Its scan is bounded at 20,000 posts per query. The pagination line shows
  `(from N scanned)` when that bound is what limited the result.

## Reliability, safety and recovery

Phase 06 hardened the parts of the application that decide whether a download
that *looks* finished actually is. What follows is the behaviour as it is now,
not as it was planned.

### A download is complete only when the file is

A task reaches `Completed` only after all of this has happened, in order:

1. every required track has been transferred;
2. separate video and audio tracks have been combined, when the source served
   them separately;
3. **the result has been read back** — for a combined file with `ffprobe`, and
   for a single-track download by a content check plus a probe when FFmpeg is
   available;
4. the file has been moved into its final destination;
5. the task row **and** its history row have been written, in one transaction.

A source that answers `200` with an HTML or JSON error page is the failure this
catches: the body is not empty, the status is a success, and without a read-back
the file would be named `.mp4` and recorded as a download. Such a response now
fails with a message naming what the source actually returned.

If the record cannot be written, the task is **not** reported as finished. The
file stays on disk and the task stays retryable; the alternative — telling the
interface a download completed when the database disagrees — is the one outcome
the UI must never show.

### Cancelling and pausing cannot be outrun

Every state change that finalizes a download is a compare-and-set: `UPDATE …
WHERE id = ? AND state = 'downloading'`. A cancel or pause that lands while the
file is being moved into place wins, and the completion is discarded rather than
announced. This is not theoretical — the queue manager and the transfer workers
are separate tasks sharing a pool of connections, and a plain read-then-write let
a cancelled task be returned to `downloading` and then downloaded to completion.

Pausing is not cancelling. A paused task keeps its partial file and its state;
resuming continues from what is on disk.

### Partial files and `Range`

A paused download keeps its `.part` file. A cancelled or permanently failed one
deletes it. A partial is only ever *continued* when the source's answer can be
validated:

| The source's answer | What happens |
| --- | --- |
| `206` starting at the requested offset | Continued. |
| `206` starting anywhere else | Restarted from zero. |
| `206` with a total smaller than the offset | Restarted from zero. |
| Malformed or missing `Content-Range` | Restarted from zero. |
| `200` (the server ignored `Range`) | Restarted from zero — the full body is never appended to the partial. |
| `416 Range Not Satisfiable` | Restarted from zero. |

Restarting truncates the file first, so a full response can never be written on
top of bytes already there. And a continuation is never taken on the strength of
the offset alone: the partial carries the resource's identity beside it, the
request asks the *server* to confirm that identity with `If-Range`, and the
response is checked against what the partial was written under before a byte is
appended. See [Partial-file identity](#partial-file-identity) for the gates, and
[Known limitations](#known-limitations) for what a source can still prevent.

### Disk space and the destination

Before a download starts, the destination directory is created and a probe file
is actually written and removed. That is the only test that answers the question:
a read-only folder, a disconnected share, a full disk, and a permissions problem
all present as a failed write and as nothing else. A failure here is reported
before any bytes are transferred.

Free space is checked when the size is known, with headroom of **3×** the
expected file — a video whose sound arrives separately is written as two tracks
and then copied into one combined file, so the peak is more than the final size.
When the size is *not* known the comparison is skipped rather than guessed: a
source that omits `Content-Length` must not have its download refused. Free
space is usually known only on a resume or a retry, because `Content-Length`
arrives with the response.

Running out of room mid-transfer is still possible, and the preflight cannot
prevent it: it reports what was true a moment ago. What it does guarantee is that
the partial is discarded rather than promoted, and the failure is a message about
space rather than a mystery.

### Filesystem safety

* Destination paths are checked twice against the download root — once on the
  path *text*, and once on where the filesystem says it actually resolves. A
  junction or symbolic link inside the download folder that leads elsewhere is
  refused, both when the path is planned and again immediately before the first
  write.
* The `overwrite` conflict policy uses the platform's atomic replace. The file
  that was there is never deleted first, so a failure cannot leave the user with
  neither the old file nor the new one.
* Moving a file from another volume is copied to a sibling and then renamed, so
  a copy that fails part-way leaves the destination either absent or complete —
  never a truncated file that a later run would accept as an existing download.
* Removing a queue entry or clearing completed items touches the queue only.
  Files and history are kept.

### What may be contacted

Every outbound request — through the HTTP client or the browser — is checked
against a destination policy. Loopback, private, link-local, shared, reserved and
multicast addresses are refused, along with the special-use names (`localhost`,
`*.local`, `*.internal`, `*.home.arpa`). Redirects are re-checked at every hop,
not only at the start.

There is deliberately **no allowlist**. Douyin serves media from a wide set of
CDN names that changes without notice, and a list that must be updated to keep
downloads working would be switched off long before it caught anything. The
policy is about *where* traffic may go, not *which* hosts are legitimate — that
is the resolver's job, and it already refuses anything that is not a Douyin page.

The check inspects the URL, not the resolved address, so it does not defend
against DNS rebinding. It is a guard against accident and misdirection, not a
hard security boundary.

### Timeouts

Page and short-link requests get a **total** budget (`Request timeout` in
Settings, default 30 s): a page that has not finished in that time is not going
to.

Media transfers get a **read** timeout instead, using the same setting. A total
deadline would abort a large file on a slow connection that was still making good
progress — which is exactly what it did before Phase 06 — whereas a read timeout
fails only when the source stops sending, which is what "the transfer stalled"
actually means.

### Logging and sensitive values

A Douyin *media* URL is a signed URL: its query string is a bearer token. Media
URLs and the resolver's landing URLs are therefore never logged or put in an
error's detail. What is logged is the scheme, host, and path — enough to say
which operation failed and against which host, and nothing that could be reused.

The download *task's* stored error, which the UI shows, is a normal sentence:
what went wrong, in terms of the file rather than the request.

### Settings that cannot be used

Settings are validated when saved, and repaired **field by field** when loaded.
A stored value outside its range — a concurrency of 1000, a relative download
folder, a template containing a path separator — is reset to its default and the
change is logged. One bad value does not cost the user their other preferences,
and it cannot stop the application from starting.

### What was measured, and where

The load behaviour is measured by tests rather than asserted, and the figures
that matter are recorded here because a claim without a measurement is not
evidence.

| What | How | Result |
| --- | --- | --- |
| The concurrency limit is an upper bound | 25 queued transfers, a limit of 2, sampling the in-flight count while it drains | Never above 2. |
| The pool actually *reaches* its limit | 3 queued transfers, a limit of 3, transfers held open long enough to overlap | Peak of exactly 3. |
| A large batch is bounded | 25 transfers, a limit of 2 | Peak of 2, all 25 completed. |
| The destination check survives concurrency | 16 simultaneous preflights of one directory | All succeed; no leftover probe files. |

Measured on a 16-core Windows 11 machine. The same suite was then run with eight
CPU-burning processes saturating the machine, because that is the condition under
which timing-based tests lie: several failed there, none by exceeding the limit —
they failed because a fixed sleep had not given the transfers time to start. Those
tests now wait for the condition instead of guessing at a duration, and the suite
passes under the same saturation. What that measured is that the *pipeline's
timings stretch by roughly an order of magnitude on a starved machine*, which is
worth knowing and is not a correctness problem.

### How destinations are checked

There are two halves, and both are needed.

**The URL.** Before a request is built, the destination is classified: the
scheme, the host (a literal IP is judged arithmetically; a name by its reserved
suffixes), and — on every redirect hop, through a custom redirect policy — the
hop's own URL. A refused URL is a typed, **non-retryable** error.

**The address.** A URL only carries a name. Which address that name becomes is
decided by whoever resolves it, and checking a name and then resolving it again
is not a check: a name whose answer changes between the two (DNS rebinding)
walks straight through. So resolution goes through `GuardedResolver`
(`network/resolver.rs`), installed as the HTTP client's resolver. It classifies
the name first (a reserved name is refused without a lookup), then every address
in the answer. What it returns is what the connector dials — there is no second
lookup for anything to interfere with.

Two details are load-bearing:

- **A mixed answer is refused whole.** A name answering with a public address
  *and* a loopback one is either misconfigured or hostile, and the two are
  indistinguishable. Keeping the public member would survive the attack; refusing
  the answer does not.
- **The refusal survives the transport stack.** It happens inside the resolver,
  several layers below the error mapping, and `reqwest` reports it as an ordinary
  connect error — which would otherwise be *retried*, dialling a blocked
  destination until the attempt budget ran out. The refusal carries a marker and
  a stable category tag; the error mapping walks the source chain looking for
  them, and the refusal that comes back is typed and non-retryable.

TLS is untouched: the connection still uses the URL's hostname for SNI and
certificate verification. Pinning the address does not weaken the certificate
check, because the certificate is matched against the name, never the address.

One deliberate exception to the policy lives beside it: the renderer's client
for reading the browser's DevTools target list, which is built with the
permissive policy because that endpoint *is* `127.0.0.1` on a port this
application chose. It is a control channel to the application's own child
process, not a content fetch, and wiring the content policy in front of it would
refuse every render.

### Partial-file identity

A `.part` file only means "some bytes of *something*". Matching offsets and
totals do not prove the bytes on disk and the bytes about to arrive belong to the
same version of the resource — a re-encode with the same length would pass every
offset check and splice two versions into one file.

So a partial carries its identity beside it, in a `.meta.json` sidecar written
**when the response headers arrive** — not when the transfer completes, because
the point is that a transfer paused mid-body already knows what it is
downloading. The sidecar holds the response's validator and the resource's total
size. It never holds the URL: the task has it, and a signed media URL's query
string is a credential.

Only a **strong** validator is stored. A weak `ETag` (`W/"…"`) is not: it is
defined to compare equal for representations that are merely *semantically*
equivalent, so two byte-different encodings can share one, and `If-Range` is
defined not to accept one at all. Where a source offers a weak tag and a
`Last-Modified` beside it, the date is used instead; where it offers only the
weak tag, the partial cannot be resumed.

A `Last-Modified` is taken **only when it can be deduced to be strong**, and the
deduction is the one RFC 9110 permits a client: the response's own `Date`, at
least a second later than the `Last-Modified`. Both come from the same origin and
so share a clock, and that gap rules out a second modification inside the second
the date names. Without a `Date`, with an unreadable one, or with one inside the
same second, the date stays implicitly weak and the partial is not resumable. The
test is applied to every response, the `206` included — an ambiguous date is
refused in both directions rather than only the first.

On resume, five gates decide between continuing and restarting:

1. The sidecar must exist, parse, and name a validator that can establish
   identity, and the file's length must fit inside the recorded total. Anything
   else — missing, malformed, or drift — means the partial's identity is not
   established, and it is discarded.
2. The request carries `If-Range` with the stored validator, which makes the
   *server* decide whether the range is safe. A mismatching condition is answered
   with the full-body `200` the specification requires, which the client treats
   as a restart (never an append).
3. Because some origins answer `206` without honouring `If-Range`, the response's
   own validator is compared against the stored one, and a mismatch discards the
   partial.
4. **A response that names no validator at all is refused.** A `206` says the
   range was satisfied; it says nothing about whether the condition was even
   evaluated, and an origin that ignores `If-Range` answers exactly as one that
   honoured it. "It did not tell me anything" is not "it told me it is the same
   entity", and a matching offset and total are not evidence either — a re-encode
   at the same length matches both exactly. So the partial is discarded and the
   transfer restarts from zero.
5. The response's stated total is compared against the one the partial was
   written under — a second, independent signal that speaks even when the
   validator comparison has nothing to work with.

A source that offers no usable validator cannot have its partials resumed, and
one that answers a range without naming a validator cannot either. Both are
properties of the source, and each costs one re-download; the alternative costs a
file assembled from two versions.

### The browser

Chromium runs in a throwaway profile with a remote-debugging port chosen by the
OS, and is started off-screen: Douyin refuses headless access, so a window that
is positioned outside the visible desktop is what makes resolution work at all.
The process is identified by its own pid, and cleanup uses that pid — it never
terminates processes by name, so an unrelated Chrome or Edge window is never
touched. The temporary profile is removed when the session is dropped, including
after a crash of the browsing session.

## The Single URL workflow

```text
URL ─▶ validate ─▶ expand short link ─▶ classify
                                            │
                    ┌───────────────────────┴───────────────────────┐
                    ▼                                               ▼
              post intent                                     profile intent
                    │                                               │
          fetch share page, parse aweme               report "use the Profile
                    │                                Downloader" — not a post
          normalize Post + MediaItem + variants
                    │
          persist to SQLite  ─▶  preview on screen
                    │
          user picks media item + variant
                    │
          DownloadManager.enqueue  ─▶  transfer worker
                    │
          HTTP stream (progress, cancel, retry)
                    │
          StorageManager: temp file ─▶ safe final path
                    │
          DownloadTask = COMPLETED  ─▶  download_history
```

The screen reports its state explicitly — idle, validating, resolving, resolved,
unsupported, unavailable, error, downloading, completed — so a user can tell an
invalid link from a source that would not answer from content that is genuinely
unsupported.

## Live validation

Two kinds of real verification back this build.

**The download pipeline, end to end, against a real server.** `cargo test
--test download_pipeline` starts a throwaway HTTP server on loopback, boots the
core with the *real* `reqwest` client, and asserts that bytes leave the socket,
land in the right file under `{creator}/{type}`, match what was served, and are
recorded in `download_history` with status `COMPLETED`. It also covers a 404
(permanent, not retried) and a cancel mid-transfer (socket stops, no file left
behind). This is `cargo test --all-targets`, not an ignored test.

**The real Douyin path, deliberately.** `cargo test --test live_douyin --
--ignored --nocapture` runs the whole workflow — analyze, choose a variant,
download, verify the file, check history — against the shipped links:

| Step | `https://v.douyin.com/7yyhouNSzvA/` | `https://v.douyin.com/SMt7NsUPaGo/` |
| --- | --- | --- |
| Short-link expansion | ✅ → `douyin.com/video/7684462711358426420` | ✅ → `douyin.com/user/MS4wLjABAAAA76ZX…` |
| Classification | ✅ post | ✅ profile |
| Post + media resolution | ✅ via browser — 1 item, 3 variants | n/a |
| Download | ✅ 5,911,418 bytes to `Videos/7684462711358426420.mp4` | n/a |
| File verified | ✅ real MP4 (`ftyp`/`moov`, video track present) | n/a |
| History recorded | ✅ `COMPLETED` | n/a |
| Treated as a downloadable post? | yes (correct) | **no** (correct — reported as a profile) |

The resolver logs exactly which URL it asked for and what came back, so a change
on Douyin's side is visible without re-instrumenting anything:

```text
WARN resolver: page loaded but carried no post data
     post_id="7684462711358426420"
     url=https://www.douyin.com/video/7684462711358426420?previous_page=web_code_link
     diagnostic=bytes=72914 state_blobs=[] parsed_blobs=0 aweme_keys=[] loader_sections=[-]
INFO resolver: falling back to browser rendering  renderer="chromium"
INFO resolver: post resolution completed via browser  media_items=1 variants=3
```

**The real profile path, deliberately.** `cargo test --test live_douyin --
--ignored --nocapture --test-threads=1` runs the whole workflow against the
shipped public links, including the full Phase 03 flow:

| Step | Result for `https://v.douyin.com/mv4vQSIXgg0/` |
| --- | --- |
| Short-link expansion | ✅ → `douyin.com/user/MS4wLjABAAAA76ZX…` |
| Classification | ✅ profile (never treated as a downloadable post) |
| Creator resolution | ✅ name `郭虎`, handle `@42299195146`, 50 posts, verified badge, signature, avatar |
| Post listing | ✅ **21 genuine creator posts** |
| Pagination | ✅ page two advanced, no repeats |
| Selected post → media | ✅ `media_items=2`, `variants=4` |
| Task through the shared pipeline | ✅ `tasksCreated=1` |
| Real file | ✅ `郭虎/Videos/7393529130723085603.mp4`, **100,889,029 bytes** (second run: 11,156,435 bytes) |
| History | ✅ `Completed`, same size |
| Listing afterwards | ✅ that post now reads `Downloaded` |

### Why a profile needs a browser *window*

Douyin's listing request (`/aweme/v1/web/aweme/post/`) — the one the *page
itself* makes — is answered differently depending on whether the browser has a
window. The same public page, the same fresh throwaway profile, the same URL:

| Browser | Result |
| --- | --- |
| `--headless=new` | `403 Blocked by ArgusSecurityPlugin Sign Invalid`, or `200` with a **0-byte body**; grid empty |
| `--headless=old` | same |
| **an ordinary window** | the real listing |

Measured: **8 headless attempts, 0 listings. 4 windowed attempts, 4 listings** —
21 posts and ~1.2 MB of payload each time. The empty `200` is the dangerous
shape: a refusal wearing the appearance of an empty page, which must never be
read as "this creator has no posts".

This is not signature forging, session theft, or stealth tooling — it is an
ordinary browser window opening the same public page a person would open. The
same applies to a signed-out video page, which headlessly redirects to the feed
with no player at all, so post resolution asks for a window too.

### The page's own refresh, and the bounded retry

Even in a window the *first* listing request often fails, and the page says so:
`服务异常，重新刷新拉取数据`. That message contains a clickable `<span>刷新</span>`
— not a button or a link, which is why an earlier selector-based search for a
retry control missed it. Waiting does not help; pressing it does:

```text
initial state              : 403, 0 cards
control (no action, 20 s)  : still 0 cards   <- it does not recover by itself
press 刷新                 : a fresh listing request is sent; cards render
```

So the retry is the page's own control, pressed **at most twice** per listing
request — the same "two extra attempts" rule the project applies elsewhere. It
happens inside the single browser session the render already paid for, so it
costs no extra browser launch, and the application never re-signs, retypes, or
replays a parameter itself. A listing that is genuinely empty still reads as
empty, and a refusal still carries a typed error rather than an invented result.

## Windows UX and accessibility

### Destructive actions ask first

Two actions were irreversible enough to need a question: **Clear completed** on
the Downloads screen and **Restore defaults** in Settings. Each now asks in
place — the button becomes a named question with a confirming button and a
cancel — rather than opening a modal. The application has no dialog primitive,
and adding one (focus trap, overlay, portal, escape handling) for two actions is
a lot of machinery to get wrong; asking in place keeps the question next to what
it is about and cannot leave the page behind it unreachable.

What the question does take seriously is the keyboard: Escape cancels, focus
moves to the confirming button when the question appears, and returns to the
trigger when it is answered — so a keyboard user is never left with focus on an
element that no longer exists.

These confirmations are for actions that lose *state*, not files. Clearing
completed entries never deletes media, so its question says so.

### Reachable without a mouse

* Every screen is reachable from the sidebar, which is a labelled `<nav>` of
  real links.
* Every control that can be focused shows where the focus is. The shared
  `<Button>` has always done this; the controls that are *not* `<Button>` — the
  Library's view tabs, the test-link chips, a notice's dismiss button — were each
  missing it, and now share one `focus-visible` ring. `focus-visible` rather than
  `focus`, so the ring appears for keyboard navigation and not on a mouse click.
* The Library's view tabs are a named group of toggle buttons, so which view is
  showing is available to assistive technology rather than only implied by
  colour.
* A form field is linked to its own label, hint and error by id. The error is a
  live region and the field is marked invalid, so a rejected value is announced
  with the field it belongs to instead of being a sentence that happens to sit
  underneath it.
* A progress bar exposes real `progressbar` semantics and states the numbers as
  text. When the total is unknown it omits `aria-valuenow` rather than reporting
  a zero — a screen reader announcing "0 percent" for a healthy download would be
  worse than announcing nothing.
* Status is never carried by colour alone: every badge has its state written out,
  and the queue rows say what they are doing in words.

### Layout

The screens reflow to a single column at narrow widths, and the tables, the tab
row, the filter panel and the action bars all wrap rather than clipping. The
theme follows the persisted setting and the OS preference, and the focus ring and
every status colour resolve through the theme's tokens, so both themes stay
legible.

## Known limitations

- **A resumed download is only as good as the validator the source offers.** A
  `Range` continuation is taken only when the partial names an identity the
  source *confirms*: the request carries `If-Range`, and the response's status,
  starting offset, stated total, and validator are each checked before a byte is
  appended. A `206` that names no validator is refused rather than trusted, so a
  resource that changed under a stable URL — even to a re-encode of exactly the
  same length — is restarted, never spliced. What remains belongs to the source:
  a weak `ETag` is not usable as a resume validator at all, a `Last-Modified` is
  used only when a `Date` at least a second later lets it be deduced as strong,
  and a source offering neither is not resumable. Each of those costs one
  re-download, and the transfer restarts from zero rather than guess. See
  [Partial-file identity](#partial-file-identity).
- **The browser's own network traffic is bounded, not fully sealed.** The HTTP
  client validates every resolved address it dials — see [How destinations are
  checked](#how-destinations-are-checked) — and the app-owned Chromium is now
  held to the same rule: it is launched with every request routed through a
  loopback proxy this application owns, which applies the same policy and the
  same resolver to each destination the page dials. A rebinding redirect or a
  subresource aimed at loopback, a private range, or the cloud metadata address
  is refused before a socket opens, and that is verified end to end against the
  real browser. What is *not* covered: WebRTC can open UDP flows no HTTP proxy
  carries, and a page that never makes an HTTP(S) request in the first place is
  outside it too. The boundary is "HTTP(S) requests the page makes", which is
  where these attacks live, rather than every socket a browser can open.
- **Free space is checked when it is knowable, and reported as unknown when it is
  not.** A source that omits `Content-Length` gives the application nothing to
  compare against, so no comparison is made — the download proceeds and a genuine
  out-of-space failure is handled when it happens. The check also cannot reserve
  the space: another program can take it between the check and the write.
- **Progress is bounded, not free.** Progress frames are coalesced before they
  cross into the webview, and database writes for progress are throttled, but a
  very large batch still produces a steady stream of both. The queue's rendering
  was measured to stay responsive at the tested batch sizes; it has not been
  profiled at every scale.


- **Combining tracks needs FFmpeg.** Douyin serves video and audio separately, and
  producing one playable file means combining them. With FFmpeg installed this is
  automatic; without it, such a download is refused with a message naming FFmpeg
  rather than producing a silent video. See [Video and audio](#video-and-audio).
- **Pause keeps the bytes; resume usually cannot use them against Douyin.** A
  pause stops the transfer and deliberately keeps its partial file, and a resume
  offers that partial back to the source as an HTTP `Range`. The transfer layer
  validates the response before appending anything — a server that ignores
  `Range` and answers with the whole resource is restarted from zero, never
  appended to. Douyin's media URLs are signed and short-lived, though, so a URL
  that was valid at the pause is usually expired at the resume: the resume is
  honest rather than optimistic, and falls back to a clean restart. Pausing is
  still worth doing — it stops the network immediately and releases its worker
  slot.
- **Clearing or removing never deletes media.** Removing a task from the queue,
  and clearing the completed entries, affect the list only. The downloaded file
  and the history row stay. Removing a task that is still downloading is offered
  as a cancel first, because the bytes have to stop before the row describing
  them can go.
- **Resolution depends on Douyin serving the page to a browser.** It does that
  without a sign-in today — verified repeatedly — but it is a public endpoint
  outside this project's control, and a page that refuses an unauthenticated
  browser is reported as refused.
- **The screens were driven in a browser, not in the packaged desktop window.**
  The React application was loaded from the Vite dev server against a stubbed IPC
  bridge and exercised through the real interactions — resolve a URL, load and
  page a profile, select and submit, search and filter the Library, run a
  download action, answer a confirmation — with the rendered result and the
  commands issued inspected each time. That verifies the screens' own behaviour
  and the shape of the calls they make. It does **not** verify the Rust core
  behind a real webview, which the test suites above cover separately, and the
  Tauri desktop window itself was not hand-driven in this environment. The
  distinction matters: a stubbed bridge proves what the screen asks for, not that
  the real backend answers.
- **Some media hosts refuse this client.** Douyin serves the same track from
  several mirrors, and one of them (`v3-web.douyinvod.com`) answers `403` to a
  non-browser request while `*.zjcdn.com` and the `douyin.com` play endpoint
  accept it — consistent with the CDN behaviour recorded under
  [every HTTP path that was probed](#every-http-path-that-was-probed-for-reference),
  and with the URLs being session-bound. The download therefore tries the post's
  other renditions of the same track before giving up, and a track whose *only*
  rendition is refused fails with a clear error rather than a false success. No
  `Referer`, cookie, or browser-identity header is synthesized to get around the
  refusal.
- **Audio discovery is best-effort.** The audio track is only visible in the
  page's own network traffic, and a page that never starts playback requests
  nothing. That leaves the track layout **unknown**, which the screen reports as
  "audio not checked" — never as a source that has no sound — and the download is
  the video on its own. Watching the network and seeing a track are different
  facts, and the application does not conflate them.
- **A video cannot currently be downloaded without its audio** from the Single URL
  screen: if the source serves them apart, the download is the complete file. The
  separate audio-only track remains individually downloadable.
- **Douyin needs a browser *window*, and the app keeps exactly one.** A headless
  browser is answered with a `403` or a `200` whose body is zero bytes; an
  ordinary window gets the real listing. The application therefore renders Douyin
  pages in a real window — positioned **off the visible desktop**, because Chrome
  ignores `--start-minimized` and off-screen is what actually keeps it out of the
  way — and reuses **one** browser for the whole session rather than starting one
  per operation. Measured live: a profile listing (creator, then grid), a second
  page, and a post resolution — four renders — shared one process, and concurrent
  requests queue behind it rather than racing to start their own. The window is
  still a fresh throwaway profile; it never reads the user's Chrome or Edge
  profile, never forges a signature, and never signs in. Set
  `DOUYIN_DOWNLOADER_BROWSER_WINDOW=visible` to put it on screen instead.
- **The page's own refresh, and one more look, absorb Douyin's flakiness.** The
  first listing request routinely fails — the page says so itself —
  and its `刷新` control is what recovers it, so the wait presses that at most
  twice. When even that is not enough, the resolver takes the page once more,
  which is what a person would do next; a source that refuses twice is then
  believed rather than asked again.
- **Douyin's media CDN refuses some renditions with `403`.** A track with another
  rendition falls back and succeeds; a track whose *only* rendition is refused
  fails with that reason and leaves nothing behind, rather than producing a
  silent video or an empty file. See [Video and audio](#video-and-audio).
- **A listing page costs two browser windows.** Listing a page resolves the
  creator and then reads the grid, and each is a full browser launch. The
  interface is unchanged from Phase 01, so this is deliberate, but it is worth
  knowing that loading a page is slower than it looks.
- **Douyin's profile markup is read structurally, not by a fixed selector.** The
  collector anchors on the page's own `data-e2e` markers (`user-info`, the works
  tab, the count rows) with text fallbacks, and treats the numeric abbreviations
  the page uses (`72.0K`, `1.2万`) as numbers. A field the page does not publish
  stays absent; none is inferred.
- **Queue management is Phase 04.** Bulk submission creates ordinary download
  tasks; the worker pool, pause/resume with byte ranges, and restart recovery are
  not part of this phase.
- Pause stops the transfer and `resume` restarts it from the beginning; byte-range
  resume is deferred (the `resume_from` field is already in the transfer
  request).

## Browser-assisted resolution

Douyin's post pages do not contain the post. Its share pages server-render only
the page's own configuration, and the JSON endpoints answer `403` or an empty
body carrying a risk-control verdict of `强制登录` ("forced login"). The post's
media arrives from a signed, session-bound endpoint that only the page's own
JavaScript can call — so the only way to see what the page is given is to let a
browser run it.

The resolver does that as a **fallback, never a default**:

```text
resolve post
   ├─ HTTP: the served pages carry the post   → normalize            (via http)
   └─ the pages responded, but held no post   → render in a browser   (via browser)
        ├─ the player exposes media          → normalize            (via browser)
        └─ no browser, or no media           → the typed error from the HTTP step
```

The browser resolves; **it never downloads.** It hands back signed URLs that the
ordinary HTTP pipeline fetches, so there is one downloader, one storage layer,
and one history.

### What the browser does and does not do

- It opens the same public page a person would, in a **fresh temporary profile**
  with **no sign-in**. The user's own browser profile, cookies, and sessions are
  never opened, read, or copied.
- It does not sign in, solve a challenge, or forge a request signature. A page
  that refuses an unauthenticated browser is reported as refused.
- It is on by default and can be switched off in **Settings → Advanced →
  Browser-assisted resolution**. With it off, resolution is limited to the HTTP
  path and says so; the resolver then reports what the HTTP path actually found
  rather than complaining about a browser the user never asked for.
- It is bounded three ways — start-up, page readiness, and a total budget — and
  the process *and* its temporary profile are removed on every path: success,
  failure, timeout, or a cancelled request.

### How the media is read

The page's XGPlayer instance holds it in `player.config.url`. That is the only
place the signed URLs appear — not in the markup, not in the page's embedded
state. The extractor reads it defensively, keeps only `http(s)` URLs, and never
invents a URL or a quality tier the player did not list. A missing or malformed
player is a typed error, never a guess.

The URLs are **signed and time-limited** and bound to the session that obtained
them, so they are used immediately and never cached or stored.

### Every HTTP path that was probed, for reference

Every public path to a post's media was probed directly. The results, for
`https://v.douyin.com/7yyhouNSzvA/` → `aweme_id 7684462711358426420`:

| Request | Status | Content | Post data? |
| --- | --- | --- | --- |
| `v.douyin.com/{code}` | 302 | — | redirect to a landing URL (varies by request: a `share/video/…` URL or `douyin.com/video/…?previous_page=web_code_link`) |
| `iesdouyin.com/share/video/{id}/` (mobile UA) | 200 | `text/html`, ~32 KB | `_ROUTER_DATA` present, `loaderData` = `{video_layout, video_(id)/page}` — **page configuration only**, no `videoInfoRes` |
| the same URL including the server-issued `share_sign`/`from_ssr` params | 200 | `text/html`, ~34 KB | same: configuration only |
| `m.douyin.com/share/video/{id}` | 200 | `text/html`, ~32 KB | same |
| `douyin.com/video/{id}` (any browser UA) | 200 | `text/html`, ~72 KB | anti-bot JavaScript VM shell; **no state blob at all** |
| `douyin.com/video/{id}` (no `User-Agent` header) | 200 | `text/html`, ~815 KB | `RENDER_DATA` present, but it decodes to the PC app shell's configuration (`tccConfig`, `backendAbTest`, `abFormatData`) — the video id appears only as a route parameter `{"params":{"id":"…"}}` |
| `iesdouyin.com/aweme/v1/web/aweme/detail/` | **403** | `text/plain`, 7 B | body `blocked` — refused at the CDN edge (`x-alicdn-da-ups-status: endOs,0,403`) |
| `iesdouyin.com/aweme/v1/web/aweme/post/` | **403** | same | same |
| `douyin.com/aweme/v1/web/aweme/detail/` | 200 | `text/plain`, **0 B** | risk-control abort |
| `api.amemv.com`, `aweme.snssdk.com`, `douyin.com/web/api/v2/…/iteminfo/` | 200 | **0 B** | risk-control abort |
| `douyin.com/oembed`, `m.douyin.com/video/{id}` | 404 | — | no such endpoint |

The 0-byte responses are the informative ones. They carry:

```text
X-Whale-Throughput-Abort-Data: {"content":"anonymous","id":53,"name":"强制登录","post_action":"[]","source":"namespace"}
```

`强制登录` is "forced login". That is ByteDance's risk engine answering the
question directly: the web API served an empty body because the request was
anonymous. Two separate access controls are in play — a CDN/WAF block on
`iesdouyin.com` from this network, and an identity requirement on `douyin.com`'s
API.

Neither is reachable from an unsigned HTTP request. This application does not
collect credentials and does not defeat anti-bot protections; what it does instead
is let a browser be a browser, and read only what the page is publicly given.

## Video and audio

Douyin's player streams a video and its audio as **two separate renditions**. The
player's configuration names some of them and the page fetches the rest itself,
and the audio is in the second group — which is why reading the player alone
produced a video with no sound.

**How the audio is found.** While the page renders, the browser reports the
network activity to the application (via the DevTools Network domain, kept to a
small bounded window with no response bodies). A track URL is visible in the
request itself, so watching costs a page's worth of small records rather than its
traffic. Only Douyin's own track paths are treated as tracks, so an advertisement
or an unrelated clip that happens to be a video cannot become a rendition of the
post. This is why the fix is not "add FFmpeg": without the audio URL there is
nothing to combine.

**What the screen tells you.** Each media item carries a track layout, and the
three cases are kept apart deliberately:

| Badge | Meaning |
| --- | --- |
| **video + audio** | both tracks were found; the download will be complete |
| **video only — source has no audio** | the page's traffic was watched *and the page actually fetched a track*, none of which was audio |
| **audio not checked** | no track request was observed — the traffic was not watched, or the page never fetched its media — so nothing about the tracks was established |

The last two are not the same statement, and the application does not present one
as the other. In particular, watching the network and seeing a track are different
facts: a page that never started playback requests nothing, and that silence is
reported as "not checked" rather than as a source that has no sound.

**Combining.** The two tracks are downloaded by the ordinary HTTP pipeline —
browser resolution still never transfers media — and then combined with FFmpeg
into one MP4. The streams are **copied** (`-c copy`): same codec, same bitrate,
same pixels, no filter applied.

**A file is not a result.** The combined output is read back before the task is
marked complete, and a file that is missing the streams that were asked for is a
failure however plausible it looks.

**FFmpeg is required for this, and only this.** A download whose audio is a
separate track is refused with a clear error naming FFmpeg when it is not
installed — never silently downgraded to a silent video. The check happens before
the download starts, so the refusal costs nothing. Point the application at a
specific build with `DOUYIN_DOWNLOADER_FFMPEG`; otherwise it looks on `PATH` and
in the usual install locations, the same way it finds a browser.

```bash
winget install Gyan.FFmpeg
```

## Scope

- Only **publicly accessible** Douyin content is in scope.
- The application does not bypass authentication, private-content restrictions,
  access controls, or platform security protections. The mobile user agent it
  presents when fetching a share page is a *request format* the page is built to
  serve, not an access-control bypass; the same public page is requested.
- **Watermark removal is not implemented and will not be.** Douyin's playback and
  watermarked renditions are told apart by a URL path marker, and that marker is
  *classified*, never rewritten. A watermarked rendition is offered labelled as
  watermarked; a clean source is offered as clean when the platform provides one.
  No media is ever post-processed to remove a watermark.
- Douyin requests read the share page for a post, and the redirect for a short
  link. Response bodies are never logged, and secrets, session tokens, and
  cookies never enter a log line.
- **Browser-assisted resolution is not a bypass.** It renders the same public page
  a person would open, in a fresh throwaway profile with no sign-in, and reads
  only what that page is given. It never opens, reads, or copies the user's own
  browser profile, and it neither signs in nor solves a challenge nor forges a
  request signature. What it does with what it finds is hand a URL to the
  ordinary downloader — the browser itself transfers nothing.
- The `Demo mode` fixture resolver (below) is registered only when explicitly
  enabled, and only answers for its own two URLs. It is never on the live path.

## Test URLs

Two real Douyin links ship as fixtures. They are defined once in Rust
(`src-tauri/src/resolvers/fixture.rs`) and reach the UI through the
`get_test_fixtures` command — they are never hard-coded in TypeScript.

| Purpose | URL |
| --- | --- |
| Single video post | `https://v.douyin.com/7yyhouNSzvA/` |
| Creator profile | `https://v.douyin.com/SMt7NsUPaGo/` |

Both are short links, so they cannot be told apart until the redirect is
followed; each fixture therefore declares which screen it belongs on.

Fixtures drive the URL validation, URL parsing, platform identification, and
error-handling tests, and back the "Project test links" quick-fill buttons on the
Single URL and Profile Downloader screens.

## Environment variables

| Variable | Effect |
| --- | --- |
| `DOUYIN_DOWNLOADER_DATA_DIR` | Override the data directory (database, logs). Used by tests. |
| `DOUYIN_DOWNLOADER_DEMO` | Set to `1` to register the in-memory fixture resolver ahead of the live one, so the resolve flow returns sample data offline. Clearly separated from the production path: it answers only for its own two URLs, and its media URLs point at `cdn.example`. |
| `DOUYIN_DOWNLOADER_BROWSER` | Full path to a Chrome or Edge executable to use for browser-assisted resolution, when it is not in one of the usual places. Checked before the built-in locations. |
| `DOUYIN_DOWNLOADER_BROWSER_WINDOW` | Set to `visible` (or `1`) to place the managed browser window on screen instead of off it. Off-screen is the default; the override exists so a machine where the off-screen position behaves differently can be put back without a rebuild. |
| `DOUYIN_DOWNLOADER_FFMPEG` | Full path to an `ffmpeg` executable, when it is not on `PATH` or in the usual install locations. `ffprobe` is looked for beside it. |
