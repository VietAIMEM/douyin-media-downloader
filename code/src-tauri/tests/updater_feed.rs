//! The updater, driven the way an installed copy drives it.
//!
//! `release_artifacts.rs` verifies the published files with the verifier the
//! updater calls. This goes through the updater itself: a real
//! `tauri_plugin_updater::Updater` is built, pointed at a feed this test serves
//! over loopback, and asked to check and download. The HTTP fetch, the manifest
//! parse, the download and the signature check are the plugin's own code, not a
//! re-implementation of it that agrees with itself.
//!
//! The negative case is the one worth having: a package with a single byte
//! changed must be refused *after* it has been downloaded. That is what stops a
//! tampered feed from installing anything.
//!
//! Ignored by default, because it needs a release build to have been run:
//!
//! ```text
//! DOUYIN_UPDATER_PUBKEY=../.release-keys/test.key.pub \
//!   cargo test --test updater_feed -- --ignored --nocapture
//! ```

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use tauri_plugin_updater::{RemoteRelease, UpdaterExt};
use url::Url;

const TARGET: &str = "windows-x86_64-nsis";

/// A served route: path, body, content type.
type Route = (&'static str, Vec<u8>, &'static str);

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

/// The public key a release build compiles in, as a path to a `.pub` file or the
/// key itself.
fn public_key() -> String {
    let raw = std::env::var("DOUYIN_UPDATER_PUBKEY").unwrap_or_else(|_| {
        panic!("DOUYIN_UPDATER_PUBKEY must name the public key the release was signed for")
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

/// The release's NSIS installer, read from wherever the manifest says it lives.
fn installer() -> Vec<u8> {
    let listing = fs::read_to_string(release_dir().join("latest.json"))
        .expect("release/latest.json must exist — run `npm run release` first");
    let manifest: RemoteRelease = serde_json::from_str(&listing)
        .expect("release/latest.json must be a manifest the updater can parse");

    let url = manifest
        .download_url(TARGET)
        .expect("the NSIS target must name a download URL");
    let name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .expect("a URL with a file name");
    fs::read(release_dir().join(percent_decode(name).expect("a plain file name")))
        .expect("the installer must be readable")
}

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

/// The release's manifest, with the artifact URL pointed at this test's feed.
///
/// Everything else is the release's own announcement — the version, the notes,
/// the signature — so what the updater accepts here is what it would accept
/// from a real channel. `version` overrides the announced version, for the case
/// where the announcement and the signature are made to disagree.
///
/// The entry is served under two target names. An installed copy knows which
/// installer produced it and asks for `windows-x86_64-nsis`; a test binary was
/// not installed by one, so the updater falls back to the bare `windows-x86_64`
/// and would otherwise find nothing.
fn manifest(port: u16, version: Option<&str>) -> String {
    let raw = fs::read_to_string(release_dir().join("latest.json"))
        .expect("release/latest.json must exist — run `npm run release` first");
    let mut value: serde_json::Value =
        serde_json::from_str(&raw).expect("release/latest.json must be JSON");

    if let Some(version) = version {
        value["version"] = serde_json::json!(version);
    }

    let platforms = value
        .get_mut("platforms")
        .and_then(|platforms| platforms.as_object_mut())
        .expect("the release manifest must describe platforms");
    let announced = platforms
        .get(TARGET)
        .cloned()
        .expect("the release manifest must describe the NSIS target");

    let mut local = announced;
    local["url"] = serde_json::json!(format!("http://127.0.0.1:{port}/artifact"));

    platforms.insert(TARGET.to_string(), local.clone());
    platforms.insert("windows-x86_64".to_string(), local);
    value.to_string()
}

/// Serve what `build` returns on a loopback port, and return that port.
///
/// The port is only known once the listener is bound, and the manifest has to
/// name it, so the routes are built from it rather than beside it.
fn start_feed(build: impl FnOnce(u16) -> Vec<Route>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("a bound address").port();
    let routes = build(port);

    thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut stream) = incoming else { break };
            let mut reader = match stream.try_clone() {
                Ok(clone) => BufReader::new(clone),
                Err(_) => continue,
            };

            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            // Drain the headers; the updater sends no body.
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) if line == "\r\n" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }

            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_string();
            let (status, body, content_type) =
                match routes.iter().find(|(route, ..)| *route == path) {
                    Some((_, body, content_type)) => ("200 OK", body.clone(), *content_type),
                    None => ("404 Not Found", b"no such route".to_vec(), "text/plain"),
                };

            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        }
    });

    port
}

/// An app with the updater plugin in it, as the application has.
///
/// It is built from the application's own `tauri.conf.json` rather than a mock
/// context, because the updater reads its configuration — including
/// `requireSignedVersion`, which is what binds a package to the version it was
/// signed for — at plugin setup. A mock context carries no plugin configuration
/// at all, so a test built on one would be running a differently configured
/// updater than the application ships.
fn app() -> tauri::App<tauri::test::MockRuntime> {
    tauri::test::mock_builder()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .build(tauri::generate_context!())
        .expect("the application under test must start")
}

/// Look at `port` and hand back the update it announces.
///
/// The version gate is opened deliberately: this build *is* the announced
/// version, so the default "only replace me with something newer" rule would
/// decline before any of the transfer or verification code ran. What is under
/// test here is fetching, parsing, downloading and verifying — not the
/// comparison, which the plugin does the same way for every application.
async fn check(
    app: &tauri::App<tauri::test::MockRuntime>,
    port: u16,
) -> tauri_plugin_updater::Update {
    app.handle()
        .updater_builder()
        .endpoints(vec![Url::parse(&format!(
            "http://127.0.0.1:{port}/latest.json"
        ))
        .expect("a loopback URL")])
        // A test build only warns about a plain-HTTP endpoint; a release build
        // refuses it outright, which is why the feed cannot be an `https` one
        // here without a certificate the operating system trusts.
        .expect("a loopback endpoint is allowed in a test build")
        .pubkey(public_key())
        .timeout(Duration::from_secs(30))
        .version_comparator(|_, _| true)
        .build()
        .expect("the updater must build")
        .check()
        .await
        .expect("checking the feed must succeed")
        .expect("the feed announces a release, so an update must be offered")
}

#[tokio::test]
#[ignore = "needs a release build: run `npm run release` first"]
async fn the_updater_finds_the_published_release_and_accepts_its_signature() {
    let installer = installer();
    let expected = installer.clone();

    let port = start_feed(|port| {
        vec![
            (
                "/latest.json",
                manifest(port, None).into_bytes(),
                "application/json",
            ),
            ("/artifact", installer, "application/octet-stream"),
        ]
    });

    let app = app();
    let update = check(&app, port).await;
    assert_eq!(
        update.version,
        env!("CARGO_PKG_VERSION"),
        "the updater offered a version the manifest does not announce"
    );

    let downloaded = update
        .download(|_, _| {}, || {})
        .await
        .expect("the signed package must be accepted");
    assert_eq!(
        downloaded, expected,
        "the updater returned different bytes than the release published"
    );
    println!(
        "checked {}, downloaded and verified {} bytes",
        update.version,
        downloaded.len()
    );
}

#[tokio::test]
#[ignore = "needs a release build: run `npm run release` first"]
async fn a_tampered_package_is_refused_after_download() {
    let mut tampered = installer();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xFF;

    let port = start_feed(|port| {
        vec![
            (
                "/latest.json",
                manifest(port, None).into_bytes(),
                "application/json",
            ),
            ("/artifact", tampered, "application/octet-stream"),
        ]
    });

    let app = app();
    let update = check(&app, port).await;

    let outcome = update.download(|_, _| {}, || {}).await;
    assert!(
        outcome.is_err(),
        "the updater accepted an installer whose bytes do not match its signature"
    );
    println!("tampered package refused: {}", outcome.unwrap_err());
}

#[tokio::test]
#[ignore = "needs a release build: run `npm run release` first"]
async fn a_package_signed_for_another_version_is_refused() {
    // The endpoint's response is not signed, so its `version` field proves
    // nothing on its own. `requireSignedVersion` is what binds the package to
    // the version it was signed for; this checks that binding by announcing a
    // version the signature does not name.
    let installer = installer();

    let port = start_feed(|port| {
        vec![
            (
                "/latest.json",
                manifest(port, Some("1.0.1")).into_bytes(),
                "application/json",
            ),
            ("/artifact", installer, "application/octet-stream"),
        ]
    });

    let app = app();
    let update = check(&app, port).await;
    assert_eq!(update.version, "1.0.1");

    let outcome = update.download(|_, _| {}, || {}).await;
    match outcome {
        Err(tauri_plugin_updater::Error::SignedVersionMismatch { signed, announced }) => {
            println!("version binding held: signed for {signed}, announced {announced}");
        }
        other => panic!("a package signed for one version was accepted under another: {other:?}"),
    }
}
