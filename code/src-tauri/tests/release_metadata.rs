//! The release identity, held to one value.
//!
//! The application version is written down in three places and read from two of
//! them, by two different tools: Cargo stamps `CARGO_PKG_VERSION` into everything
//! the Rust side reports, and the Tauri bundler takes the version it writes into
//! the installer and the registry from `tauri.conf.json`. `package.json` carries
//! a third copy that npm requires but nothing else reads.
//!
//! Nothing in the toolchain makes those agree. A release built with them out of
//! step installs as one version, reports another in its own About screen, and is
//! offered as an update to nobody — or offered forever, because the installed
//! version never reaches the published one. That is a class of bug which is
//! invisible until the update machinery is already in users' hands, and it is
//! cheap to catch here instead.

use std::fs;
use std::path::{Path, PathBuf};

/// One version, parsed far enough to check its shape.
#[derive(Debug, PartialEq, Eq)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        // A pre-release or build suffix (`1.0.0-rc.1`) is a valid semver, but
        // this project has no use for one yet, and accepting it here would mean
        // silently accepting it in an installer path too.
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The `version` string in a JSON file, read as text.
///
/// Parsed rather than pattern-matched so a comment, a nested field of the same
/// name, or a differently-shaped file cannot quietly satisfy the check.
fn version_in(path: &Path) -> String {
    let raw = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("{} must be readable: {err}", path.display()));
    let json: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|err| panic!("{} must be valid JSON: {err}", path.display()));
    json.get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("{} must carry a top-level `version`", path.display()))
        .to_string()
}

#[test]
fn the_version_is_valid_semver() {
    let version = env!("CARGO_PKG_VERSION");
    assert!(
        Version::parse(version).is_some(),
        "the package version {version:?} is not MAJOR.MINOR.PATCH, which is what the \
         installers and the updater compare by"
    );
}

#[test]
fn every_place_that_names_the_version_names_the_same_one() {
    let cargo = env!("CARGO_PKG_VERSION").to_string();
    let tauri = version_in(&manifest_dir().join("tauri.conf.json"));
    let npm = version_in(&manifest_dir().join("..").join("package.json"));

    assert_eq!(
        tauri, cargo,
        "tauri.conf.json says {tauri} but Cargo.toml says {cargo}: the installer would \
         record one version while the application reported another"
    );
    assert_eq!(
        npm, cargo,
        "package.json says {npm} but Cargo.toml says {cargo}: npm's copy has no reader, \
         which is exactly why it goes stale unnoticed"
    );
}

#[test]
fn the_release_version_is_at_least_the_first_public_one() {
    // A guard against the version being reset to 0.x while the release docs and
    // the updater manifest describe it as the first stable release.
    let version = Version::parse(env!("CARGO_PKG_VERSION")).expect("checked above");
    assert!(
        version.major >= 1,
        "this is the first public release, so the major version is 1 or more"
    );
}
