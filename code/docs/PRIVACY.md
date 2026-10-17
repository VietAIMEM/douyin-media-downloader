# Privacy

What this application stores, what it sends, and what it does not do. It is
written to be checkable against the application rather than taken on trust.

---

## The short version

- Everything the application knows about you is on your own computer.
- It contacts Douyin, because that is what it is for. That is the only thing it
  ever talks to — this build carries no update channel and checks for nothing.
- There is no telemetry, no analytics, and no crash reporting. Nothing about your
  use of the application is sent anywhere.
- It never signs in to Douyin, and never reads or copies your browser's cookies,
  history, or saved sessions.

---

## What is stored, and where

Everything is under your own user profile. Nothing is stored anywhere else.

| What | Where | Why it exists |
| --- | --- | --- |
| **Settings** | `%APPDATA%\DouyinDownloader\DouyinDownloader\data\douyin-downloader.db` | Your download folder, naming templates, limits, theme, and preferences. |
| **Library** | same database | For each post you have analyzed: the creator's name and handle, the caption, the post id and URL, the publication date, media types and variants, and whether a local copy exists. |
| **Download history** | same database | The outcome of each download, when it happened, its size, and where it was saved. |
| **Queue state** | same database | Which downloads are waiting, running, paused, or finished, so an interrupted queue can be recovered when the application restarts. |
| **Logs** | `%APPDATA%\DouyinDownloader\DouyinDownloader\data\logs\` | What the application did, for diagnosis. |
| **Downloaded media** | Your download folder | The files themselves. |
| **Temporary browser profile** | System temporary folder | Created for one page render and deleted afterwards. |

### About the browser profile

When a page needs a real browser, the application starts Chrome or Edge against a
**brand-new, empty profile** in the temporary folder. It is not your browser
profile: no cookies, no history, no saved passwords, no extensions, and no
sign-in. The directory is removed when the render finishes, including when it
fails.

---

## What is sent over the network

### Douyin

The application fetches Douyin pages and media in order to download them. Those
requests go to Douyin's servers and carry the ordinary information any HTTP
request carries — your IP address, and a user agent identifying the application —
and whatever the page you asked for requires. Douyin's own privacy policy applies
to what Douyin does with them.

**Nothing is sent to Douyin about you.** There is no account, no session, and no
identifier that ties one download to another or to you.

The application asks for a Douyin share page with a mobile browser's user agent
in one case, because that is the form of the page that contains the post's data
rather than a JavaScript shell. This is a request-format choice for a public page,
not an attempt to get past anything.

### The update channel

**This build has none, so it never asks anybody about updates.** Settings →
**Updates** shows the version you are running and says there is no channel to
check.

That is not merely a setting: the address a release build would check is compiled
in, and this build has none and carries no key to verify a package with. The
mechanism exists in the code and is tested, but nothing in this build will reach
for it. If you later build with a channel configured, that build would fetch a
small manifest when — and only when — you pressed the check, and the operator of
that address would see the request itself, your IP address and the user agent, as
any web server does. Nothing about your library, your downloads or your settings
would be sent with it.

The channel's address is compiled into the build. The committed configuration
contains no address at all, so a default build never contacts one.

### What is never sent

- No usage statistics, no feature counts, no "anonymous diagnostics".
- No crash reports. If the application fails, that fact stays in your local log.
- No list of what you downloaded, searched for, or have in your Library.
- No file paths from your machine.
- No tokens, cookies, or credentials — the application does not have any to send.

**If you are asked to send a log for support, it is you sending it, deliberately,
and you can read it first.** See [Support](SUPPORT.md).

---

## Signed media URLs in logs

Douyin's media links carry a signature in the query string which is, in effect, a
short-lived credential. The application writes the scheme, host, and path of a
request to its log, and **never the query string**, so a log file cannot be used
to fetch media or to recover a token. This is enforced in one place in the code
and covered by tests.

---

## What is deliberately not stored

- **Your browser's cookies or sessions.** The application never reads them.
- **The URL of a partial download.** A partial file's sidecar records the
  resource's validator and size, never the signed URL it came from.
- **The contents of pages.** Only the structured facts the Library needs.
- **Anything from a page you did not ask about.**

---

## Removing your data

Nothing is deleted behind your back, and nothing is deleted for you.

| To remove | Do this |
| --- | --- |
| Downloaded media | Delete the files from your download folder. |
| Settings, Library, history, queue | Close the application, then delete `%APPDATA%\DouyinDownloader\DouyinDownloader\data\douyin-downloader.db` (and any `-wal` / `-shm` files beside it). The application creates a fresh one next time it starts. |
| Logs | Delete `%APPDATA%\DouyinDownloader\DouyinDownloader\data\logs\`. |
| The application itself | Uninstall from **Apps → Installed apps**. This removes only the program files and shortcuts — it does not touch any of the rows above. |

Uninstalling never deletes your downloaded media or your database. That is
deliberate: an uninstall that silently removed a folder you chose is an uninstall
that could remove files you still want.

**To keep your data**, copy the database file and your download folder. Those two
are everything.

---

## If crash reporting is ever added

There is none today. If it were added it would have to be opt-in, would have to
say exactly what it collects, and would have to exclude paths, library contents,
and page addresses. Until such a thing exists and is documented here, the
statement stands: **this application sends nothing about your use of it to
anybody.**
