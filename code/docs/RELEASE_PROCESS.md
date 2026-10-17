# Release process

How a version of Douyin Downloader is built, signed, described, and published —
and what has to be true before any of it happens.

Everything here is a real procedure that was run; where something could not be
done in this environment, it says so instead of describing what it would be.

> **This project is not published, and this procedure is not in use.** The
> application is built for personal use on one machine with `npm run tauri:build`,
> which needs no keys and no feed — see [personal use](PERSONAL_USE.md). What
> follows describes the machinery for signing and distributing a build, which is
> implemented and tested but dormant. Nothing here is required to install or use
> the application.

---

## 1. Versioning

The version is `MAJOR.MINOR.PATCH` and follows the ordinary semantic-versioning
reading of the application's user-visible behaviour:

| Change | Version |
| --- | --- |
| A change that can break a user's saved settings, library, or workflow, or a change to the supported source scope | `MAJOR` |
| A new feature, or a change to how an existing one behaves | `MINOR` |
| A fix that changes nothing about how the application is used | `PATCH` |

`1.0.0` is the first version of this application. There is no `0.x`
compatibility promise to keep, so the major number is 1 from the start and moves
only on a genuinely breaking change.

### One version, written down three times

The version appears in three files, and two of them are read by different tools:

| File | Who reads it |
| --- | --- |
| `src-tauri/Cargo.toml` | Rust. `CARGO_PKG_VERSION` is what the application reports about itself. |
| `src-tauri/tauri.conf.json` | The Tauri bundler. It is what the installer and the registry record, and what the updater compares against. |
| `package.json` | npm. Nothing else reads it, which is exactly why it goes stale unnoticed. |

Nothing in the toolchain keeps them in step, so two things do:

- `cargo test --test release_metadata` fails if they disagree, or if the version
  is not `MAJOR.MINOR.PATCH`. It runs with the rest of the suite.
- `npm run release` refuses to start if they disagree.

**To release a version, change all three.**

### Why the updater depends on this

The updater compares the version compiled into the running application (from
`Cargo.toml`, via `CARGO_PKG_VERSION`) with the version the release manifest
announces. If `tauri.conf.json` were ahead, the installer would record a version
the application does not report and the update would be offered forever. If it
were behind, the application would never be offered the update at all.

---

## 2. What the build needs

| Requirement | Version used for the 1.0.0 build | Notes |
| --- | --- | --- |
| Windows x64 | 10.0.26300 | The only target this release supports. |
| Rust | 1.99.0 (`cargo 1.99.0`) | `rust-version = "1.77"` is the declared minimum. |
| Node.js | 24.19.0 | |
| npm | 12.0.2 | |
| Visual Studio Build Tools | MSVC v143 (Visual Studio 2022 Community) | The Rust MSVC toolchain needs the linker and the Windows SDK. |
| WebView2 runtime | 154.0.4258.62 | Shipped with Windows 11 and current Windows 10. The installers install it if it is missing. |
| NSIS | fetched by the bundler into `%LOCALAPPDATA%\tauri\NSIS` | Not installed by hand. |
| WiX Toolset 3.14 | fetched by the bundler into `%LOCALAPPDATA%\tauri\WixTools314` | Not installed by hand. |

`Cargo.lock` and `package-lock.json` are committed, so the dependency graph is
the one that was tested rather than whatever is newest.

### Runtime dependencies the user needs

These are not build requirements; they are what the application needs in order to
do certain kinds of work, and the application reports them honestly when they are
missing. See the [User Guide](USER_GUIDE.md) and [Troubleshooting](TROUBLESHOOTING.md).

| Dependency | Needed for | Present on a clean Windows install? |
| --- | --- | --- |
| Chrome or Edge | Reading a Douyin page that will not hand its media to a plain HTTP client. Edge is on every supported Windows install. | Yes |
| FFmpeg and ffprobe | Combining a video track with its separately served audio track. | **No** — the user installs it, or such a download is refused by name. |
| WebView2 runtime | The application window itself. | Yes (Windows 11); the installer supplies it otherwise. |

---

## 3. Building a release

```bash
cd code
export TAURI_SIGNING_PRIVATE_KEY=/path/to/updater.key
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD='…'
export DOUYIN_UPDATER_PUBKEY=/path/to/updater.key.pub
export DOUYIN_RELEASE_BASE_URL=https://downloads.example.com/douyin-downloader/stable/
npm run release
```

`npm run release` ([`scripts/release.mjs`](../scripts/release.mjs)) does, in order:

1. Checks the three version copies agree, and that the version is
   `MAJOR.MINOR.PATCH`.
2. Checks the signing key, the public key, and the base URL are present. It stops
   if any is missing — nothing below this point can produce a releasable artifact
   without them.
3. Runs the Rust tests, `cargo clippy -- -D warnings`, the frontend tests and the
   TypeScript check. `--skip-checks` bypasses them for local iteration and is not
   for a release.
4. Builds the frontend and the Rust release binary, and produces the MSI and NSIS
   installers plus the signed updater packages.
5. Collects only the artifacts whose names carry **this** version, so a stale
   installer from an earlier release cannot be published under the current name.
6. Writes `release/SHA256SUMS.txt` and `release/latest.json`, and copies the
   artifacts beside them.
7. Prints what it produced. It does not upload anything.

A plain `npm run tauri:build` still works and produces the same installers with
no secrets at all. What it does not produce is the updater packages, because the
committed configuration carries no update feed and no key — see below.

### Why the release configuration is supplied separately

`tauri.conf.json` ships with `createUpdaterArtifacts: false` and an empty
endpoint list. That is deliberate:

- An application must not contact an update endpoint its owner did not choose.
- A build must not compile in somebody's key. The updater's public key is what
  decides which packages may be installed, so a build carrying the wrong one
  would either refuse every genuine update or accept somebody else's.
- A default build therefore reports that updating is not configured, rather than
  pointing somewhere convenient.

`npm run release` writes a small overlay configuration and passes it to
`tauri build --config`, which is how the real endpoint and the real public key
reach a release build and nothing else. The overlay lives in `release/` and is
never committed.

---

## 4. Signing

There are two unrelated things called "signing" here, and neither substitutes for
the other.

### Updater signing — implemented and required

Every updater package is signed with an Ed25519 key in minisign form, and the
application verifies it against the public key compiled into the build. A package
that does not carry a valid signature is refused; the download fails and nothing
is installed.

- The **private key** never enters version control. `.gitignore` excludes
  `*.key`, `*.pem`, `*.pfx`, `*.p12` and `code/.release-keys/`.
- The **public key** is what `DOUYIN_UPDATER_PUBKEY` supplies to a release build.
- The key is used through `TAURI_SIGNING_PRIVATE_KEY`, so it can come from a CI
  secret store without ever being written to disk.

Generating a key pair:

```bash
npx tauri signer generate -w path/to/updater.key
```

**Status for 1.0.0: no project key exists.** The workflow was implemented and
verified with a local test key, which is kept outside version control and is not
used by any build that could be published. The release owner has to generate the
project key and keep the private half in a secret store; until then no release
build can be produced.

### Windows code signing — not configured

Authenticode code signing is what makes Windows show a publisher name instead of
"Unknown publisher" and what keeps SmartScreen from warning on first run.

**Status for 1.0.0: no certificate.** No such certificate exists for this
project, none was invented, and the artifacts are therefore unsigned. Section 8
of the release report describes what a user sees because of it.

---

## 5. Release channels

| Channel | Purpose | Status |
| --- | --- | --- |
| **Stable** | Supported releases, if this application is ever distributed beyond one machine. | The channel such a release would use. **Not deployed, and not needed** — this is a personal-use build; no feed is hosted and none is compiled in. |
| Beta | Opt-in pre-release testing. | Planned. Not configured. |
| Dev | Builds for testing, not for users. | Planned. Not configured. |

There is no channel-switching control in the application, and that is
deliberate: a channel that exists only in a settings key is a channel that can be
pointed at a feed nobody verified. If Beta is introduced, it will be a separate
build with its own compiled-in endpoint and its own key, so that a Stable
installation cannot be moved to a Beta feed without installing a Beta build.

Each channel is expected to have:

- its own base URL, ending in `/stable/`, `/beta/` or `/dev/`;
- its own `latest.json`;
- the same signing key, or an explicitly separate one — never a key shared with a
  channel whose artifacts are not reviewed.

### What each channel's users see

- A feed that is reachable and has nothing newer: **"Version 1.0.0 is the newest
  available."**
- A feed that cannot be reached, or is not configured: **"The update check
  failed."** with the underlying reason. The application does not fall back to any
  other location, and never treats an unreachable feed as "up to date".
- An artifact whose signature does not verify: **"The update could not be
  downloaded."** Nothing is installed and the running version is unchanged.

---

## 6. Publishing

Nothing in this repository uploads anything. Publishing is a deliberate,
separate act:

1. Run `npm run release` with the real channel base URL and the real keys.
2. Upload the contents of `code/release/` to that base URL — the artifacts, their
   `.sig` files, and `latest.json`. The manifest's URLs are built from
   `DOUYIN_RELEASE_BASE_URL`, so it only works for that location.
3. Verify after uploading: fetch `latest.json` from the public URL and confirm the
   artifact URLs in it resolve and their sizes match `SHA256SUMS.txt`.
4. Only then announce the version.

### Withdrawing a faulty release

`latest.json` is the switch. Replacing it with a manifest naming the previous
version stops the faulty version being offered to anybody who has not already
taken it; an installed copy can be moved back by publishing that older version as
a newer-versioned manifest, because the updater installs whatever the manifest
says is newer than the running build. Deleting a bad artifact while a manifest
still points at it turns every check into a download failure, so the manifest goes
first.
