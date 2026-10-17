//! The assembled release, checked with the machinery that will consume it.
//!
//! This is the only place where "the release is signed" is more than a claim
//! about a file existing. The verification below is run by `minisign-verify` —
//! the crate the updater plugin calls to decide whether a downloaded package may
//! be installed — over the artifacts `npm run release` actually produced, with
//! the public key a release build compiles in. If this passes, an installed copy
//! would accept these packages. If a byte of the package is changed, or a
//! different key is used, it fails here for the same reason it would fail there.
//!
//! Ignored by default, because it needs a release build to have been run:
//!
//! ```text
//! DOUYIN_UPDATER_PUBKEY=../.release-keys/test.key.pub \
//!   cargo test --test release_artifacts -- --ignored --nocapture
//! ```
//!
//! It reads `release/latest.json` and the artifacts beside it, and the public
//! key named by `DOUYIN_UPDATER_PUBKEY` — a path to a `.pub` file, or the key
//! itself.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use minisign_verify::{PublicKey, Signature};
use tauri_plugin_updater::RemoteRelease;

const TARGETS: [&str; 2] = ["windows-x86_64-nsis", "windows-x86_64-msi"];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate lives inside the workspace")
        .to_path_buf()
}

fn release_dir() -> PathBuf {
    std::env::var_os("DOUYIN_RELEASE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("release"))
}

/// The public key a release build compiles in.
///
/// In the plugin's configuration this is the base64 text of a minisign public
/// key file — which is what `.pub` holds — so the same value reaches the test as
/// would be baked into the build.
fn public_key() -> String {
    let raw = std::env::var("DOUYIN_UPDATER_PUBKEY").unwrap_or_else(|_| {
        panic!(
            "DOUYIN_UPDATER_PUBKEY must name the public key the release was signed for, \
             as a path to a .pub file or as the key itself"
        )
    });
    let path = Path::new(&raw);
    if path.is_file() {
        return fs::read_to_string(path)
            .expect("the public key file must be readable")
            .trim()
            .to_string();
    }
    raw.trim().to_string()
}

/// The manifest, parsed by the updater's own type.
///
/// Not by a struct written here: the point is that the file's shape *is* the
/// shape the plugin accepts, and only the plugin's own deserializer can say so.
/// A manifest a hand-written struct would happily read and the updater would
/// reject is precisely the failure this rules out.
fn manifest() -> RemoteRelease {
    let path = release_dir().join("latest.json");
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} must be readable: {err}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|err| {
        panic!(
            "{} is not a manifest the updater can parse: {err}",
            path.display()
        )
    })
}

/// The artifact and signature the manifest names for one platform.
fn artifact_for(manifest: &RemoteRelease, target: &str) -> (PathBuf, String) {
    let signature = manifest
        .signature(target)
        .unwrap_or_else(|err| panic!("the manifest must describe {target}: {err}"))
        .clone();
    let url = manifest
        .download_url(target)
        .unwrap_or_else(|err| panic!("{target} must name a download URL: {err}"));

    // The URL is what an installed copy will fetch, so the file this test reads
    // has to be the file that URL names — not merely something with a similar
    // name in the same directory.
    let name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .expect("a URL with a file name");
    let artifact = release_dir().join(percent_decode(name).expect("a plain file name"));
    (artifact, signature)
}

/// Undo the percent-encoding the manifest applies to a name with spaces in it.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Verify exactly as the updater does.
///
/// Both values arrive base64-encoded, and the plugin decodes that layer before
/// parsing the minisign text inside — see `base64_to_string` in the plugin. The
/// `true` is `allow_legacy`, which is what the plugin passes; verifying with
/// anything else would be a weaker test than the updater itself.
fn verify(artifact: &[u8], signature: &str, key: &str) -> Result<(), String> {
    let decoded_key = STANDARD
        .decode(key.trim())
        .map_err(|err| format!("the public key is not base64: {err}"))?;
    let decoded_signature = STANDARD
        .decode(signature.trim())
        .map_err(|err| format!("the signature is not base64: {err}"))?;

    let public_key = PublicKey::decode(
        std::str::from_utf8(&decoded_key).map_err(|err| format!("the key is not text: {err}"))?,
    )
    .map_err(|err| format!("the public key does not parse: {err}"))?;
    let signature = Signature::decode(
        std::str::from_utf8(&decoded_signature)
            .map_err(|err| format!("the signature is not text: {err}"))?,
    )
    .map_err(|err| format!("the signature does not parse: {err}"))?;

    public_key
        .verify(artifact, &signature, true)
        .map_err(|err| err.to_string())
}

/// SHA-256 of a file, in the lower-case hex `sha256sum` writes.
///
/// The platform's own hasher rather than one written here: a hand-rolled digest
/// would be a poor thing to trust, and a test that checks published checksums
/// with the same code that published them checks nothing.
fn sha256_hex(path: &Path) -> String {
    let output = Command::new("certutil")
        .arg("-hashfile")
        .arg(path)
        .arg("SHA256")
        .output()
        .unwrap_or_else(|err| panic!("certutil must run: {err}"));
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .map(str::trim)
        .find(|line| line.len() == 64 && line.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| {
            panic!(
                "certutil produced no digest for {}: {}",
                path.display(),
                text.trim()
            )
        })
        .to_ascii_lowercase()
}

#[tokio::test]
#[ignore = "needs a release build: run `npm run release` first"]
async fn the_manifest_is_one_the_updater_can_read() {
    let manifest = manifest();
    assert_eq!(
        manifest.version.to_string(),
        env!("CARGO_PKG_VERSION"),
        "the channel announces a different version than this build reports"
    );
    assert!(
        manifest.notes.is_some(),
        "the manifest should carry release notes, so an update can describe itself before it \
         is installed"
    );

    for target in TARGETS {
        let (artifact, _) = artifact_for(&manifest, target);
        assert!(
            artifact.is_file(),
            "{target} names {} but there is no such file",
            artifact.display()
        );
    }
}

#[tokio::test]
#[ignore = "needs a release build: run `npm run release` first"]
async fn every_release_artifact_verifies_against_the_shipped_key() {
    let manifest = manifest();
    let key = public_key();

    for target in TARGETS {
        let (artifact, signature) = artifact_for(&manifest, target);
        let bytes = fs::read(&artifact).expect("the artifact must be readable");
        verify(&bytes, &signature, &key).unwrap_or_else(|err| {
            panic!(
                "{} does not verify against the configured key: {err}",
                artifact.display()
            )
        });
        println!("verified {target}: {} bytes", bytes.len());
    }
}

#[tokio::test]
#[ignore = "needs a release build: run `npm run release` first"]
async fn each_signature_names_the_version_it_was_made_for() {
    // The updater is configured with `requireSignedVersion`, which refuses a
    // package whose signature does not name the version the manifest announces.
    // Without it, the valid signature for an older release could be served under
    // a newer version's name.
    let manifest = manifest();
    let key = public_key();

    for target in TARGETS {
        let (artifact, signature) = artifact_for(&manifest, target);
        let decoded = STANDARD
            .decode(signature.trim())
            .expect("the signature is base64");
        let parsed = Signature::decode(std::str::from_utf8(&decoded).expect("text"))
            .expect("the signature parses");
        let comment = parsed.trusted_comment();
        let signed_version = comment
            .split('\t')
            .find_map(|field| field.strip_prefix("version:"))
            .unwrap_or_else(|| {
                panic!("{target}: the trusted comment names no version: {comment}")
            });
        assert_eq!(
            signed_version,
            manifest.version.to_string(),
            "{target}: signed for a different version than the manifest announces"
        );

        // The check above is only worth anything because the signature really
        // covers that comment, which is what verifying establishes.
        let bytes = fs::read(&artifact).expect("the artifact must be readable");
        verify(&bytes, &signature, &key).expect("the artifact verifies");
    }
}

#[tokio::test]
#[ignore = "needs a release build: run `npm run release` first"]
async fn a_tampered_artifact_does_not_verify() {
    let manifest = manifest();
    let key = public_key();
    let (artifact, signature) = artifact_for(&manifest, "windows-x86_64-nsis");

    let mut bytes = fs::read(&artifact).expect("the artifact must be readable");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;

    let outcome = verify(&bytes, &signature, &key);
    assert!(
        outcome.is_err(),
        "a modified installer verified against the release signature, which would mean the \
         signature covers nothing"
    );
    println!("tampered artifact refused: {}", outcome.unwrap_err());
}

#[tokio::test]
#[ignore = "needs a release build: run `npm run release` first"]
async fn a_different_key_does_not_verify_the_release() {
    // The failure that matters most: a package signed by somebody else must not
    // be installable. The key here is a second test key, generated the same way
    // and committed as a fixture — a public key is not a secret — so this is a
    // real, well-formed key that simply is not the release's.
    let manifest = manifest();
    let (artifact, signature) = artifact_for(&manifest, "windows-x86_64-nsis");
    let other_key = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/updater-other-test-key.pub"),
    )
    .expect("the fixture key must be readable");

    let bytes = fs::read(&artifact).expect("the artifact must be readable");
    let outcome = verify(&bytes, &signature, other_key.trim());
    assert!(
        outcome.is_err(),
        "an artifact verified against a key that did not sign it"
    );
    println!("signed by another key, refused: {}", outcome.unwrap_err());

    // And the release's own key still accepts it, so the refusal above is about
    // the key rather than about the artifact.
    verify(&bytes, &signature, &public_key()).expect("the release's own key verifies it");
}

#[tokio::test]
#[ignore = "needs a release build: run `npm run release` first"]
async fn the_published_checksums_match_the_artifacts() {
    let path = release_dir().join("SHA256SUMS.txt");
    let listing = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} must be readable: {err}", path.display()));

    let mut checked = 0;
    for line in listing.lines().filter(|line| !line.trim().is_empty()) {
        let (expected, name) = line
            .split_once("  ")
            .unwrap_or_else(|| panic!("unrecognised checksum line: {line}"));
        let file = release_dir().join(name);
        let actual = sha256_hex(&file);
        assert_eq!(
            actual, expected,
            "{name} does not match the checksum published beside it"
        );
        checked += 1;
    }

    assert!(
        checked >= 4,
        "the listing should cover both installers and their signatures, found {checked}"
    );
    println!("{checked} artifacts match their published checksums");
}
