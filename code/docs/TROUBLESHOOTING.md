# Troubleshooting

What to do about the messages this application actually produces. Where a
message is quoted, it is quoted from the application.

If you are reporting a problem, see [Support](SUPPORT.md) for what to include —
and for what not to.

---

## The application will not start

**Nothing appears at all.** The window needs the WebView2 runtime, which is
present on Windows 11 and on current Windows 10. If it is genuinely missing, the
NSIS installer installs it; an MSI installation on a machine without it needs it
added separately — install the **Evergreen WebView2 Runtime** from Microsoft.

**A window opens and closes, or the Home screen reports a database problem.**
Start with the log, which is written even when the window fails:

```
%APPDATA%\DouyinDownloader\DouyinDownloader\data\logs\
```

`RUST_LOG=debug` in the environment before launching gives much more detail.

**The database is locked or unreadable.** Another copy of the application may
still be running — check Task Manager. If the file is genuinely damaged, move
`douyin-downloader.db` aside (the application recreates it, starting empty) and
keep the original for diagnosis. Your downloaded media is untouched by this.

---

## "This content needs a browser to read it, and no supported browser was found."

No Chrome or Edge was found in the usual places.

- Edge is on every supported Windows install. If it was removed, install Chrome
  or Edge.
- If a browser lives somewhere unusual, name it:

```
set DOUYIN_DOWNLOADER_BROWSER=C:\path\to\chrome.exe
```

**The browser is found but the page still will not load.** Some pages are served
differently to a headless browser. If you are willing to see the window, this
puts it on screen rather than off it, which is occasionally what the site wants:

```
set DOUYIN_DOWNLOADER_BROWSER_WINDOW=visible
```

Either variable can be set in the same Properties → Advanced dialog as any other.

---

## "FFmpeg"

> This video keeps its sound in a separate track, and putting the two together
> needs FFmpeg. Install FFmpeg, or point the application at an ffmpeg executable.

Douyin served the video and its audio separately. Combining them needs FFmpeg.

- Install it: `winget install Gyan.FFmpeg`, or from
  [ffmpeg.org](https://ffmpeg.org/download.html).
- Make sure `ffmpeg.exe` **and** `ffprobe.exe` are on your `PATH`. The Home
  screen reports both.
- Downloads that need only one track do not need FFmpeg and are unaffected.

If FFmpeg is unavailable, a download that needs it **fails and says so**. It does
not save a silent video and call it complete.

---

## Network problems

> A network problem prevented this operation. Check your connection and try
> again.

A request could not be completed: a timeout, a dropped connection, or a source
that refused it. Retryable failures are retried automatically, with a growing
delay, up to the attempt limit in Settings.

> The source is temporarily unavailable. Try again shortly.

The source answered with a server error. Wait and retry.

> The source did not return this content.

The source answered `4xx`. Retrying the same request will produce the same
answer, so it is not retried automatically. Common causes: the post was removed
between reading the page and downloading the media, or the media link expired.
Analyze the link again to get a fresh one.

> This content redirected somewhere the application will not follow.

A redirect pointed somewhere this application refuses to contact — a loopback or
private address. This is a safety boundary, not a mistake to work around. It
usually means a link is not what it claims to be.

**Timeouts on a slow connection.** Raise **Request timeout** in Settings. Media
transfers are given a stall budget rather than a total deadline, so a large file
on a slow line is not cut off for being slow — only for stopping.

---

## Downloads that fail

> The file could not be downloaded. You can retry the download.

The transfer did not complete. Use **Retry** on the task. The reason is in the
log if the message is not specific enough.

> The source returned an error page instead of the file. Try again later.

The source answered `200` with HTML or JSON instead of media. Almost always
expired or invalid media link — analyze the post again.

> The downloaded file could not be read as media.

The bytes arrived but are not a media file. Same cause as above.

> The source returned an empty file.

Zero bytes. Retry, or analyze the link again.

**Everything fails at once.** Check whether Douyin itself is reachable in a
browser. When the site is having trouble, this application cannot do better than
the site.

---

## "Not enough space in the download folder"

> Not enough space in the download folder: this download needs about 2.4 GB, and
> 1.1 GB is free.

The check compares the free space on the destination volume against three times
the expected size, and only when the source stated a size. Free some space, or
choose a folder on another drive in Settings.

The number is three times the file because a download that needs combining holds
the two input tracks and the combined output at once. A download whose size the
source did not state is not refused — the check says what it cannot know rather
than guessing.

---

## "The download folder could not be written to…"

or

> The download folder could not be created.

The destination does not exist and cannot be created, is read-only, is on a
disconnected network share, or you do not have permission to write there.

- Check the path in Settings → **Download directory**.
- Use **Browse…** to pick an existing folder.
- A folder on a removable or network drive that is not currently connected will
  fail this way.

> The download folder contains a link that leads outside it. Choose a different
> download folder.

The folder, or a subfolder inside it, is a symbolic link or a Windows junction
pointing somewhere else. The application refuses to write through it, because a
link is how a download ends up written outside the folder you chose. Pick a
different folder, or remove the link.

---

## The Library says a file is missing

The post is shown as no longer on disk, and is downloadable again. The file was
moved, renamed, or deleted outside the application — by you, by a cleanup tool,
or by emptying the Recycle Bin.

The Library reads the real filesystem, so this is a statement about the file, not
a stale entry. Download it again, or restore the file to where it was.

---

## Updates

**Settings → Updates says there is no update channel.** That is this build being
honest rather than broken. A build made with `npm run tauri:build` carries no feed
and no key to verify one with, so there is nowhere to check. Upgrading means
running a newer installer over this one; see
[personal use](PERSONAL_USE.md#4-upgrading-by-hand).

The two below can only arise in a build that *does* carry an update channel.
They are what the update mechanism does when it is switched on; none of them can
happen in the personal-use build, which has no channel to check.

**The download failed and mentions a signature.** The package did not verify
against the application's signing key. This is the check working: the file is not
the one the channel published, and it will not be installed. Do not try to work
around it — report it.

**The application does not restart after an update.** Start it from the Start
Menu. Your data is untouched. If it still will not start, run the same version's
installer by hand.

**You want to go back a version.** Run the older version's installer over the
installed one. Your settings, Library and downloads are not touched by installing
either way. A database a newer version has migrated may not open in an older one,
so copy the data directory first — see
[personal use](PERSONAL_USE.md#4-upgrading-by-hand).

---

## Installer problems

**"Windows protected your PC."** SmartScreen on an unsigned installer. The 1.0.0
installers are not code-signed, so there is no publisher for Windows to name.
Choose **More info → Run anyway** if you built the installer yourself, or hash the
file and compare against your build output — see
[personal use](PERSONAL_USE.md#windows-will-warn-you-about-the-publisher).

**The MSI asks for an administrator and you do not have one.** The MSI installs
for all users and needs elevation. The NSIS installer
(`…_x64-setup.exe`) installs for the current user and does not.

**Installation failed part-way.** Run the installer again; it is safe to rerun.
If it will not complete, uninstall from **Apps → Installed apps**, then install
again. Uninstalling does not remove your settings, history or downloads, so
nothing is lost by doing this.

**An old version is still there after upgrading.** Close the running application
first — Windows will not replace files a running program holds open. Then run the
new installer again.

---

## Where to look for detail

| | |
| --- | --- |
| Logs | `%APPDATA%\DouyinDownloader\DouyinDownloader\data\logs\` |
| More detail | set `RUST_LOG=debug` before launching |
| Database | `%APPDATA%\DouyinDownloader\DouyinDownloader\data\douyin-downloader.db` |
| Diagnostics | the Home screen: version, database health, resolvers, browser and FFmpeg availability |

Before sharing a log, read [Support](SUPPORT.md) — logs contain file paths and
page addresses, and it explains what to remove.
