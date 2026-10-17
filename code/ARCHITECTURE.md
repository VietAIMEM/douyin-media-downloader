# Architecture

Douyin Downloader is a Tauri 2 desktop application. This document describes the
layers, the boundaries between them, and the decisions that later phases are
expected to build on without restructuring.

Phase 01 delivered the shell, the command layer, persistence, settings, error
handling, logging, and the resolver interfaces, with every path that would leave
the machine deferred behind a typed error. Phase 02 filled in the first real
workflow — Single URL analysis and download — by extending those interfaces
rather than replacing them.

## Layers

```
React UI (src/)
    |  invoke("command", args)            // no Douyin knowledge in TypeScript
    |  listen("download://event")          // one live download stream
    v
Tauri commands (src-tauri/src/commands/)
    |  validate, convert errors, serialize
    v
Application services (src-tauri/src/services/)
    |  AppCore (composition root) + single_url (the URL->download workflow)
    |  profile (listing + bulk submit) + library (local reads + export)
    |
    +-- Resolver layer   (resolvers/)   ProfileResolver, PostResolver, MediaResolver
    |                                   douyin/{mod,extract,player} — real resolution
    +-- Download layer   (downloads/)   DownloadManager + TransferWorker + history
    +-- Media layer      (media/)       MediaMuxer — combining tracks with FFmpeg
    +-- Storage layer    (storage/)     StorageManager
    +-- Settings layer   (settings/)    SettingsService
    +-- Network layer    (network/)     HttpClient (the only reqwest) + PageRenderer
    |                                   policy/ — which destinations may be contacted
    +-- Database layer   (database/)    Database + repositories
```

The dependency arrow only ever points down. The UI never sees a SQLite error, a
reqwest error, or a raw Douyin response shape; it sees a serialized error with a
stable `kind`/`code` pair and a user-safe `message`.

### Why this layout instead of `src/frontend` + `src/backend`

The brief sketched `src/frontend/{pages,components,features,stores,services}` and
`src/backend/{models,resolvers,downloads,database,storage,settings,errors}`. The
delivered layout keeps every one of those responsibilities but splits them the
way Tauri requires:

| Sketched | Delivered | Why |
| --- | --- | --- |
| `src/frontend/*` | `src/*` | Vite owns `src/` as the web root; the frontend is the only thing there, so a `frontend/` level is redundant. |
| `src/backend/*` | `src-tauri/src/*` | The Rust core must be its own Cargo package. `src-tauri/` is that package, and the Tauri CLI expects it. |
| `src/backend/models` | `src-tauri/src/models/` | Unchanged in substance. |
| — | `src-tauri/src/commands/` | New: the IPC boundary is a layer, not an afterthought. |
| — | `src-tauri/src/services/` | New: the composition root (`AppCore`) that wires the layers and owns startup ordering. |

Module responsibilities are unchanged; only the directory names differ, and they
differ because the toolchain fixes them.

## Rust core

### Composition root — `services/mod.rs`

`AppCore::bootstrap(AppPaths)` is the single place the application is assembled,
in order: logging → paths → database → migrations → repositories → settings →
HTTP client → resolver registry → storage → download manager + transfer worker.
It returns an `AppCore` plus the `WorkerGuard` that keeps the log appender alive.

`bootstrap_with(paths, http_override)` is the same construction with the HTTP
transport substituted. It exists because the client cannot be built before
settings are read — its timeout comes from them — so an injected transport has to
be applied at the composition root rather than passed in from outside. Tests use
it to drive the whole core over a mock client; it is not a test-only backdoor,
and `bootstrap` is a thin call to it with `None`.

`AppCore` is stored in Tauri's managed state (`app_state.rs`) so commands resolve
it with `State<AppState>` rather than reaching for globals.

### Single URL workflow — `services/single_url.rs`

The one place that knows the *sequence* of the workflow:

```
analyze_url     : URL ─▶ SourceUrl ─▶ Post + MediaItem ─▶ SQLite
create_download : ids ─▶ stored media row ─▶ DownloadRequest ─▶ DownloadManager
```

It sits between the commands and the resolvers so the orchestration is testable
without a Tauri runtime, and so the resolvers stay unaware of the database and
the queue.

Two decisions are worth calling out:

- **`analyze_url` classifies after expansion.** The `UrlClassification` it
  returns describes what the link *resolved to* (`post` / `profile`), while
  `classification.original` keeps exactly what the user pasted. That is why the
  Single URL screen can show both without the frontend knowing anything about
  short links.
- **A profile link is a successful analysis, not a failure.** It returns
  `intent = "profile"` with a `notice` and no post, so the screen can say "this is
  a profile, use the Profile Downloader" instead of treating it as an error or —
  worse — as a downloadable post.
- **`create_download` takes ids, never a URL.** The media location is read from
  the database, which was written by the resolver. A compromised renderer cannot
  point a transfer at an arbitrary address, and TypeScript never learns how
  Douyin encodes media. The URL is additionally re-checked to be `http(s)` before
  it becomes a transfer target.
- **The creator travels as a domain object.** `PostResolver::resolve_post`
  returns only a `Post`, so the normalized `Creator` rides along under a
  documented metadata key (`resolvers::RESOLVED_CREATOR_KEY`) as a serialized
  `Creator` — a domain model, not a payload shape. That keeps the service free of
  platform knowledge while still letting the screen show who posted the content,
  and `analyze_url` persists the creator before the post so the post's foreign
  key resolves.

### Profile workflow — `services/profile.rs`

The second workflow, and the same shape as the first: one module knows the
sequence, and it reaches *down* into the layers Phase 02 built rather than
around them.

```
resolve_profile    : URL ─▶ SourceUrl ─▶ Creator ─▶ SQLite
list_profile_posts : URL + cursor ─▶ PostPage ─▶ posts + creator ─▶ SQLite
submit_downloads   : ids ─▶ stored post ─▶ media resolved ─▶ AppCore::create_download
```

Three decisions are worth calling out:

- **Submission takes ids, never URLs.** The same rule as `create_download`, for
  the same reason: the media location is read from the database, which the
  resolver wrote, so the profile screen cannot become a way to fetch an arbitrary
  address.
- **A batch is not all-or-nothing.** Each selected post is resolved and submitted
  on its own, and its outcome is recorded per post. One unsupported post, one
  source that will not answer, one video whose audio needs an FFmpeg that is not
  installed — each fails alone, and the rest of the batch proceeds. The summary
  reports `selected` / `resolved` / `tasksCreated` / `skipped` / `failed`
  separately, because creating a task is not the same as downloading media.
- **Local status is computed, not stored.** `local_status` folds the source's
  availability together with the post's latest download row and whether the file
  that row names is actually on disk. A completed row whose file is gone reads as
  `ready`, not `downloaded` — a row is not a file. A post the source reports as
  unavailable stays unavailable however successful its history.
- **Ending a listing is a claim, so it needs evidence.** An empty grid means one
  of two very different things, and the profile's own declared post count is what
  separates them: if it names posts further on than have been read, the listing
  did not end — it was withheld, and that is a typed `UnavailableContent` rather
  than a quiet "no more posts". The page's own service-error banner counts as
  evidence too, and outranks the count: when the site says its listing request
  failed, the resolver never reports the listing as finished, whatever the count
  says. Only when the declared count is reached, or when there is no count and no
  banner and earlier pages read successfully, is a listing reported as complete.

  The banner is carried on the collected payload (`pageError`) as a *diagnostic*
  — it is what lets the error's technical detail quote the source's own words
  instead of asserting an inferred cause. It never changes what the grid means;
  it corroborates it.
- **Some pages are served only to a window.** `RenderRequest::visibility` lets a
  caller ask for an ordinary browser window instead of a headless one, and both
  Douyin pages this resolver renders now ask for it. Verified against the live
  site: the identical request is refused headlessly — the listing endpoint
  answers `403` or a zero-byte `200`, and a video page is redirected to the feed
  with no player at all. Nothing else about the launch changes, so this is not a
  disguise; it is the page's actual requirement.
- **The profile's own refresh control is the retry.** Even in a window the first
  listing request usually fails. The page says so (`服务异常，重新刷新拉取数据`)
  and puts a clickable `刷新` control in that message, and pressing it makes the
  page request the listing again. The settle predicate that waits for the grid
  therefore also presses that control — at most twice per request, tracked by a
  counter in the page, inside the one browser session the render already paid
  for. No parameter is re-signed or replayed by the application; it triggers the
  page's own request, as a person would.

### The profile screen

`pages/ProfileDownloaderPage.tsx` browses a creator's posts a page at a time and
submits a selection. Two things it is built around, because both are easy to get
subtly wrong:

- **Selection is keyed by post id, not by list position.** Filtering and paging
  therefore cannot move a tick onto a different post, and "select all visible" is
  never silently "select every post the creator has, including the ones that have
  not been loaded".
- **A stale response is discarded.** Switching profiles while a request is in
  flight must not pour the previous creator's posts onto the new screen. The
  screen also refuses a second page request while one is in flight, and a failed
  page leaves everything already loaded exactly as it was.

Live download events tighten the loop: a task the screen created is mapped back
to its post, so a row moves from `downloading` to `downloaded` or `failed` when
the transfer actually finishes rather than when the page is reloaded.

### Resolver layer — `resolvers/`

Three capability traits plus a URL parser:

| Interface | Responsibility |
| --- | --- |
| `ProfileResolver` | `resolve_profile(url) -> Creator`, `list_posts(creator, PageRequest) -> PostPage` |
| `PostResolver` | `resolve_post(url) -> Post` |
| `MediaResolver` | `resolve_media(post) -> Vec<MediaItem>` |
| `SourceUrlParser` | `parse(raw) -> SourceUrl`, `expand(raw, http) -> SourceUrl` |

`ResolverRegistry` holds `Arc<dyn ...>` for each, selects a parser by asking each
one in turn, and dispatches by platform. `describe()` reports what is registered,
which is what the Home screen renders — the UI's resolver list is the real
registry, not a hard-coded array.

Implemented:

- `DouyinUrlParser` — real: host matching, short-link/profile/post classification,
  ID validation, canonicalization. No network for classification; `expand`
  follows the redirect (and reads no body).
- `FixtureResolver` — a real implementation of all three resolver traits backed by
  in-memory data for the two shipped test URLs. Registered only when
  `DOUYIN_DOWNLOADER_DEMO=1`, and always available to tests.
- `DouyinResolver` — real post and media resolution (Phase 02) and real profile
  resolution and listing (Phase 03).

### Douyin post resolution — `resolvers/douyin/`

A Douyin page embeds its state in one of several blobs, and which one — and
whether it carries the post at all — depends on the request. So the resolver is
built to be told, not to assume:

1. **expand** the short link (redirect only), keeping the landing URL verbatim
   on `SourceUrl::resolved_url`;
2. **fetch** that landing URL first — the parameters on it (share signature,
   render flag) were issued and are checked by Douyin, and a URL rebuilt by hand
   is not the request the platform invited. The known share hosts are tried
   afterwards, so a pasted concrete post URL still works;
3. **extract** the post from *any* recognised state blob, by shape.

`extract.rs` is pure — bytes in, domain models out — which is what makes it
fully testable against a saved payload without touching the network.

**Blob-agnostic extraction.** `find_aweme_in_page` reads `_ROUTER_DATA` (plain
JSON assigned to a global), `RENDER_DATA` (percent-encoded JSON in a script tag),
`__NEXT_DATA__`, and `__INITIAL_STATE__`. It first tries the structural locations
Douyin is known to use, then falls back to a depth-bounded search for an object
that *looks like a post* — `aweme_id` plus a `video`, `images`, or `music` field.
Keying on shape rather than on a path is what makes a schema move survivable
without a code change, and requiring a media field keeps a bare id reference from
being mistaken for the post.

The brace matcher that locates the JSON is string-literal aware, because captions
routinely contain `{` and `}`.

**Four outcomes, kept distinct**, because they mean different things to a user:

| Situation | Kind |
| --- | --- |
| No state blob at all (the anti-bot VM shell) | `Resolution` — a page this build cannot read |
| A blob is present but holds no post | `UnavailableContent` — the source declined to include it |
| A post is present but has no media | `UnsupportedContent` — nothing downloadable |
| The request itself failed | `Network` |

When a page loads but yields no post, the failure is annotated with what the page
*actually contained* — its size, which blobs were present, how many parsed,
whether any post-bearing key appeared, and the loader sections — and logged at
`warn`. That is what makes "the source returned no post" an evidenced statement
rather than a guess, and it is the first thing to read if Douyin changes shape
again.

When several candidates are tried and all fail, the *most specific* error wins
(`informative_rank`), so a content answer is not masked by a transport failure on
the last host attempted.

**Then a browser, if the pages held nothing.** The resolver works in the order
the source requires rather than the order that is convenient:

```
resolve post
   ├─ HTTP: the pages carry the post        → normalize (via http)
   └─ the pages responded, but no post      → render in a browser (via browser)
        ├─ the player exposes media         → normalize (via browser)
        └─ no browser / no media            → the typed error from the HTTP step
```

Only a *content* failure triggers the fallback. A transport failure would fail
the same way in a browser, so rendering is not attempted — that would cost a
browser launch to learn nothing. When no renderer is available the original HTTP
reason is returned rather than a complaint about a browser the user never asked
for. `resolution.via` records which path was taken, and `SingleUrlService` is
unaware of the distinction; it receives a `Post` either way.

**Watermark policy.** `play_addr` and `download_addr` differ by a path marker
(`playwm`). The resolver *classifies* that marker — a watermarked rendition keeps
`is_original == false` and is labelled "watermarked" — and never rewrites the URL
to the clean path. Rewriting is watermark removal, which this application does
not do. A watermarked variant is offered as what it is.

For the current state of Douyin's public endpoints and exactly what each returns,
see the repository README's "Where Douyin withholds the post".

### Network layer — `network/`

`HttpClient` is the only module that knows `reqwest` exists. Phase 01 needed one
capability (follow a redirect); Phase 02 added two more:

| Method | Used by |
| --- | --- |
| `resolve_redirect` | short-link expansion — reads the redirect, never the body |
| `fetch_text` | share-page retrieval — bounded by `max_bytes`, mobile-UA profile |
| `download` | the transfer worker — streams to a file, reports progress, polls a `CancellationHandle` |

**A ranged transfer is validated, never assumed.** `TransferRequest::resume_from`
becomes a `Range` header, and the response has to earn the right to be appended:
it must be `206`, its `Content-Range` must start at the offset that was asked
for, and the resource must not be shorter than what is already on disk. Anything
else — a `200` because the server ignored the range, a different start offset, a
`416` — discards the partial and fetches the resource from the beginning. A
server that ignores `Range` answering with the whole body is the case that
matters: appending would duplicate everything before the offset, and the file
would be twice as long as the resource and wrong from the first byte.

`CancellationHandle` is a shared `Arc<AtomicBool>` the transfer loop checks
between chunks, which is what makes cancellation stop the socket rather than
only the bookkeeping. `download` uses `Response::chunk()` rather than `text()`
so the size cap and cancellation apply while reading, and writes with
`write_all` + `sync_all`.

Failures are classified at the boundary: retryability is decided from the HTTP
status (408/429/5xx retryable; other 4xx permanent) or the transport error shape,
and carried on `AppError::is_retryable()` so the worker does not have to
re-interpret an error to decide whether to try again. Page fetches and media
transfers map statuses differently — a 404 page is `UnavailableContent`, a 404
media resource is an untryable `Download` failure.

### Browser-assisted resolution — `network/renderer.rs`, `network/chromium/`

Some public pages assemble their content with JavaScript and only then ask their
own server for it, using request parameters a plain HTTP client cannot produce.
For those pages the only way to see what the page itself is given is to let a
real browser run it. Douyin's post pages are one of them.

```
DouyinResolver
      │  only when the served pages contain no post
      ▼
PageRenderer                     (network/renderer.rs — the seam)
      ▼
ChromiumRenderer                 (network/chromium/mod.rs — process + lifecycle)
      ▼
cdp::CdpClient                   (network/chromium/cdp.rs — the protocol)

...and every browser it launches sends all of its traffic through

proxy::GuardedProxy              (network/chromium/proxy.rs — the network boundary)
```

**The boundary.** `PageRenderer` renders one public URL, waits for a condition
the caller names, runs one collection script, and returns normalized
`MediaSource`s. It knows nothing about Douyin. The resolver knows nothing about
processes, ports, or the DevTools protocol — it supplies the JavaScript (which is
platform knowledge) and receives media sources. That is why the CDP module is
private to `chromium`, and why `RenderedPage`/`MediaSource` exist at all: no raw
protocol structure crosses the seam.

**`ChromiumRenderer`** discovers an installed browser — an explicit path
(`DOUYIN_DOWNLOADER_BROWSER`), then Chrome's usual locations, then Edge's, then
`PATH` — launches it against a **fresh temporary profile**, drives it over CDP,
and tears it down. It never opens, reads, or copies the user's own browser
profile, so no real cookies or sessions can reach the page. Nothing about the
session is persisted.

The one thing the launch varies is whether the browser has a **window**, because
some public pages serve their content to an ordinary window and refuse the same
request headlessly. `RenderRequest::visibility` carries that choice; headless is
the default and every caller that does not ask for a window is unaffected. A
windowed browser is positioned **off the visible desktop** — it is a real window,
because that is what the page requires, but there is no reason for it to be in
the user's way. Chrome ignores `--start-minimized`, so off-screen is what
actually achieves that; `DOUYIN_DOWNLOADER_BROWSER_WINDOW=visible` puts it back
on screen. It is not a disguise — nothing else about the launch changes, and the
page is the same public URL in the same throwaway profile.

**One browser, kept for the session.** Starting a browser per render is what put
a window on screen for every profile page, every page of a listing, and every
post resolved for download. `ChromiumRenderer` therefore holds a
`BrowserSession` — the process, the CDP client, and the profile — and navigates
that one browser as often as it needs to. Measured: a profile listing (two
renders: the creator, then the grid), a second page, and a post resolution — four
renders — all shared **one** browser process; and three renders and two
concurrent renders each measured one launch.

The rules that keep it safe:

- **Serialized.** The gate that always made renders one-at-a-time now also means
  two concurrent callers queue behind the same browser instead of racing to
  start their own.
- **A dirty session is never reused.** `clean` is cleared for the duration of a
  render and set again only when the render completed. A session whose render was
  cancelled, timed out, or failed is replaced, because driving a browser whose
  page and command stream are in an unknown state is how a render silently
  collects the wrong page.
- **A reused session gets one free retry.** If a kept session fails, it may
  simply be dead — the browser crashed, or something outside the application
  closed it. That is not the caller's problem, so the session is replaced and the
  render tried once more on a browser that is certainly alive. A live test kills
  the app's browser by its own pid and asserts the next render recovers.
- **The pid is known.** `session_pid()` names the process this application owns,
  so anything acting on it — a diagnostic, a test simulating a crash — never has
  to reach for every `chrome.exe` on the machine.
- **Cleanup is explicit and on drop.** Replacing or discarding a session closes
  the socket, terminates the process, and removes the profile, in that order; and
  `BrowserProcess::Drop` still covers the paths that skip it. A live test asserts
  no profile outlives the renderer that owned it.

**Bounds and cleanup.** Three budgets apply — start-up, the wait for the page's
own readiness signal, and the total. A `BrowserProcess` guard terminates the
process (and its tree) and removes the profile when dropped, which covers the
success, failure, timeout, cancelled-future, and panic paths alike. The renderer
also refuses to start a browser when `PageRenderer::is_available()` is false, so
turning the feature off costs nothing.

**A page that is not ready is retried once.** A public page occasionally serves a
visitor a challenge or a half-built shell and works on the next visit. A second
look at the same public content is a legitimate retry — and the error is marked
`retryable` only for that specific case, so a browser that would not start is not
retried. When a page never becomes ready, the failure carries a structural
diagnostic (which landmarks the page had, never its content) so the reason is
visible in the log.

**The browser never downloads.** It resolves a signed URL; the ordinary HTTP
pipeline fetches it. This is a hard rule: `PageRenderer` has no method that
writes media bytes, so the browser cannot become a second downloader by accident.

**Signed URLs are short-lived.** Douyin's media URLs carry an expiry and a
signature bound to the session that obtained them, so they are used immediately
and never cached. A URL resolved now is not stored anywhere for later.

**A profile page is not a post page.** A profile's listing request
(`/aweme/v1/web/aweme/post/`) is refused for an anonymous visitor with
`403 Blocked by ArgusSecurityPlugin Sign Invalid` — a signature check on the
request the *page itself* makes. The page renders its header happily and the grid
stays empty, which is why `resolve_profile` succeeds and `list_posts` reports a
typed `UnavailableContent`: the two are genuinely different answers and the
screen says so. Forging the signature would be defeating an access control, and
signing in on the user's behalf is not something this application does.

**The renderer can return the page's own payload.** `RenderedPage::payload`
carries the raw JSON the collection script produced, next to the normalized
`media` view of it. A platform may ask a page for more than its media — a
profile's identity and post grid — and interpreting that is the resolver's job,
so the renderer hands the payload over rather than learning what a creator is.

**Which anchors are the creator's posts is decided in Rust.** A profile page is
full of `/video/…` links: site navigation, a footer of unrelated
recommendations, and search-engine feed cards all carry them, and every one of
those is a different creator's work. The collection script therefore reports
*structure* — which region an anchor sits in, whether it carries an image, what
`source` query parameter it arrived with — and `resolvers/douyin/profile.rs`
decides. That keeps the judgement pure and testable against recorded payloads
instead of against a live page that changes shape without warning.

**Policy lives in the service layer, not the resolver.** `SettingGatedRenderer`
(`services/browser.rs`) wraps the renderer and answers `is_available()` from the
user's `browser_assisted_resolution` setting, read live, so switching the
preference takes effect on the next resolution rather than the next launch. The
resolver asks one question and gets an answer that already accounts for both the
preference and whether a browser exists.

### Separate tracks — `resolvers/douyin/player.rs`, `media/`

Douyin serves a video and its audio as two renditions, not one file. The player
names some of them in its configuration and fetches the rest itself, and the
audio is routinely in the second group — which is why reading the player alone
produced a silent video.

**Finding the audio.** The renderer enables the DevTools Network domain and keeps
a bounded buffer of the events that pass while it drives the page
(`network/chromium/cdp.rs`). No response bodies are requested: a media URL is
visible in the request itself, so watching costs a page's worth of small records
rather than its traffic. `network_media_from_events` turns those events into
unclassified candidates — pure, and therefore testable against recorded events
rather than a live page — and the platform decides what they are: only Douyin's
own track paths (`media-video…`, `media-audio…`) are promoted to tracks, so an
advertisement or an unrelated clip that happens to be video cannot become a
rendition of the post.

The player's list leads and the network's additions follow, so a track the source
ranked is never displaced by one the network merely saw a request for.

**Knowing what was found.** `TrackLayout` records what an item has, and it
distinguishes three situations that are easy to conflate:

| | meaning |
| --- | --- |
| `AudioVideo` | both tracks were found; a download can be complete |
| `VideoOnly` | the traffic was watched, the page **actually fetched a track**, and none of them was audio |
| `Unknown` | nothing was established — the network was not watched, or the page requested no track at all |

Watching the network and seeing a track are different facts, so `RenderedPage`
carries `network_captured` *and* the media observed on it. A page that never
started playback requests nothing, and that silence must not become a claim that
the source has no sound — it is `Unknown`, which the screen shows as "audio not
checked". `TrackEvidence` (`resolvers/douyin/player.rs`) is the small type that
keeps the three cases apart, and `apply_track_layout` takes it instead of a bare
boolean so the weaker input cannot be passed by mistake.

**Combining them.** `MediaMuxer` (`media/`) is the seam; `FfmpegMuxer` is the
only implementation, and the download layer knows nothing about processes.
FFmpeg is used because building a correct MP4 from two independently encoded
streams — sample tables, interleaving, timestamps — is a large amount of subtle
work that the brief explicitly sanctions a tool for. The streams are **copied**
(`-c copy`), never re-encoded and never filtered, so the video that comes out is
the video that went in.

**A file is not a result.** After a mux, the output is read back with `ffprobe`
(or `ffmpeg`'s own report when only that is installed) and the task is complete
only when the streams that were asked for are actually there. A mux that
"succeeds" into a file with no audio is a failure, and the bytes-level
`ftyp`/`moov` check used in tests is only a second opinion — a demuxer is the
authority.

**When FFmpeg is missing**, a download that needs two tracks is refused with a
`MuxerUnavailable` error naming it. It is never quietly downgraded to a silent
video: the user asked for the video, and handing them something that looks like
it but has no sound is worse than saying the tool is missing. The check happens
before the download starts, so the refusal costs nothing, and again inside the
worker, so a retry after the tool is removed also fails cleanly.

### Download layer — `downloads/`

`DownloadManager` is a complete lifecycle interface: `enqueue`, `get`, `list`,
`cancel`, `pause`, `resume`, `retry`, `remove`, `clear_completed`, `retry_failed`,
`schedule_pending`, `summary`, and `subscribe` (a `tokio::sync::broadcast` of
`DownloadEvent`). `QueueDownloadManager` implements it against SQLite, including
the state machine (`Queued → Downloading → Completed`, with `Paused`, `Failed`,
`Cancelled` as the other terminals), per-state counters, and
`recover_interrupted()`.

Phase 04 replaced the single-flight worker with the centralized scheduler the
phase called for, without moving the interface:

```
enqueue / resume / retry ──▶ QueueDownloadManager   state machine, persistence
                                   │                 transitions, event channel
                                   ▼
                            submit(id) ──▶ queue
                                   │
                          TransferWorker::dispatch_loop
                                   │
                     ┌─────────────┴─────────────┐
                     ▼                           ▼
              a free slot?              re-read the limit
                     │                    (so a change applies live)
                     ▼
          one Tokio task per running transfer ──▶ HttpClient + StorageManager
                                              ──▶ MediaMuxer + probe
                                              ──▶ Database + history
```

The decisions that matter:

- **Submitting makes a task eligible; it does not spawn it.** A queue of ten
  thousand tasks is ten thousand ids in a `VecDeque` — not ten thousand Tokio
  tasks parked on a semaphore, which is what "bounded worker pool" has to mean to
  be true. The only Tokio tasks that exist are the transfers actually running,
  plus the dispatcher.
- **Concurrency is read live.** The limit comes from `max_concurrent_downloads`
  on every dispatch, so raising it starts waiting work immediately and lowering
  it stops new work starting — without killing anything already transferring.
- **A task's own id rides the queue, and its persisted state decides.** The
  dispatcher re-reads the row before starting a transfer, so a task that was
  paused, cancelled, or removed while it waited is simply skipped. An id whose
  previous transfer is still unwinding goes *back* in the queue rather than away,
  which is what lets a resume that arrives mid-unwind still run.
- **The dispatcher starts lazily.** The worker is built while the application is
  still being assembled, where there is no runtime to spawn on; the loop starts
  the first time there is work, and `lib.rs` kicks it once the app is up so
  persisted queue state is picked up after a restart.
- `QueueDownloadManager::new` gives the state-only manager the Phase 01 tests
  use; `with_worker` attaches the transfer worker. The interface is identical.
- Progress is coalesced to one broadcast every 200 ms by `TaskProgress`, and
  progress is *not* written to SQLite per chunk: the row is updated on state
  transitions, which is what keeps a fast transfer from hammering the database.
- A failed attempt removes its `.part` file; a successful one is moved into place
  by `StorageManager::finalize`, so a partial file never appears as a result.
- `pause` stops the transfer and keeps what it has written; `resume` puts the
  task back under the scheduler and offers the partial back to the source as an
  HTTP `Range`. The two stops are different instructions and are modelled as
  such — a *cancel* removes the partial, a *pause* deliberately keeps it —
  because a pause that discarded its own bytes would make the resume a restart
  wearing a resume's name.
- The worker re-reads a task's state before applying a terminal transition, so a
  cancel or pause that lands while a transfer is finishing wins.

`downloads/history.rs` writes the one history row per task. It is idempotent per
task (`history_repo::exists_for_task`), because either half of the pipeline can
reach the terminal state — the worker for an in-flight task, the manager for one
cancelled before it started.

### Storage layer — `storage/`

`StorageManager` owns path decisions and filesystem writes. `filenames.rs` holds
the template engine (`{creator}`, `{date}`, `{ext}`, ...), the Windows-illegal
character rules, and the conflict policy (`Rename` / `Overwrite` / `Skip`).

Security rules enforced here, not in the UI:

- Every resolved path is checked to be inside the configured download directory
  before anything is written, so a template cannot escape via `..`.
- Templates are validated before they are saved as settings, so a bad template is
  rejected once rather than at every download.
- Paths are absolute and traversal-free.

### Settings layer — `settings/`

`SettingsService` is the only writer of the `settings` table.

**Storage format:** the whole `AppSettings` struct is serialized as a single JSON
document under one key (`app_settings`). That means adding a field to the model
does not require a migration — the blob simply gains a property — and the
inverse: a value written by a newer build is not silently dropped by an older
one. The trade-off (no SQL-level queries over individual preferences) is
irrelevant for a settings blob of this size.

`save()` validates *before* writing, so a rejected value never reaches disk. The
service caches the current settings but `reload()` re-reads from SQLite; startup
always goes through `reload()`.

### Database layer — `database/`

`Database` wraps an r2d2 pool of `rusqlite` connections. Every connection is
initialized with `foreign_keys = ON`, WAL journaling, `synchronous = NORMAL`, and
a busy timeout. `with_conn(f)` is the only way to run a statement, so the pool is
never handed out and connections are always returned.

Repositories (`creators`, `posts`, `media`, `downloads`, `history`, `settings`)
own the SQL; models never build SQL and the UI never sees a `Connection`.

**Migration strategy:** forward-only, versioned, and recorded. Migrations are
plain `.sql` files under `migrations/` at the project root, embedded into the
binary with `include_str!`, and each is applied inside a transaction that also
records it in `schema_migrations` with its description and timestamp. Running
them again is a no-op — a startup over an existing database is verified to not
duplicate rows. Adding a migration is one file plus one entry in the list; there
is no down-migration, matching the fact that this app owns its local file.

Phase 02 added two: `0002_download_extension.sql` adds `downloads.extension`.
The extension belongs to the resolved media variant, not to the URL — Douyin's
playback URLs carry no file extension at all (`.../aweme/v1/play/?video_id=…`),
so recovering it from the URL would be guesswork. Existing rows are unaffected:
`NULL` means "unknown" and the naming layer falls back to a per-media-type
default.

`0003_history_unique_task.sql` makes "one history row per task" a unique index
rather than a convention. Both halves of the download pipeline can finish a task
— the worker when a transfer stops, the manager when a task never started — and a
check-then-insert loses that race: two writers each see no row and each add one.
The index lets the insert itself be a no-op (`ON CONFLICT (task_id) DO NOTHING`),
so the outcome is the same whichever writer arrives first. `task_id` stays
nullable, because a history row may outlive the task it came from, and SQLite
treats NULLs as distinct.

`Database::status()` is what the Diagnostics card renders: health, location,
schema version against the newest migration, journal mode, whether foreign keys
are enforced, and per-table row counts.

### Error model — `errors/`

One error type, `AppError`, carrying two audiences at once:

| Field | Audience | Example |
| --- | --- | --- |
| `kind` / `code` | UI branching | `UnsupportedContent` / `unsupported_content` |
| `message` | logs | "unsupported host `example.com`" |
| `user_message` | the screen | "Only Douyin links are supported." |
| `technical_detail` | developer, debug builds only | `host=example.com` |
| `retryable` | the retry scheduler | `false` for a `404`, `true` for a timeout |
| `source` | error chain | the underlying `rusqlite`/`io`/`reqwest` error |

Kinds: `Validation`, `Network`, `Resolution`, `Download`, `Storage`, `Database`,
`UnsupportedContent`, `UnavailableContent`, `BrowserUnavailable`, `NotFound`,
`Unknown`. Several map from `?` via `From`, so `rusqlite::Error` becomes
`Database` rather than leaking as `Unknown`.

`BrowserUnavailable` exists because "this content needs a browser and there is not
one" is something a user can act on — install Chrome or Edge — and it is not the
same statement as "the content is gone". Collapsing it into a generic error would
have hidden the only remedy.

`retryable` defaults from the kind (`Network`/`Download` yes; everything else no)
and can be overridden per call site with `with_retryable`. That is what lets a
`404` be permanent even though transfers in general are retryable, without the
worker re-deriving the decision from an error string.

`to_serialized()` produces the wire shape `{ kind, code, message, details? }`.
`details` is populated only under `cfg!(debug_assertions)` and is omitted
entirely (not `null`) in release, so internal detail cannot ship to a user. A
user message is never derived from raw input: validation failures set
`user_message` explicitly instead of echoing what was typed.

`to_command_error()` is the single conversion every command uses on its error
path.

### Commands — `commands/`

Thin adapters: unwrap `State<AppState>`, call exactly one core operation, convert
the error. No network, parsing, database, or filesystem logic lives here.

| Command | Delegates to |
| --- | --- |
| `classify_url`, `expand_url`, `resolve_url_metadata`, `get_test_fixtures` | resolver registry / `analyze_url` |
| `analyze_url`, `create_download_task` | `services::single_url` |
| `cancel_download`, `retry_download`, `list_downloads`, `get_download_summary` | `DownloadManager` |
| `open_download_path` | reads the path from the DB, re-checks containment, hands it to the opened plugin |
| `list_history`, `get_history_count`, `get_settings*`, `get_database_status`, `get_app_info` | repositories / services |

### Logging — `logging.rs`

`tracing` with an `EnvFilter`, a human-readable stdout layer, and a daily-rolling
file appender under the data directory. Levels are driven by the persisted
`log_level` setting. The `WorkerGuard` is held by `run()` and dropped on
`RunEvent::Exit` so buffered records are flushed.

The policy, enforced by what is logged rather than by filtering: short links are
logged by code, response bodies are never logged, and no session token or cookie
ever enters a log line. Resolver, download, and database failures log at `warn`
or `error` with the technical message and the error source chain.

Phase 02 added the pipeline's milestones as structured events — URL analysis
started, short link expanded, post resolution started/completed (with the post
id, media type, item and variant counts), download started/completed/cancelled,
retry scheduled with its attempt and delay, and download failures with their
retryability. Per-chunk progress is deliberately *not* logged: a fast transfer
would drown the file, and the progress stream already carries that information.

## Frontend

`src/` is React 18 + TypeScript + Tailwind, with Zustand for state and
react-router (HashRouter) for navigation. There is no HTTP client in the
frontend: `services/ipc.ts` wraps `invoke`, `services/api.ts` names each command,
and `types/backend.ts` declares the wire types.

HashRouter rather than BrowserRouter because Tauri serves a single document over
the asset protocol — a path route would 404 on reload.

`npm test` runs `vitest` with `jsdom` and Testing Library over the shared
controls. `vitest.config.ts` merges the application's Vite config rather than
restating it, so the `@/` alias resolves to the same place in a test as in the
app — a second copy of that mapping is the kind of thing that works until someone
moves a directory. Globals are off: every test imports `describe`, `it` and
`expect` explicitly, because a file that needs no import to get `expect` is also
a file where a typo in a helper name silently refers to something else.

The suite covers behaviour a person needs in order to use a control without a
mouse or without sight — a field's error being attached to the field, a
destructive button asking first and being escapable, a progress bar saying what
it measures. It is not an accessibility certification and it does not render
whole screens.

Theming: Tailwind colors resolve through CSS variables (`--color-surface` etc.)
defined for `:root` and `.dark`, so the persisted `theme` setting actually drives
the palette instead of only toggling a class. A small pre-paint script in
`index.html` applies the OS preference before React mounts, because the
authoritative value lives in SQLite and is not readable until the backend
answers.

The UI holds no Douyin knowledge. The two test URLs are never written in
TypeScript — `get_test_fixtures` returns each link's `intent` (`profile` /
`post`, declared in Rust because both are short links and cannot be told apart
before the redirect is followed), its label, and the classification the parser
actually produced.

### The Single URL screen

`pages/SingleUrlPage.tsx` drives the workflow and reports its state explicitly —
idle, validating, resolving, resolved, unsupported, unavailable, error,
downloading, completed — so a user can tell an invalid link from a source that
would not answer from content that is genuinely unsupported. The status is
*derived* from the response and the error kind rather than stored, so it cannot
drift out of sync with what the backend said.

The page never decides what a link means. It renders `intent`, the normalized
`Post` / `MediaItem` / `MediaVariant` values, and — on failure — the backend's
`kind`, `message`, and a short recovery hint chosen from that kind
(`lib/errors.ts`). It never sees a media URL it could tamper with, because
`create_download_task` sends ids only.

### Live download progress

The backend broadcasts one `DownloadEvent` stream; `lib.rs` forwards it to the
webview under `download://event` and `hooks/useDownloadEvents` subscribes. The
two screens fold it differently on purpose:

- **Progress** frames patch the row already on screen, so a fast transfer does
  not cause a backend round-trip per chunk.
- Anything that changes a task's *shape* (queued, state change, completion,
  failure, removal) triggers one coalesced refresh, so the row, the summary, and
  the history all come from the backend rather than from a reconstruction of
  events in the client.

## The Library

The Library answers one question — *what do I have locally, and what is it
doing?* — by reconciling five things the application already stores: a post, the
creator who wrote it, the media resources it holds, the newest download task for
it, and whether the file that task produced is still on disk.

### Why an entry is a post, not a media item

A post discovered by a profile listing **has no media items at all**: Phase 03
resolves media only when a download is actually asked for. A media-item-level
library would therefore be empty of everything a user has discovered but not yet
downloaded — which is exactly what the "New" view exists to show.

The cost of the post-level choice is that a carousel is one row. That is covered
rather than hidden: `media_item_count` and `downloaded_item_count` say how many
resources the post holds and how many are on disk, so a three-image carousel
with one image fetched is visibly partial rather than reported as complete.

### One status rule, shared

`services/library.rs::reconcile_status` is the only place source availability is
folded together with local download state and file existence, and the Profile
Downloader's `local_status` delegates to it. Two copies would eventually
disagree about whether a post counts as downloaded.

It encodes three decisions:

1. **What the source says about availability outranks anything local.** A post
   reported unavailable is not made available by having been downloaded once.
2. **A finished download counts only while its file is on disk.** A row is not a
   file; a post whose file was deleted reads as `Ready` — downloadable again —
   rather than as a download that exists.
3. **A cancelled attempt says nothing about the content**, so it leaves the
   status the post already had.

### What is done in SQL, and what is not

The **high-cardinality** narrowing happens in `repositories/library.rs`:
free-text search, content type, creator, publication date. Those decide how many
rows there are to look at, and they are the ones indexes help.

The **derived** filters do not: duration, resolution, and above all status, which
depends on whether a file exists. SQLite does not know that, and a status
computed without it would be exactly the "a row is not a file" mistake this
application exists to avoid. So the repository returns raw facts and the service
derives the status — which means `scan` cannot paginate, because paginating
before a filter that can drop rows would produce pages that are short, or empty,
for no visible reason. `LibraryPage::scanned` reports how many posts were
considered, so the bound is visible rather than implied.

### Counts

Each view is counted with its own media-type and status predicate, taken over the
whole match rather than the page on screen — a tab never claims a number the
listing cannot produce.

### Export

`services/library.rs::to_csv` and `to_json` write the *match*, never just the page
on screen, to a file inside the download folder (`exports/`), which is the one
place this application is allowed to write. Two things about the CSV are
deliberate and documented in `README.md`: fields are quoted per RFC 4180, and a
value beginning with `=`, `+`, `-`, `@`, a tab or a carriage return is prefixed
with an apostrophe so a caption that reads as a formula is treated as text.

### The screen — `pages/LibraryPage.tsx`

The page owns no library logic; it owns three things the backend cannot know.

**Which answer to believe.** Typing in the search box starts a query per debounce
and they do not necessarily come back in order, so each request carries a
sequence number and a response is applied only when it is still the newest. An
answer to a query the user has since changed would otherwise fill the table with
rows that do not match the controls on screen.

**What "selectable" means.** Only rows whose reconciled status is `new` or
`ready` can be ticked, because those are the only ones a submission could
usefully act on. `Select new on this page` selects exactly those — the same rule
the backend applies when it skips what is already downloaded or already running.

**How to submit.** `Download selected` calls `submit_profile_downloads` — the
Profile Downloader's own command — with the selected post ids, then re-queries
and reports the outcome counts. There is deliberately no Library-specific
download command: the easiest way to introduce a second download path is to add
one for a screen that "just needs to start a download".

## Reliability and data integrity

Phase 06 is a set of invariants about *when the application is allowed to say
something worked*. Each one lives in the module that owns the fact, and each is
stated here with the failure it prevents.

### The completion invariant

A `DownloadTask` may be marked `Completed` only after: every required track has
been transferred; separate video and audio tracks have been combined; the result
has been **read back**; the file has been moved into its final destination; and
the task row and its history row have been written together.

`TransferWorker::validate_single_track` (`downloads/transfer.rs`) is the piece
that was missing. A two-track download was always probed, because combining two
tracks and getting one file is exactly the kind of thing that silently goes
wrong. A single-track download was not: the bytes went straight to the final name
on the strength of the transfer finishing. That proves the *transfer* worked and
nothing else — a media CDN answering `200` with an HTML error page produces a
non-empty file with the right extension that is not media at all.

The check runs in three tiers, cheapest first, so it holds on a machine with no
FFmpeg installed:

1. the file is not empty;
2. its leading bytes are not a text document (`leading_text_marker`) — a
   dependency-free sniff, deliberately narrow, because a false positive here
   would refuse a download that actually worked;
3. when a prober is available, the container really holds a stream.

### Completion is reported only if the database agrees

`finish_completed` returns whether the task actually became completed, and the
caller acts on that answer. Three separate things had to change:

* the `Completed` event moved *inside* the branch that knows the write happened.
  It used to be emitted unconditionally, so a task the guard had refused to
  finalize was still announced to the UI as finished;
* the write stopped being `let _ = …`. A failed record is now a task that did not
  complete, logged with the task id — not a silent success;
* the state change and the history row became **one transaction**
  (`Database::with_transaction`). They describe a single event, and statements
  that autocommit can leave the first without the second.

### State transitions are compare-and-set

`downloads_repo::update_if_state` writes `WHERE id = ? AND state = ?` and reports
whether a row changed. The earlier read-then-blind-write allowed a `cancel` to
commit between the two: `claim` would overwrite `Cancelled` with `Downloading`
and then download a task the user had stopped. The manager and the transfer
workers are separate tasks sharing a pool of eight connections, so the window was
real rather than theoretical.

The same shape now guards `finish_completed`, `finish_failed` and
`finish_cancelled`. The last two also *return whether they did anything*, because
emitting a `Failed` or `Cancelled` event for a task that had already reached a
different terminal state told the UI something the database disagreed with.

### Which destinations may be contacted — `network/policy.rs`

Every URL the application requests is either typed by the user or produced by a
resolver reading a page whose JavaScript the application does not control. The
second kind is why this module exists.

`refusal` is pure and allocation-light, so it can run on every hop of a redirect
chain, and it is applied in two places: at the start of each `HttpClient` method,
and inside a `redirect::Policy::custom` that re-checks each hop. `ChromiumRenderer`
calls the same function on its navigation URL, so the policy is a property of the
application rather than of one transport.

The one subtlety worth recording: only `to_ipv4_mapped` is unwrapped, never
`to_ipv4`. `Ipv6Addr::to_ipv4` also matches the IPv4-*compatible* range `::/96`,
which contains `::1` — unwrapping that turned loopback into `0.0.1` and let the
single most important address on the list through. A test caught it.

## Binding the connection to the checked address

The URL check alone leaves a hole the size of DNS: a URL carries a *name*, and
the address that name becomes is decided by whoever resolves it. Checking the
name and then letting the HTTP client resolve it again is not a check — a name
whose answer changes between the two lookups (DNS rebinding) walks straight
through, and the connection lands on the unchecked address.

`network/resolver.rs` closes it by being the resolution step.
`GuardedResolver` implements `reqwest::dns::Resolve` and is installed as the
client's resolver, so it sits exactly where `hyper`'s connector asks for
addresses — and what it returns is what the connector dials, with no second
lookup in between. It classifies the name first (a reserved name is refused
without a lookup), asks the real resolver, classifies the whole answer, and
returns only approved addresses.

Two decisions in it are worth recording:

- **A mixed answer is refused whole**, not filtered. A name answering with a
  public address and a loopback one is either misconfigured or hostile, and the
  two are indistinguishable; keeping the public member would survive the attack,
  because the client would still dial whichever member the connector handed it.
  The property that matters is "the address dialled was checked", and filtering
  does not give it.
- **A refusal must not be retried.** It happens inside the resolver, below the
  error mapping, and `reqwest` reports it as an ordinary connect error — which
  the mapping would have marked retryable, dialling a blocked destination until
  the attempt budget ran out. The refusal carries a marker and a stable category
  tag (`Refusal::tag`), `policy_refusal` walks the error's source chain for them,
  and the refusal that comes back is a typed, non-retryable error naming the
  host — never the URL, whose query string may be a credential.

TLS is unaffected: the connector still dials with the URL's hostname, so SNI and
certificate verification see the name, not the pinned address. A URL that is
*already* an IP literal never reaches the resolver — `hyper-util` dials those
directly — and is refused by the URL check instead. Between them, the two halves
cover every outbound request the HTTP client makes.

The deterministic tests inject a fake resolver, which is the same seam the
guarded client itself resolves through. The rebinding case is directly
expressible: a resolver whose first answer is public and whose second is
loopback — the first lookup is allowed, the second is refused, and no path
returns the loopback address.

## Chromium: what is and is not covered

The renderer validates its navigation URL against the same policy, and its
DevTools client is deliberately **permissive**: the target list lives on
`127.0.0.1` on a port this application chose, and the policy exists to keep
*content* away from loopback — a control channel to the application's own child
process is not that case. (Wiring the content policy in front of it refused
every render; the live browser tests caught it within one run.)

Checking the navigation URL bounds where a render can be *pointed* and nothing
more. Chromium resolves DNS with its own stack, follows its own redirects, and
fetches its own subresources, so a page that redirects to — or fetches from — a
loopback or private address is a request this application would never see.

That half is now closed by an enforceable network boundary rather than by a
documented gap. Every launched browser is given
`--proxy-server=http://127.0.0.1:<port>` pointing at
`network/chromium/proxy.rs`, a listener this application owns on a port it
chose, plus `--proxy-bypass-list=<-loopback>`, which removes Chromium's
*implicit* bypass for loopback and link-local destinations — without it, the one
class this exists to refuse would never reach the proxy at all.

`GuardedProxy` is not a second policy. It calls the **same**
`DestinationPolicy` and the **same** `GuardedResolver` the HTTP client uses, so
the browser ends up with the same guarantee: the address dialled is an address
that passed classification. `CONNECT host:port` (an `https` destination) is
classified by name, resolved once, and *every* address in the answer
classified; a refusal answers `403` and no tunnel is opened, while an approved
destination that simply will not connect answers `502` — a network condition and
a policy decision are not the same answer and are not given the same one.
Absolute-URI `http` requests are checked the same way before anything is
forwarded.

What this does not cover: WebRTC can open UDP flows no HTTP proxy carries, and
Chromium's own background services are outside it too (the launch already passes
`--disable-background-networking`). The honest statement is that this bounds
*HTTP(S) requests the page makes*, which is where a rebinding or subresource
attack lives, and not literally every socket the browser can open.

It is verified two ways. Deterministically, against a resolver whose answer a
test controls: a public name answering with loopback, a mixed answer, and a
public name answering with the cloud metadata address each get a `403` at the
wire. End to end, against the real browser: `127.0.0.1.sslip.io` is an ordinary
public name that resolves to this machine, so every URL-level check passes — the
test proves a plain client reaches a loopback server through it, and that the
browser does not.

### Session lifecycle

The renderer keeps one browser alive and navigates it as often as needed.
`network/chromium/mod.rs` decides, per render, whether the browser already there
may serve it (same visibility, last render completed), and what a failure means:
a page that did not become ready is a statement about the *page*, so it is
looked at again on the same browser, while anything else — a browser that would
not answer, a navigation that never returned — leaves a session nobody can
vouch for, so it is retired and one replacement is allowed. The
`BrowserLauncher`/`LaunchedBrowser` seam exists so this can be pinned
deterministically: the tests drive the real session code against a loopback CDP
server, with no browser and no network.

### Timeouts

`ReqwestHttpClient` builds two clients from one setting:

| Client | Shape | Why |
| --- | --- | --- |
| page | total `timeout` | A page that has not finished in the budget is not going to. |
| transfer | `connect_timeout` + `read_timeout` | `ClientBuilder::timeout` is a *total* deadline covering the whole body, so applying it to a transfer aborts a large file on a slow connection that was still making good progress. A read timeout fails only when the source stops sending. |

### Filesystem containment — `storage/mod.rs`, `storage/filenames.rs`

`is_within` compares path *text*, which cannot see a junction or symbolic link:
`D:\Downloads\link\file.mp4` is inside `D:\Downloads` as a string and somewhere
else entirely as a file. `resolved_is_within` resolves the deepest part of the
candidate that already exists — the file itself usually does not — and compares
where it actually is.

The check runs when the path is planned and again in `prepare_directory`, which
is the gate immediately before the first write. That second placement is the one
that matters: a link planted between planning and writing is still caught, and
`prepare_directory` taking the whole `DestinationPlan` (rather than a bare path)
is what gives it the root to compare against.

`finalize` no longer deletes an existing file before replacing it. `rename`
replaces on both platforms this ships on, so the delete only created a window in
which the user's file was gone and its replacement had not arrived. A
cross-volume move stages the copy beside the destination and renames it, so a
failed copy leaves the destination either absent or complete.

### Disk space — `storage/space.rs`

One platform call (`GetDiskFreeSpaceExW`) behind a function that returns
`Option<u64>`; no dependency for a single syscall. `None` means "unknown" and
every caller already handles it, because a volume that will not report a figure
is a case that exists.

`StorageManager::preflight` creates the directory, writes and removes a probe
file — the only test that answers "can this be written to at all" — and compares
free space against `required_headroom(expected)`. The headroom is a *multiple*
(3×), because a video whose sound arrives separately is written as two tracks and
then copied into a combined file.

The size is usually known only on a resume or a retry, because `Content-Length`
arrives with the response. That is the honest limit of a preflight, and it is why
an unknown size skips the comparison rather than guessing one.

### Settings that cannot be used

`AppSettings::repair` answers a different question from `validate`: not "may this
be saved?" but "the application is starting and this is what was in the database
— what should it actually use?". A stored blob is not necessarily one this build
wrote. Each field is repaired on its own, so one bad value does not cost the user
every other preference, and the list of changes is logged — a repair the user
never hears about is a bug report nobody can act on.

## Interaction quality

`components/ui/ConfirmButton.tsx` is an inline two-step confirmation rather than
a modal. The application has no dialog primitive, and adding one — focus trap,
overlay, portal, escape handling — for two actions is a lot of machinery to get
wrong. What it does take seriously is the keyboard: Escape cancels, focus moves
to the confirming button when the question appears and returns to the trigger
when it is answered.

`Field` links a control to its label, hint and error **by id** rather than by
wrapping it in a `<label>`. Wrapping gives a screen reader the label as the
control's name and nothing else: the hint and the error are siblings the control
has no relationship to, so neither is announced.

`lib/a11y.ts` exports one focus ring for the controls that are deliberately not
`<Button>` — the view tabs, the test-link chips, a notice's dismiss button. A
constant rather than a component, because these controls share nothing else.

## Extending into later phases

| Phase | What it adds | Where |
| --- | --- | --- |
| 03 | Profile listing and bulk selection | **Built.** `DouyinResolver::resolve_profile` reads the creator from the rendered profile page; `list_posts` enumerates through the same renderer with a scroll-driven cursor. `services/profile.rs` owns the sequence, and submission reuses `AppCore::create_download`. |
| 04 | Worker pool, pause/resume with byte ranges | **Built.** `TransferWorker` is now a dispatcher over a persisted queue with a live-read concurrency limit; pause keeps the partial and resume offers it as a `Range`, validated before anything is appended. `DownloadManager`'s interface is unchanged. |
| 05 | Library views | **Built.** `database/repositories/library.rs` holds one joined read over posts, creators, media and downloads; `services/library.rs` reconciles the status, applies the derived filters, pages, and writes JSON/CSV. It adds no tables and no second download path — a selection is submitted through `submit_profile_downloads`, which is the same command the Profile Downloader uses. |
| 06 | Reliability, security, UX | **Built.** `network/policy.rs` decides which destinations may be contacted; `storage/space.rs` reads free space; the completion invariant in `downloads/transfer.rs` became a compare-and-set inside a transaction; `storage/mod.rs` resolves links before writing and replaces files atomically. |
| 07 | Packaging | `tauri.conf.json` bundle section; no architecture change. |

Not yet built, and deliberately visible as such:

- **Combining Douyin's separate tracks — built in Phase 02.** The player serves
  video and audio as two renditions, and the audio is only visible in the page's
  own network traffic, so the browser renderer reports that traffic and the
  resolver marks each item with a `TrackLayout`. A video item marked `audio_video`
  is fetched as both tracks and combined with FFmpeg (`-c copy`, never
  re-encoded); the result is read back with `ffprobe` before the task is marked
  complete, so a file missing a track is a failure rather than a plausible-looking
  success. The container is named explicitly (`-f mp4`) because the mux output is
  a temporary `.part` path FFmpeg could not otherwise infer a format from.

## Security and scope

- No watermark removal, and no bypassing of authentication, private-content
  restrictions, access controls, or platform protections. The resolver
  *classifies* Douyin's watermarked rendition marker; it never rewrites it.
  `MediaVariant` can represent an original/clean source when one is legitimately
  offered, which is all later phases will need.
- Only publicly accessible content is in scope.
- The live Douyin requests are: the short-link redirect (redirect only, body
  never read) and the public share page for a post. The mobile user agent used
  for the latter is a request format the page is built to serve, not an
  access-control bypass.
- Response bodies are never logged, the fetched page is capped at 4 MB, and
  secrets, session tokens, and cookies never enter a log line.
- File writes are confined to the download directory by an explicit containment
  check; templates that could escape are rejected at validation time; a
  substituted caption cannot introduce a path separator or a `..` hop; and
  `open_download_path` re-checks containment before handing a path to the OS.
- The path handed to the OS is read from the database, never from the caller, so
  the renderer cannot point the opener at an arbitrary file.
- Browser-assisted resolution renders the same public page a person would open.
  It does not sign in, solve a challenge, or forge a request signature; a page
  that refuses an unauthenticated browser is reported as refused. It uses a fresh
  throwaway profile and never touches the user's own browser data, and it starts
  only when the user's setting allows it and a browser is installed.

## Browser-assisted resolution — dependency

`tokio-tungstenite` (with `connect`/`handshake`, no TLS — the endpoint is always
loopback) speaks the Chrome DevTools Protocol, which is a WebSocket protocol.
`futures-util` drives that socket; it was already in the tree through `reqwest`,
so it adds no new compilation. No browser-automation framework is used: the few
CDP commands this application needs are implemented directly, because a framework
would be a second system to maintain for no additional capability.
