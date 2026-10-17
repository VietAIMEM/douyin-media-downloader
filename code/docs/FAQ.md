# Frequently asked questions

## Why can't the application read this profile or post?

Most often because Douyin did not serve it to an anonymous visitor. The
application reads public pages exactly as a logged-out browser would; it does not
sign in, use your account, or read your browser's cookies. Content that is
private, friends-only, deleted, region-restricted, or behind a challenge is
therefore not available to it, and the application says which of those it looks
like rather than failing silently.

Two specific cases worth knowing:

- **"This content is no longer available."** The post is gone or was made
  private. Nothing will change that.
- **"The page did not finish loading in the browser in time."** Douyin served
  something other than the post — sometimes its front page, sometimes a
  half-built shell. The application retries once, and if the second look is the
  same it reports it. Trying again later sometimes works, because it depends on
  what the site served that minute.

## Why did some media in a post fail, and not others?

A post can list several renditions of the same video, served from different CDN
hosts. If one host refuses or times out, the application tries the next, and only
reports a failure when every rendition of that track failed. A failure on one
item in a carousel does not affect the others; each is its own task.

## Why did my download start over instead of resuming?

Deliberate, and it is the safe answer.

When a download is paused, the bytes already received are kept, along with a note
of which version of the file they came from — its ETag, or failing that its
`Last-Modified` date. Resuming asks the source to continue from where it stopped,
and the application only accepts that continuation when the source can prove it
is the **same file**:

- the source must answer the ranged request with a validator that matches the one
  the partial was written under; and
- the size must still be what it was; and
- the response must start at exactly the byte the partial ends at.

If any of that cannot be established, the file is fetched again from the
beginning. Two files can have exactly the same length and different contents —
a re-encode, a re-upload — and splicing the tail of one onto the head of the
other produces a file that plays for a while and then does not. Starting over
costs one download; a spliced file costs a file you cannot trust.

**What this means in practice:** many servers do not offer a usable validator at
all, or do not support resuming. Douyin's own media links are signed and
short-lived, so a resume usually cannot reuse them anyway. Pausing still stops
the network immediately and frees the slot for other downloads.

## Why does it need FFmpeg?

Douyin often serves a video and its sound as two separate tracks. Combining them
into one playable file is exactly what FFmpeg's `-c copy` does: it copies the
existing streams into a new container without re-encoding anything.

Without FFmpeg, a download that needs two tracks is refused with a message naming
FFmpeg. It does not save the video without its sound, because a silent video
presented as a successful download is worse than an honest failure. Downloads
that need only one track work without FFmpeg.

Install FFmpeg from [ffmpeg.org](https://ffmpeg.org/download.html) or with
`winget install Gyan.FFmpeg`, and make sure `ffmpeg.exe` and `ffprobe.exe` are on
your `PATH`. The Home screen reports whether they were found.

## Why does it need a browser?

Some Douyin pages build themselves entirely with JavaScript and only then ask
their own server for the media, using request parameters a plain HTTP client
cannot produce. For those pages the only way to see what the page itself is given
is to let a real browser run it.

The application uses an installed Chrome or Edge, in a **fresh throwaway
profile** — no cookies, no history, no saved sessions, nothing from your own
browsing — and closes it afterwards. Media is always downloaded by the
application over an ordinary HTTP connection, never by the browser.

Edge is present on every supported Windows install, so this normally needs
nothing from you. If no browser is found, the Home screen says so, and the
`DOUYIN_DOWNLOADER_BROWSER` environment variable can point at one.

## Why isn't a particular quality or variant offered?

The application offers what the page exposes to the browser. If Douyin's page
lists only one rendition for a post — which it sometimes does for logged-out
visitors — then that is what there is. The application does not invent a variant
it was not offered.

## Where is my download history?

In the application database, at
`%APPDATA%\DouyinDownloader\DouyinDownloader\data\douyin-downloader.db`, along
with your settings, your Library, and the state of the download queue. The
**History** screen in the application is a view of it.

## Does uninstalling delete my downloaded files?

No. Uninstalling removes the application's own program files and shortcuts. It
does not touch:

- your download folder or anything in it, wherever you pointed it;
- the database, so your settings, Library, and history are still there if you
  reinstall;
- any log files.

There is no "delete my data" option in the uninstaller, and nothing is removed
without you deleting it. To remove everything, uninstall and then delete
`%APPDATA%\DouyinDownloader` and your download folder yourself.

## I installed a newer build and something went wrong. What now?

Installing replaces the program files and nothing else, so your settings,
history and downloads are unaffected even if the install fails.

- **The install failed part-way.** Run the installer again; it is safe to rerun.
  If it will not complete, uninstall from **Apps → Installed apps** and install
  again. Uninstalling does not remove your settings, history or downloads.
- **An old version is still installed afterwards.** Close the running application
  first — Windows will not replace files a running program holds open — then run
  the new installer again.
- **The application does not start afterwards.** Run the same version's installer
  again. Your data is not touched; see
  [personal use](PERSONAL_USE.md#4-upgrading-by-hand).
- **A newer version behaves badly.** Install the previous version's installer
  over it. Your data is kept either way, but a database the newer version has
  migrated may not open in the older one, so copy the data directory first if you
  intend to go back.

## Can it download a whole profile at once?

Yes, subject to the concurrency limit in Settings. It reads a page of the
creator's posts at a time, you select what you want, and it queues them all.
Posts are downloaded independently, so a failure on one does not stop the rest.

## Is there a limit on how much it can download?

Only what your disk and your patience allow. There is a free-space check before
each download whose size is known, and a destination that cannot be written to is
refused before the transfer starts rather than after it.

## Why does Windows say the publisher is unknown?

The installers are not code-signed. Signing needs a paid certificate that this
project does not have, so Windows has nothing to name and SmartScreen may warn on
first run. That warning is about the missing certificate, not about the file.

If you would rather check the file than trust it, hash it and compare against the
output of the build that produced it:

```bash
certutil -hashfile "Douyin Downloader_1.0.0_x64-setup.exe" SHA256
```

That confirms the file is the one you built. Nothing but a certificate can
confirm who built it.

## Can I use it without an internet connection?

No. Everything it does involves reading a Douyin page and downloading from
Douyin's servers.

## Does it phone home?

Only to Douyin, for the pages and media you ask for — and nothing else, because
this build has no update channel to check. There is no telemetry, no analytics,
and no crash reporting. See the [Privacy information](PRIVACY.md).
