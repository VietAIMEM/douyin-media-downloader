# User guide

How to use Douyin Downloader, screen by screen.

The application downloads **publicly accessible** Douyin media. It does not sign
in, does not use your account or your browser's session, and does not attempt to
get past anything that stops it — if Douyin will not serve something to an
ordinary anonymous visitor, this application will not have it either. See
[What it will not do](#what-it-will-not-do).

---

## Installing and launching

Download the installer for **Windows x64** and run it.

- `Douyin Downloader_1.0.0_x64-setup.exe` — the recommended installer. It
  installs **for the current user**, so it does not need an administrator and
  does not modify anything outside your own account.
- `Douyin Downloader_1.0.0_x64_en-US.msi` — the same application as an MSI
  package, for machines managed by an administrator. The MSI installs for all
  users and will ask for elevation.

Either installer creates a Start Menu entry under **Douyin Downloader**. There is
no desktop shortcut.

**Windows will warn you that the publisher is unknown.** The 1.0.0 installers are
not code-signed, so Windows names no publisher and SmartScreen may offer a
**More info → Run anyway** link on first run. The warning is about the absence of
a certificate, not about the file. If you would rather check the file than trust
it, hash it yourself and compare against the output of the build that produced
it — see [personal use](PERSONAL_USE.md#windows-will-warn-you-about-the-publisher).

### What you need

| | |
| --- | --- |
| **Chrome or Edge** | Recommended. Edge is already on every supported Windows install. Some Douyin pages only hand their media to a real browser; without one, those pages cannot be read. |
| **FFmpeg** | Optional. Douyin sometimes serves a video's sound as a separate track, and putting the two back together needs FFmpeg. Without it, such a download is refused with a message naming FFmpeg rather than saving a silent video. Everything else works. |

The Home screen reports whether each of these was found.

---

## Home

Where the application's status lives: the version you are running, whether the
database is healthy, which resolvers are registered, and whether browser-assisted
resolution and media muxing are available. If something is not working, this is
the first screen to look at.

---

## Single URL

For one post.

1. Paste a Douyin link into **Douyin link** — a share link
   (`https://v.douyin.com/…`) or a direct post link.
2. Press **Analyze**. The application reads the page and shows what it found.
3. Choose the media you want. A post with several images, or with several video
   renditions, lists them all; tick the ones you want.
4. Press **Download**.

The media is added to the Downloads queue and the files are written to your
download folder.

---

## Profile Downloader

For a creator's posts.

1. Paste a profile link into **Profile URL**.
2. Press **Load profile**. This identifies the creator and reads the first page
   of their posts.
3. Browse the posts, select the ones you want, and use **Download selected**. You
   can select the whole page.
4. If the creator has more posts, load the next page and select from it.

A profile link is never treated as a single downloadable post. If a link is a
profile, the Single URL screen says so instead of failing in a confusing way.

Selecting many posts submits them all to the same queue, subject to the
concurrency limit in Settings.

---

## Downloads

Everything queued and in progress.

Each row shows what it is, how far it has got, how many attempts it has had, and
its state:

| State | Meaning |
| --- | --- |
| **Queued** | Waiting for a free slot. |
| **Downloading** | Transferring now. Progress, speed, and an estimated time remaining appear once there is enough of a sample to measure one. |
| **Paused** | Stopped, with what it had already written kept. |
| **Completed** | The file is on disk and in your Library. |
| **Failed** | It did not work. The row says why. |
| **Cancelled** | Stopped on purpose. |

Controls:

- **Pause** and **Resume** a task. Pausing stops the transfer immediately and
  keeps the bytes already received.
- **Cancel** stops a task and discards what it had written.
- **Retry** a failed task.
- **Clear completed** removes finished rows from the list. It asks first, and it
  does not delete files.
- **Open folder** on a finished download shows the file in Explorer.

Progress is reported as it happens, so a large queue stays responsive while it
runs.

### About retrying and resuming

A download that fails in a way that could work next time is retried
automatically, with a growing delay between attempts, up to the attempt limit in
Settings. A download that failed for a reason that will not change — the post is
gone, the source refused it — is not retried.

**Resuming does not always continue where it left off.** The application asks the
source to continue from what it already has, and only does so when the source can
prove the file has not changed underneath it. When it cannot — a different
version, no way to tell, or a source that does not support it — the download
starts again from the beginning rather than risk producing a file assembled from
two different versions. Starting over is deliberate; see the
[FAQ](FAQ.md#why-did-my-download-start-over-instead-of-resuming).

---

## Library

What you have downloaded, browsed and searched locally.

- **View tabs** narrow the list: everything, or only what is new, ready, or
  already downloaded.
- **Search the library** matches creator, caption, post id, URL, and filename.
- Filters narrow by type, length, and size, and by creator.
- Sort by published date, duration, or size.
- **Export JSON** and **Export CSV** write the current selection's metadata into
  your download folder.

An entry whose file has been deleted or moved outside the application is shown as
no longer on disk, and becomes downloadable again.

---

## History

Every download that finished, failed, or was cancelled, with its outcome, the
time, the file size, and where it was saved. History is a record; removing an
entry does not delete the file.

---

## Settings

### Download folder and file naming

- **Download directory** — where files are written. **Browse…** opens the
  Windows folder picker; you can also type a path, including one on another
  drive. **Use default** returns to `Downloads\Douyin` inside your user folder.
- **File name template** — the name each file gets, without the extension. The
  available placeholders are listed under the field.
- **Folder template** — the subfolders under the download folder. Use `/`
  between segments; `{type}` becomes Videos, Images, or Audio. Leave it empty to
  write straight into the download folder.
- **Conflict policy** — what happens when a file of that name is already there:
  rename the new file, skip it, or overwrite it.

### Downloads

- **Maximum concurrent downloads** — how many transfers run at once.
- **Retry attempts** — how many times a retryable failure is tried.
- **Request timeout** — how long to wait on a source.

### Application

- **Browser-assisted resolution** — whether the application may open a page in
  an installed Chrome or Edge when the plain HTTP path cannot read it. It uses a
  fresh, throwaway browser profile: no cookies, no history, no sign-in.
- **Theme**, **log level**, and whether to write a log file.

### Updates

Shows the version you are running. This build carries no update channel, so it
says so rather than offering a check — there is nowhere for it to look. See
[Updating](#updating) below.

---

## Updating

This build does not update itself. Settings → **Updates** states that plainly
instead of offering a check whose only possible outcome would be a failure.

1. Close the application.
2. Run the newer build's installer over this one.
3. Launch it again.

Your settings, Library, download history and downloaded files are not affected.
The database is migrated forward automatically on the first launch of the new
version. See
[installing and keeping this build up to date](PERSONAL_USE.md#4-upgrading-by-hand),
including how to back up your data and what to do if you ever want to go back to
an older build.

---

## Where your files and data are

| What | Where |
| --- | --- |
| Downloaded media | Your download folder — `%USERPROFILE%\Downloads\Douyin` unless you changed it. Organised into subfolders if a folder template is set. |
| Settings, Library, download history, queue state | `%APPDATA%\DouyinDownloader\DouyinDownloader\data\douyin-downloader.db` |
| Logs | `%APPDATA%\DouyinDownloader\DouyinDownloader\data\logs\` |
| Temporary browser profile | The system temporary folder, created for one render and removed afterwards. |

Uninstalling the application does **not** delete any of these, and never deletes
your downloaded media. See the
[FAQ](FAQ.md#does-uninstalling-delete-my-downloaded-files).

---

## What it will not do

- It does not sign in, use your account, or read cookies from your browser.
- It does not solve challenges, defeat anti-bot measures, or fetch anything a
  logged-out visitor could not.
- It does not download private, friends-only, or deleted content.
- It does not promise that every Douyin link works. Douyin changes its pages
  without notice, and a page that no longer exposes its media to a browser will
  not be readable here either.
- It does not remove watermarks from anything.

Downloading someone else's work does not make it yours. Check what you are
allowed to do with a video before you redistribute it.
