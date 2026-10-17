# Installing and keeping this build up to date (personal use)

This build is **not published and not distributed**. It is a Windows x64
application built from this repository to be installed and used on the machine
that built it. Nothing is uploaded anywhere, no update channel is deployed, and
the installers are not signed with a Windows code-signing certificate.

Everything below assumes you built the installer yourself, or are installing one
you built. If you are looking for a public download, there is not one.

---

## 1. What you need

| | |
| --- | --- |
| Windows | x64. Windows 11 or a current Windows 10. |
| WebView2 runtime | Already present on Windows 11 and current Windows 10. The installer supplies it if it is missing. |
| Chrome or Edge | Recommended. Some Douyin pages will not hand their media to a plain HTTP client, and browser-assisted resolution is what reads those. Edge is on every supported Windows install. |
| FFmpeg and ffprobe | Optional. Only needed when a post serves its sound as a separate track. Without it, those downloads are refused with a message naming FFmpeg rather than saved silently without sound. |

The application reports on the Home screen whether the browser and FFmpeg were
found, so you do not have to guess.

---

## 2. Building and installing

From the repository:

```bash
cd code
npm install
npm run tauri:build
```

That produces, under `code/src-tauri/target/release/bundle/`:

| File | What it is |
| --- | --- |
| `nsis/Douyin Downloader_1.0.0_x64-setup.exe` | Per-user install. No administrator rights, no elevation prompt. **Use this one.** |
| `msi/Douyin Downloader_1.0.0_x64_en-US.msi` | Per-machine install for managed machines. Asks for elevation. |

Run the `.exe`. It installs to `%LOCALAPPDATA%\Douyin Downloader` and creates a
Start Menu entry. There is no desktop shortcut unless you ask for one on the
installer's last page.

`npm run tauri:build` needs no keys and no secrets. `npm run release` is a
different command that additionally signs updater packages and writes a channel
manifest; it is for publishing, which this build does not do.

### Windows will warn you about the publisher

The installers are **not code-signed**. No certificate exists for this project
and none was invented, so Windows cannot show a publisher name. You will see
**"Unknown publisher"** on the elevation prompt, and SmartScreen may show
*"Windows protected your PC"* with a **More info → Run anyway** link on first
run.

That warning is expected for an unsigned build and is not a sign that the file
is damaged. What it does **not** tell you is who built it — signing is the only
thing that would. If you want to check that the file is the one you built, hash
it yourself and compare against the build output:

```bash
certutil -hashfile "Douyin Downloader_1.0.0_x64-setup.exe" SHA256
```

---

## 3. Where your data lives

| Path | What is in it |
| --- | --- |
| `%APPDATA%\DouyinDownloader\DouyinDownloader\data\douyin-downloader.db` | Settings, Library, download queue and history. |
| `…\data\douyin-downloader.db-wal`, `…-shm` | SQLite's write-ahead log. Part of the database while the application is running. |
| `…\data\logs\` | Log files. |
| The download folder you chose in Settings | The downloaded files themselves. |

Nothing else is stored, and none of it is sent anywhere. See
[Privacy](PRIVACY.md).

**Your downloaded files are never inside the data directory.** Deleting the data
directory loses your settings and history and leaves your files alone; deleting
your files does the same in reverse.

---

## 4. Upgrading by hand

There is no in-app update check in this build — Settings → **Updates** says so,
rather than offering a check that could only fail. Installing a newer build over
the old one is the whole procedure:

1. Close the application.
2. Run the newer `…-setup.exe`.
3. Launch it again.

What happens to your data:

- **Settings, Library and download history are preserved.** The installer does
  not touch the data directory.
- **The database is migrated forward on first launch**, automatically, inside a
  transaction. If the application is interrupted mid-migration, the migration is
  retried on the next launch rather than leaving a half-updated database.
- **Downloaded files are not moved, renamed or deleted.**
- **The version recorded by Windows** (`Settings → Apps → Installed apps`) is
  updated to the new build.

Uninstalling removes the program and the Start Menu entry. It does **not**
remove the data directory, so a reinstall picks your settings and history back
up. To remove those too, delete `%APPDATA%\DouyinDownloader` yourself.

### If you need to go back to an older build

Database migrations are forward-only. A database that a newer version has
migrated may not open in an older one. If you think you may downgrade, copy the
data directory **before** upgrading.

---

## 5. Backing up settings and history

Close the application first — that is what checkpoints the write-ahead log into
the database file.

Then copy the whole data directory:

```powershell
Copy-Item -Recurse "$env:APPDATA\DouyinDownloader" "$env:USERPROFILE\Desktop\douyin-downloader-backup"
```

To restore, close the application and copy it back over the same path. A backup
taken with the application running may be missing its most recent writes, which
is why the first step is closing it.

Your downloaded media is not in that directory. Back it up separately, from
wherever you told the application to save it.

---

## 6. What this build deliberately does not do

- **It does not update itself.** There is no feed to check and no key compiled
  in to verify one with. Upgrades are the manual procedure in section 4.
- **It does not phone home.** No telemetry, no analytics, no crash reporting, no
  usage statistics. See [Privacy](PRIVACY.md).
- **It downloads only what a logged-out visitor can see.** Private, deleted,
  region-restricted or challenged content is not available, and Douyin's pages
  change without notice, so not every link works and none of them are guaranteed
  to keep working.
- **It is built for one machine.** Only Windows x64 is targeted.

The update mechanism itself is implemented and tested — the code is in the
repository and `npm run release` exercises it — but it is dormant in this build
because no channel is configured. See [RELEASE_PROCESS](RELEASE_PROCESS.md) if
you ever want to turn it on.
