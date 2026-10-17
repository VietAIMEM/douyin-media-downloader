# The idea behind Douyin Downloader

This document describes what the application set out to be, and how much of that
it actually became. It is derived from the project's own planning and
architecture documents — the original design brief and
[`code/ARCHITECTURE.md`](../code/ARCHITECTURE.md) — rather than written fresh,
so it records the original intent instead of rationalising the result.

**Intent and implementation are marked separately throughout.** Anything labelled
*Intended* is a design decision that was written down before the code existed.
Anything labelled *Implemented* was read back out of the source tree. Where the
two diverge, both are stated, including the cases where the design changed on
purpose.

---

## 1. The problem

Downloading from Douyin with ad-hoc scripts or browser extensions is awkward in
ways that have nothing to do with downloading:

- A link belongs to a *post*, but a post may hold a video, a set of images, a
  carousel, an audio track, or several renditions of the same media — and the
  page gives you no clean way to see what is actually there before you commit.
- A creator's back catalogue has to be paged through by hand, with no way to
  select across pages or skip what you already have.
- Every script grows its own download loop, its own retry logic and its own idea
  of where files go, so nothing is resumable and nothing is recorded.

The stated goal was a **focused desktop media management application, not a
collection of downloader scripts** — one that lets you see what is available,
choose what you want, and then find the result afterwards.

**Intended, and implemented.** The application is a Windows desktop app with a
library, a download queue and a history, rather than a command-line tool.

---

## 2. The two principal workflows

Both workflows were specified before implementation and both exist in the
released build.

### Single URL Downloader

Paste one post or share link; the application validates it, works out what media
the post exposes, shows that for review, and downloads what you select.

```
Paste URL → Validate → Analyse → Resolve media
         → Preview → Select media/variant → Download → Save → Record history
```

### Profile Downloader

Paste a creator profile link; the application resolves the creator, enumerates
their publicly accessible posts a page at a time, and lets you filter and select
across pages before submitting everything to one queue.

```
Paste profile URL → Resolve creator → Load public posts → Paginate
                  → Filter / select → Resolve media → Create tasks → Queue
```

The design intent was explicit that the two workflows must share the same media
and download abstractions rather than each growing their own — the Profile
Downloader is a *producer of download tasks*, not a second downloader.

**Implemented.** Both screens exist (`src/pages/SingleUrlPage.tsx`,
`ProfileDownloaderPage.tsx`) and both submit into the same download manager.

---

## 3. Media the application aims to handle

The content model was designed around normalised types rather than the platform's
own response shapes, so that the rest of the application never depends on how
Douyin happens to structure a page.

| Type | Meaning | Status |
| --- | --- | --- |
| `video` | A video post | Implemented |
| `image` | A single image | Implemented |
| `carousel` | A post holding several images | Implemented |
| `audio` | An audio resource associated with a post | Implemented |
| `text` | A post that is neither a video nor a picture | Implemented, deliberately without a download |

`text` was **not** in the original design; it was added during implementation
because a text post has no media to fetch. It is kept distinct rather than folded
into another type precisely so that such a post is never offered a download it
cannot perform — its content is saved through its own path, as a `.txt`.

A source availability status (`new`, `ready`, `unavailable`, `private`,
`unsupported`, …) is tracked **separately** from a download's own state
(`queued`, `downloading`, `paused`, `completed`, `failed`, `cancelled`). The
original design called this out as two concepts that must not be conflated: one
describes the source, the other describes a local file transfer.

---

## 4. Intended architecture

The single most important design decision was to keep **platform-specific
resolution separate from everything else**. Douyin's behaviour changes; a UI, a
download engine and a database should not have to change with it.

```
┌────────────────────────────────────────────┐
│              Desktop UI                    │
│          React / TypeScript                │
└───────────────────┬────────────────────────┘
                    │ Tauri commands (IPC)
                    ▼
┌────────────────────────────────────────────┐
│            Application core                │
│                 Rust                       │
│   Resolver │ Downloads │ Storage           │
│   Database │ Settings                      │
└────────────────────────────────────────────┘
```

### How the delivered layout differs from the sketch

The planning documents sketched `src/frontend/*` and `src/backend/*`. The
delivered tree keeps every responsibility but splits it where the toolchain
requires:

| Sketched | Delivered | Why |
| --- | --- | --- |
| `src/frontend/*` | `src/*` | Vite owns `src/` as the web root, so a `frontend/` level is redundant |
| `src/backend/*` | `src-tauri/src/*` | The Rust core must be its own Cargo package, which is what `src-tauri/` is |
| — | `src-tauri/src/commands/` | New: the IPC boundary is a layer, not an afterthought |
| — | `src-tauri/src/services/` | New: the composition root that wires the layers and owns startup ordering |

### Layers, as built

```
React UI (src/)          invoke("command")  ·  listen("download://event")
        ↓
Tauri commands           validate, convert errors, serialise
        ↓
Application services     AppCore · single_url · profile · library
        ↓
Resolvers  Downloads  Media  Storage  Settings  Network  Database
```

The dependency arrow only ever points down. The UI never sees a SQLite error, a
`reqwest` error, or a raw Douyin response shape — it sees a serialised error with
a stable `kind`/`code` pair and a user-safe message.

### The resolver boundary

The planned interfaces were `ProfileResolver`, `PostResolver` and `MediaResolver`:

- **ProfileResolver** — validate profile URLs, resolve creator information,
  enumerate public posts, manage pagination.
- **PostResolver** — resolve a content URL, retrieve metadata, identify the post
  type, retrieve associated media.
- **MediaResolver** — normalise media resources, detect available variants, and
  return downloadable media information.

**Implemented** in `src-tauri/src/resolvers/`, with the Douyin-specific work
confined to `resolvers/douyin/`. This is the boundary the whole design exists to
protect: everything that knows about Douyin lives behind it.

### The download manager

All workflows were specified to feed a single centralised manager — never a
download loop inside a screen — with one queue, worker pool, and file storage
behind it.

**Implemented** in `src-tauri/src/downloads/`. Downloads are queued, retried with
bounded attempts, paused, resumed, cancelled, and recovered after a restart.

### Storage

SQLite was chosen for persistent state from the beginning, with migrations
required from the first release. The planned entities were `creators`, `posts`,
`media_items`, `downloads`, `download_history` and `settings`.

**Implemented** in `src-tauri/src/database/`, with migrations in
`code/migrations/`. The delivered schema matches the plan and adds one table,
`media_variants`, for the renditions a single media item can expose.

### Settings

Download folder, concurrency, naming and folder templates, conflict policy,
retry and timeout limits, browser-assisted resolution, theme and logging.

**Implemented** in `src-tauri/src/settings/`.

---

## 5. Development principles

These were written down as the rules the implementation should follow.

**Separation of concerns.** UI, resolution, downloading, storage and persistence
stay independent of one another. The passing of a raw platform response into the
UI is the failure mode this rule exists to prevent.

**One download pipeline.** Every source of a download goes through the same
manager. The UI never implements download logic of its own.

**Platform isolation.** Anything that knows about Douyin lives inside a resolver.

**Normalised domain models.** The rest of the application operates on the
application's own types, not on the platform's.

**Persistent state.** Task and library state survives a restart.

**Extensibility.** Adding another platform should mean adding a resolver, not
rebuilding the application.

**Simplicity and transparency.** A first-time user should be able to paste a URL
and know what to do next, and should always be able to see what was found, what
can be downloaded, what is happening now, and why something failed.

**Testability.** Filename sanitisation, URL validation, state transitions,
database operations, resolver behaviour under bad input, and the download paths
including failure and resumption were all specified as test targets.
**Implemented:** the Rust core carries a substantial in-tree test suite plus
integration tests under `src-tauri/tests/`, and the frontend has a Vitest suite.
A small number of tests that would contact Douyin or drive a real browser are
deliberately excluded from the default run.

---

## 6. What the application deliberately does not do

These were written into the design as constraints, not as future work.

- It does **not** bypass access controls, private-content restrictions, or
  platform security protections. It works with what the source legitimately
  serves.
- It does **not** remove watermarks, and it does not alter downloaded media.
  Where a source legitimately offers an original resource, the application may
  prefer it — that is a selection, not a modification.
- It does **not** guarantee that any particular post, profile, media variant or
  host will remain accessible. Availability is the source's to decide.

---

## 7. Known limitations

Honest constraints of the released build, as opposed to the intent above.

- **Windows x64 only.** The application is a Tauri desktop app built and tested
  for Windows 11 x64. No other platform is claimed.
- **The installers are not code-signed**, so Windows shows no publisher and
  SmartScreen may warn.
- **Opening the Profile Downloader page causes browser activity.** The page
  performs a login-status check on load, which starts the application's own
  browser profile and loads Douyin's home page if a supported browser is present.
  It is not gated by the saved-login setting.
- **Public content only.** Posts that require a session, that are region
  restricted, or whose response format changes may not be readable.
- **The published history is squashed**, so a change-by-change log of
  pre-publication development is not available in this repository.

---

## 8. Where the detail lives

| Document | Contents |
| --- | --- |
| [`code/ARCHITECTURE.md`](../code/ARCHITECTURE.md) | The delivered layering and the boundaries between layers |
| [`code/docs/USER_GUIDE.md`](../code/docs/USER_GUIDE.md) | How to use the application |
| [`code/docs/PRIVACY.md`](../code/docs/PRIVACY.md) | What is stored locally and what leaves the machine |
| [`code/docs/RELEASE_NOTES.md`](../code/docs/RELEASE_NOTES.md) | Detailed notes for the 1.0.0 build |
| [`CHANGELOG.md`](../CHANGELOG.md) | Released versions |
