# Release notes

## 1.0.0 — personal-use build

**Prepared:** 2026-10-12
**Status:** **not published and not distributed.** This is a build made from this
repository for use on the machine that built it. There is no download page, no
announcement, and nothing has been uploaded anywhere.

---

### What this is

The first version of Douyin Downloader for Windows x64. The version number starts
at 1.0.0 rather than 0.x because everything in this document is intended to be
relied on rather than provisional — there is no earlier build whose behaviour has
to be kept working.

### Installing

Built with `npm run tauri:build`; see
[personal use](PERSONAL_USE.md#2-building-and-installing).

- `Douyin Downloader_1.0.0_x64-setup.exe` — per-user install, no administrator
  needed. Recommended.
- `Douyin Downloader_1.0.0_x64_en-US.msi` — per-machine install for managed
  machines; asks for elevation.

The installers are **not code-signed**, so Windows will not show a publisher name
and SmartScreen may warn on first run. See *Known limitations*.

---

### What it does

**Single URL** — paste a Douyin post or share link, analyze it, choose from the
media it exposes, and download.

**Profile Downloader** — load a creator's public profile a page of posts at a
time, select across pages, and submit the selection to one queue.

**Downloads** — a queue with a configurable concurrency limit, per-task progress
with measured speed and remaining time, pause, resume, cancel, retry, and
recovery of the queue after the application restarts.

**Library** — browse what has been downloaded, search it, filter by type, length,
size and creator, sort it, and export metadata to JSON or CSV.

**History** — every outcome, with time, size, and destination.

**Settings** — download folder with a native folder picker, file name and folder
templates, conflict policy, concurrency, retry and timeout limits, browser-
assisted resolution, theme, and logging.

**Video and audio** — when Douyin serves a post's sound as a separate track,
FFmpeg stream-copies the two together into one playable file.

### How it behaves under failure

The parts of this release that took the most work are the parts that only matter
when something goes wrong:

- A download is retried when retrying could help — a timeout, a throttled or
  overloaded source, an expired media link with another mirror available — with a
  growing delay between attempts, and bounded by the attempt limit. A failure
  that will not change (a removed post, a refused resource) is not retried.
- A transfer is only continued from a partial file when the source can prove it
  is still the same file, by matching the version validator it was written under.
  When that cannot be established, the download restarts rather than risk a file
  assembled from two versions. See the
  [FAQ](FAQ.md#why-did-my-download-start-over-instead-of-resuming).
- A file is only reported as complete after it has been read back and both the
  task and its history row written in one transaction.
- Cancelling or pausing during the last moments of a download wins: the
  completion is discarded rather than announced.

### Security and privacy

- **Destinations are checked at connection time, not just in the URL.** Every
  address the application dials is one the policy approved, including on every
  redirect hop. Loopback, private, link-local, carrier-grade NAT, and reserved
  destinations are refused, as are the special-use names.
- **The same policy holds for the browser.** Chromium is launched with every
  request routed through a guard this application owns, which applies the same
  rules, so a page cannot reach this machine or a private network through a
  redirect or a subresource.
- **No telemetry, no analytics, no crash reporting.** See
  [Privacy](PRIVACY.md).
- **Site credentials never reach a log.** A signed media URL's query string is
  never written anywhere, and neither is anything the application does not store.
- Downloads are written only inside the folder you chose. A symbolic link or
  junction that leads out of it is refused.

---

### Defects fixed since the internal phases

Not a complete list; these are the ones a user could have run into.

- **A resumed download could splice two versions of a file.** Recording the
  offset and the size was not enough, and neither was trusting a `206` response.
  A partial now records the resource's validator, sends it as `If-Range`, and is
  continued only when the response names the same validator, starts at the same
  byte, and states the same total. A response naming no validator is refused
  rather than trusted; a weak `ETag` is never accepted; a `Last-Modified` is used
  only when the response's own `Date` establishes it as strong.
- **A private address could be reached through a public name.** Checking a name
  and then letting the HTTP client resolve it again were two different
  resolutions. The resolver is now the check.
- **The browser could reach this machine.** Chromium's own network stack resolved
  DNS independently, so a redirect or a subresource aimed at loopback was not
  checked. It is now behind an enforced network boundary.
- **A page-level failure cost an extra browser start**, and a killed browser could
  leave its temporary profile behind.
- **A pause issued as a transfer was claiming its task could leave the task
  cancelled** instead of paused, and a state change could overwrite an outcome a
  worker had just committed.
- **Concurrent downloads could fail preflight on a writable folder** through a
  shared probe-file name.

---

### Known limitations

**There is no in-app update check in this build.** The update mechanism is
implemented — check, download, verify, install, restart — and has been exercised
end to end against a local feed serving a signed package, including the refusal
of a tampered one. It is dormant here because a personal-use build carries no
update channel and no key to verify one with: there is nowhere to check and
nothing that could legitimately answer. Settings → **Updates** says so rather
than offering a check whose only possible outcome is a failure. Upgrading means
running a newer installer; see [personal use](PERSONAL_USE.md#4-upgrading-by-hand).

**The installers are not code-signed.** Windows will show "Unknown publisher" and
SmartScreen may warn on first run. Hashing the file and comparing it against your
own build output confirms it is the file you built, though nothing can confirm
who built it without a certificate. See
[personal use](PERSONAL_USE.md#windows-will-warn-you-about-the-publisher).

**FFmpeg is not bundled.** Posts whose audio is a separate track need it. Without
it those downloads are refused with a message naming FFmpeg rather than saved
without sound. See [Troubleshooting](TROUBLESHOOTING.md#ffmpeg).

**Only Windows x64.** No other architecture is built.

**Not every Douyin link works, and none of them are guaranteed to keep working.**
The application reads public pages as a logged-out visitor. Private, deleted,
region-restricted, or challenged content is not available, and Douyin's pages
change without notice. See
[What it will not do](USER_GUIDE.md#what-it-will-not-do).

**Resuming rarely helps against Douyin specifically.** Its media links are signed
and short-lived, so a resumed request usually cannot reuse them and the transfer
restarts from the beginning. Pausing still stops the transfer immediately.

---

### Upgrading

There is no earlier version to upgrade from. For future builds, upgrading means
running the newer installer over this one — there is no in-app updater. What that
does and does not touch:

- Your settings, Library, download history, and downloaded files are not affected.
- The database is migrated forward automatically on first launch; there is no
  separate step, and an interrupted migration is retried rather than left half
  applied.
- Uninstalling removes the program and the Start Menu entry, and leaves the data
  directory in place.
- Downgrading is not supported. Running an older build's installer by hand works,
  but a database that a newer version has migrated may not be readable by an
  older one — copy the data directory before upgrading if you intend to go back.
  See [personal use](PERSONAL_USE.md#4-upgrading-by-hand).

### Prerequisites

Windows x64 with the WebView2 runtime (present on Windows 11 and current Windows
10). Chrome or Edge is recommended and FFmpeg is optional, as described above.
