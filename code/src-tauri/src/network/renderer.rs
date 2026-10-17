//! Browser-backed page rendering.
//!
//! Some public pages assemble their content with JavaScript and only then ask
//! their own server for it, using request parameters a plain HTTP client cannot
//! produce. For those pages the only way to see what the page itself is given is
//! to let a real browser run it.
//!
//! This module is the seam for that. It is deliberately *not* a browser
//! automation framework: it renders one public URL, waits for a condition the
//! caller names, runs one collection script, and returns normalized media
//! sources. Everything about processes, ports, and the DevTools protocol stays
//! behind it.
//!
//! # What a renderer is not
//!
//! * **Not a downloader.** It never transfers media bytes. It resolves a URL;
//!   the HTTP pipeline in [`super`] fetches it.
//! * **Not a session.** It uses a fresh throwaway profile and never reads or
//!   reuses the user's own browser profile or cookies.
//! * **Not a bypass.** It renders the same public page a user would open. It
//!   does not sign in, solve challenges, or forge signatures.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::errors::{AppError, AppResult};

/// Default budget for the page to become usable after navigation.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(25);
/// Default budget for the page to enrich itself once it is usable.
///
/// Many players publish their source list before the media element learns its
/// own geometry, so a short grace period after "ready" recovers facts that would
/// otherwise be reported as absent.
pub const DEFAULT_SETTLE_TIMEOUT: Duration = Duration::from_secs(8);
/// Default budget for the whole render, including browser start-up and teardown.
pub const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(75);

/// What kind of media a page exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaSourceKind {
    Video,
    Audio,
    /// A picture the rendered page displayed. A note/photo post's slides are
    /// images and nothing else says so, so the page itself is the evidence.
    Image,
    Unknown,
}

/// One media resource a rendered page obtained.
///
/// Fields the page did not report stay `None`; nothing here is ever inferred.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaSource {
    /// Absolute `http(s)` URL. Validated before it leaves this module.
    pub url: String,
    pub kind: MediaSourceKind,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    #[serde(default)]
    pub bitrate_kbps: Option<u32>,
    /// Labels the page itself attached — a quality gear name, a track name.
    /// Carried through for display; never used to invent a variant.
    #[serde(default)]
    pub labels: Vec<String>,
}

/// A response the page itself fetched that carried media-shaped content.
///
/// Deliberately unclassified: this is what the *network* saw, and deciding which
/// track it is belongs to the platform's resolver. Keeping it a small value type
/// also means no DevTools protocol structure escapes this module.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkMedia {
    pub url: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    /// The DevTools resource type (`Media`, `Fetch`, `XHR`, …), when reported.
    #[serde(default)]
    pub resource_type: Option<String>,
    /// HTTP status, when reported.
    #[serde(default)]
    pub status: Option<u16>,
}

/// The result of rendering one page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedPage {
    /// Where the browser actually ended up, after redirects.
    pub final_url: String,
    pub title: Option<String>,
    /// Media the page's own scripts exposed (a player's source list).
    pub media: Vec<MediaSource>,
    /// Media the page fetched over the network while rendering.
    ///
    /// A player that streams separate tracks often names only some of them in its
    /// configuration and pulls the rest itself, so this is where the remainder
    /// shows up.
    pub network_media: Vec<NetworkMedia>,
    /// Whether the network was actually watched.
    ///
    /// This is what separates "the page asked for no audio" from "nobody looked":
    /// without it, an empty `network_media` would be read as proof of absence.
    pub network_captured: bool,
    /// The raw JSON the page's own collection script returned, when the caller
    /// supplied a script whose result is not (only) media.
    ///
    /// `media` is the normalized view of this same payload; the raw value is
    /// kept because a platform may ask a page for more than its media — a
    /// profile's identity and its post grid, for instance — and interpreting
    /// that is platform knowledge, which belongs in the resolver rather than
    /// here. `None` when the render reported nothing, so an absent payload is
    /// never mistaken for an empty one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

/// Which browser profile a render should run in.
///
/// There are exactly two, and they are not interchangeable: one is a throwaway
/// directory deleted when the render's browser goes away, and the other is the
/// application's own profile, which is where a Douyin sign-in lives and which
/// therefore outlives every browser that opens it.
///
/// Kept separate from the visibility because they answer different questions —
/// whether the page sees a window, and whose cookies it sees — and a render can
/// want either combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrowserProfile {
    /// A fresh throwaway profile. The default, and the only behaviour that
    /// needs no user decision: it carries no sign-in and leaves nothing behind.
    #[default]
    Ephemeral,
    /// The application's persistent profile, which may hold a Douyin sign-in.
    ///
    /// Used only when the user has asked for authenticated profile loading, and
    /// never for a render that does not want it: a session opened on this
    /// profile is not reused for an anonymous render, or the anonymous render
    /// would silently become an authenticated one.
    Persistent,
}

/// Whether the browser that renders a page has a window.
///
/// This is not a preference. Some public pages serve their content to an
/// ordinary browser window and refuse the same request from a headless one, so
/// a page that needs the former has to ask for it. Defaulting to headless keeps
/// every existing caller unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrowserVisibility {
    /// No window. The default.
    #[default]
    Headless,
    /// A real window, positioned off the visible desktop.
    ///
    /// The page sees an ordinary browser — because it *is* one — while the user
    /// is not given a window to look at. Chrome's `--start-minimized` is ignored
    /// on Windows, so an off-screen position is what actually keeps a windowed
    /// browser out of the way; the window is otherwise ordinary.
    Windowed,
    /// A real window the user is meant to look at and type into.
    ///
    /// Only the sign-in flow asks for this, and nothing renders in it: it is an
    /// ordinary browser on an ordinary public page, and what happens next is
    /// the user typing their own credentials, which this application never sees.
    Visible,
}

/// What to render, and how to tell when it is ready.
///
/// The caller supplies the JavaScript because *what to look for* is platform
/// knowledge and belongs with the platform's resolver. The renderer supplies the
/// browser, the protocol, the timeouts, and the cleanup.
#[derive(Debug, Clone)]
pub struct RenderRequest<'a> {
    /// Public URL to open.
    pub url: &'a str,
    /// JavaScript predicate, evaluated repeatedly until it is truthy. An
    /// expression that throws counts as "not ready yet".
    pub ready_when: &'a str,
    /// Optional second predicate, polled *after* `ready_when` succeeds.
    ///
    /// This is for facts that arrive late and that the render should not fail
    /// without — a player publishes its source list before the media element
    /// knows its own dimensions. The render waits at most `settle_timeout` for
    /// it and then collects whatever is there.
    pub settle_when: Option<&'a str>,
    /// JavaScript expression returning `{ "media": [ ... ] }`. Only the shape
    /// described by [`MediaSource`] is understood; anything else is discarded.
    pub collect: &'a str,
    /// Whether the browser gets a window.
    pub visibility: BrowserVisibility,
    /// Whose cookies the browser carries.
    pub profile: BrowserProfile,
    /// How long to keep waiting for `ready_when` after the page loads.
    pub ready_timeout: Duration,
    /// How long to keep waiting for `settle_when` once the page is ready.
    pub settle_timeout: Duration,
    /// Overall budget for the whole render. Exceeding it fails the render and
    /// tears the browser down.
    pub total_timeout: Duration,
}

impl<'a> RenderRequest<'a> {
    pub fn new(url: &'a str, ready_when: &'a str, collect: &'a str) -> Self {
        Self {
            url,
            ready_when,
            settle_when: None,
            collect,
            visibility: BrowserVisibility::Headless,
            profile: BrowserProfile::Ephemeral,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            settle_timeout: DEFAULT_SETTLE_TIMEOUT,
            total_timeout: DEFAULT_TOTAL_TIMEOUT,
        }
    }

    /// Ask the page for a late-arriving fact once it is already usable.
    pub fn settle_on(mut self, settle_when: &'a str) -> Self {
        self.settle_when = Some(settle_when);
        self
    }

    /// Render in an ordinary browser window rather than headlessly.
    ///
    /// Used by a page that only serves its content to a real window. The window
    /// is still a fresh throwaway profile on the same public URL; it is simply
    /// kept off the visible desktop.
    pub fn in_a_window(mut self) -> Self {
        self.visibility = BrowserVisibility::Windowed;
        self
    }

    /// Render in the application's persistent profile, which may hold a sign-in.
    ///
    /// Only ever set by a caller that has decided the user asked for an
    /// authenticated load. Nothing here establishes that a sign-in exists —
    /// which cookies the profile holds is the browser's business, and whether
    /// they still work is the source's.
    pub fn with_persistent_profile(mut self) -> Self {
        self.profile = BrowserProfile::Persistent;
        self
    }

    /// How long to wait after the page is ready for late-arriving facts.
    pub fn with_settle_timeout(mut self, timeout: Duration) -> Self {
        self.settle_timeout = timeout;
        self
    }

    /// How long to wait for the page to become ready after it loads.
    pub fn with_ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = timeout;
        self
    }

    /// Overall budget for the render.
    pub fn with_total_timeout(mut self, timeout: Duration) -> Self {
        self.total_timeout = timeout;
        self
    }
}

/// Renders a public page in a real browser and reports the media it obtained.
#[async_trait]
pub trait PageRenderer: Send + Sync {
    /// Short identifier used in diagnostics and logs.
    fn name(&self) -> &'static str;

    /// Whether this renderer can run at all — for example whether a supported
    /// browser is installed. Checked before every attempt so a missing browser
    /// is a skipped fallback rather than a failed resolution.
    fn is_available(&self) -> bool;

    /// Render `request.url` and return the media the page obtained.
    async fn render(&self, request: &RenderRequest<'_>) -> AppResult<RenderedPage>;

    /// Release anything this renderer is holding open, without awaiting.
    ///
    /// Called on the application's exit path, where there is no runtime left to
    /// await on. A renderer that owns nothing — the HTTP-only case — has
    /// nothing to do, which is why this has a default.
    fn shutdown_now(&self) {}
}

/// A [`PageRenderer`] that is never available.
///
/// Used when browser assistance is switched off, and as the default in tests, so
/// callers never have to branch on an `Option`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopRenderer;

#[async_trait]
impl PageRenderer for NoopRenderer {
    fn name(&self) -> &'static str {
        "none"
    }

    fn is_available(&self) -> bool {
        false
    }

    async fn render(&self, _request: &RenderRequest<'_>) -> AppResult<RenderedPage> {
        Err(AppError::browser_unavailable(
            "no page renderer is configured",
        ))
    }
}

/// Most media-shaped responses to keep from one render.
///
/// A page can fetch a lot; a cap means a noisy or hostile page cannot turn the
/// candidate list into unbounded memory.
pub const MAX_NETWORK_CANDIDATES: usize = 32;

/// Media-shaped responses found in captured DevTools `Network` events.
///
/// Pure, so it can be tested against recorded events without a browser — which
/// is the only way to test it deterministically, since a real page's traffic is
/// neither stable nor replayable.
///
/// Deliberately conservative. A response is kept only when its reported content
/// type is audio or video, or when it was fetched *as* media by a media element;
/// images, scripts, stylesheets, fonts, and analytics are dropped so unrelated
/// page traffic cannot reach the candidate list. Deciding which track a
/// candidate actually is stays with the platform's resolver — this only says
/// "the page fetched something that looked like media".
pub fn network_media_from_events(events: &[serde_json::Value]) -> Vec<NetworkMedia> {
    let mut found: Vec<NetworkMedia> = Vec::new();

    for event in events {
        if event.get("method").and_then(serde_json::Value::as_str)
            != Some("Network.responseReceived")
        {
            continue;
        }
        let Some(response) = event.get("params").and_then(|p| p.get("response")) else {
            continue;
        };
        let Some(url) = response.get("url").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !is_fetchable(url) {
            continue;
        }

        let status = response
            .get("status")
            .and_then(serde_json::Value::as_u64)
            .and_then(|code| u16::try_from(code).ok());
        // A failed response taught us nothing, and a ranged read is a success.
        if matches!(status, Some(code) if code >= 400) {
            continue;
        }

        let mime_type = response
            .get("mimeType")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let resource_type = event
            .get("params")
            .and_then(|p| p.get("type"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        if !looks_like_media(mime_type.as_deref(), resource_type.as_deref()) {
            continue;
        }
        // The same resource is often fetched more than once.
        if found.iter().any(|existing| existing.url == url) {
            continue;
        }

        found.push(NetworkMedia {
            url: url.to_string(),
            mime_type,
            resource_type,
            status,
        });
        if found.len() >= MAX_NETWORK_CANDIDATES {
            break;
        }
    }

    found
}

/// Whether a response is worth considering as a media track.
fn looks_like_media(mime_type: Option<&str>, resource_type: Option<&str>) -> bool {
    if let Some(mime) = mime_type {
        let mime = mime.trim().to_ascii_lowercase();
        if mime.starts_with("audio/") || mime.starts_with("video/") {
            return true;
        }
    }
    // A media element's own request, whatever the server called it.
    matches!(resource_type, Some("Media"))
}

/// Parse the JSON a page's collection script returned into media sources.
///
/// Pure and total: it accepts only what it understands and drops the rest, so a
/// page that changes shape yields fewer sources rather than a wrong one. URLs
/// that are not `http(s)` are rejected here — this is the last point at which a
/// page can influence what the downloader will later be asked to fetch.
pub fn parse_media_sources(value: &serde_json::Value) -> Vec<MediaSource> {
    let items = value
        .get("media")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();

    items.iter().filter_map(parse_media_source).collect()
}

fn parse_media_source(value: &serde_json::Value) -> Option<MediaSource> {
    let url = value.get("url").and_then(serde_json::Value::as_str)?;
    if !is_fetchable(url) {
        return None;
    }

    let kind = match value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("video") => MediaSourceKind::Video,
        Some("audio") => MediaSourceKind::Audio,
        Some("image") => MediaSourceKind::Image,
        _ => MediaSourceKind::Unknown,
    };

    Some(MediaSource {
        url: url.to_string(),
        kind,
        mime_type: string_field(value, "mimeType").or_else(|| string_field(value, "mime_type")),
        width: u32_field(value, "width"),
        height: u32_field(value, "height"),
        duration_seconds: f64_field(value, "durationSeconds")
            .or_else(|| f64_field(value, "duration_seconds")),
        bitrate_kbps: u32_field(value, "bitrateKbps").or_else(|| u32_field(value, "bitrate_kbps")),
        labels: value
            .get("labels")
            .and_then(serde_json::Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|entry| entry.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
}

fn u32_field(value: &serde_json::Value, key: &str) -> Option<u32> {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
        .filter(|number| *number > 0)
}

fn f64_field(value: &serde_json::Value, key: &str) -> Option<f64> {
    value
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .filter(|number| *number > 0.0)
}

/// Whether a URL is something the download pipeline could legitimately fetch.
pub fn is_fetchable(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://")) && !lower.contains(['\n', '\r'])
}

/// Deterministic test double for [`PageRenderer`].
///
/// Available in non-test builds too, so integration tests and any future
/// offline mode can drive the resolver's fallback without a browser. It records
/// how many times it was asked to render, which is how tests assert that the
/// browser is *not* used when the HTTP path already succeeded.
///
/// It hands back exactly the sources it was constructed with, without applying
/// the URL filter the real renderer applies — so a test can present a hostile
/// source and check that the layer above refuses it on its own.
pub struct MockPageRenderer {
    available: bool,
    title: Option<String>,
    final_url: Option<String>,
    media: Vec<MediaSource>,
    network_media: Vec<NetworkMedia>,
    network_captured: bool,
    /// The raw payload a render reports, when a test supplies one.
    payload: Option<serde_json::Value>,
    /// Payloads handed out one per render, so a test can model a page that
    /// yields a further batch each time it is looked at.
    payload_queue: std::sync::Mutex<Vec<serde_json::Value>>,
    /// Failures to hand out before succeeding, for retry-shaped tests.
    failures: std::sync::Mutex<Vec<AppError>>,
    calls: std::sync::atomic::AtomicUsize,
    /// The visibility of the most recent request, so a test can assert which
    /// callers ask for a window.
    last_visibility: std::sync::Mutex<Option<BrowserVisibility>>,
    /// The profile the most recent request asked for, so a test can assert
    /// which callers want the signed-in one.
    last_profile: std::sync::Mutex<Option<BrowserProfile>>,
    /// The readiness predicate of every request, in order. A caller that asks a
    /// *different* question on a retry is a contract worth asserting: it is how
    /// a page of words is reached without every media page being read differently.
    ready_conditions: std::sync::Mutex<Vec<String>>,
}

impl MockPageRenderer {
    /// A renderer that reports itself as unavailable, as if no browser were
    /// installed.
    pub fn unavailable() -> Self {
        Self {
            available: false,
            ..Self::returning(Vec::new())
        }
    }

    /// A renderer that succeeds with the given sources.
    ///
    /// The network is reported as captured but empty, which is the honest
    /// default: the mock looked and found nothing beyond what it was given.
    pub fn returning(media: Vec<MediaSource>) -> Self {
        Self {
            available: true,
            title: None,
            final_url: None,
            media,
            network_media: Vec::new(),
            network_captured: true,
            payload: None,
            payload_queue: std::sync::Mutex::new(Vec::new()),
            failures: std::sync::Mutex::new(Vec::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            last_visibility: std::sync::Mutex::new(None),
            last_profile: std::sync::Mutex::new(None),
            ready_conditions: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// A renderer that always fails with `err`.
    pub fn failing(err: AppError) -> Self {
        let renderer = Self::returning(Vec::new());
        renderer.failures.lock().expect("lock").push(err);
        renderer
    }

    /// Hand out one more failure before this renderer starts succeeding.
    ///
    /// The mirror of [`Self::failing`], which is this for exactly one attempt.
    /// Queued failures are handed out before anything else, so a renderer built
    /// as `returning(media).after_failing(err)` refuses the first look at the
    /// page and answers the second — which is the shape a transient failure has.
    pub fn after_failing(self, err: AppError) -> Self {
        self.failures.lock().expect("lock").push(err);
        self
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn with_final_url(mut self, url: impl Into<String>) -> Self {
        self.final_url = Some(url.into());
        self
    }

    /// Media the page was seen fetching over the network.
    pub fn with_network_media(mut self, media: Vec<NetworkMedia>) -> Self {
        self.network_media = media;
        self
    }

    /// Simulate a render where the network could not be watched at all.
    pub fn without_network_capture(mut self) -> Self {
        self.network_captured = false;
        self
    }

    /// The raw JSON the page's collection script returned.
    ///
    /// This is what a profile page is asked for: identity and post grid rather
    /// than media, so the mock must be able to hand back an arbitrary shape.
    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = Some(payload);
        self
    }

    /// Payloads handed out one per render, in order.
    ///
    /// A page that exposes a further batch each time it is scrolled to is
    /// modelled by queueing one payload per expected render. Once the queue is
    /// empty the default payload (if any) is reported.
    pub fn with_payload_sequence(self, payloads: Vec<serde_json::Value>) -> Self {
        *self.payload_queue.lock().expect("lock") = payloads;
        self
    }

    /// How many times `render` was called.
    /// The readiness predicate of each render, in the order they were asked.
    pub fn ready_conditions(&self) -> Vec<String> {
        self.ready_conditions.lock().expect("lock").clone()
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The visibility the most recent render asked for.
    pub fn last_visibility(&self) -> Option<BrowserVisibility> {
        *self.last_visibility.lock().expect("lock")
    }

    /// The profile the most recent render asked for.
    pub fn last_profile(&self) -> Option<BrowserProfile> {
        *self.last_profile.lock().expect("lock")
    }
}

#[async_trait]
impl PageRenderer for MockPageRenderer {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn is_available(&self) -> bool {
        self.available
    }

    async fn render(&self, request: &RenderRequest<'_>) -> AppResult<RenderedPage> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.ready_conditions
            .lock()
            .expect("lock")
            .push(request.ready_when.to_string());
        *self.last_visibility.lock().expect("lock") = Some(request.visibility);
        *self.last_profile.lock().expect("lock") = Some(request.profile);

        if let Some(err) = self.failures.lock().expect("lock").pop() {
            return Err(err);
        }

        let queued = {
            let mut queue = self.payload_queue.lock().expect("lock");
            if queue.is_empty() {
                None
            } else {
                Some(queue.remove(0))
            }
        };

        Ok(RenderedPage {
            final_url: self
                .final_url
                .clone()
                .unwrap_or_else(|| request.url.to_string()),
            title: self.title.clone(),
            media: self.media.clone(),
            network_media: self.network_media.clone(),
            network_captured: self.network_captured,
            payload: queued.or_else(|| self.payload.clone()),
        })
    }
}

impl NetworkMedia {
    /// Build a candidate with only the fields a test usually cares about.
    pub fn test(url: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            mime_type: Some(mime_type.into()),
            resource_type: Some("Fetch".to_string()),
            status: Some(206),
        }
    }
}

impl MediaSource {
    /// Build a source with only the fields a test usually cares about.
    pub fn test(url: impl Into<String>, kind: MediaSourceKind) -> Self {
        Self {
            url: url.into(),
            kind,
            mime_type: None,
            width: None,
            height: None,
            duration_seconds: None,
            bitrate_kbps: None,
            labels: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_source() {
        let value = serde_json::json!({
            "media": [{
                "url": "https://cdn.example/video.mp4",
                "kind": "video",
                "mimeType": "video/mp4",
                "width": 1280,
                "height": 720,
                "durationSeconds": 72.5,
                "bitrateKbps": 641,
                "labels": ["720p"]
            }]
        });

        let sources = parse_media_sources(&value);
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        assert_eq!(source.kind, MediaSourceKind::Video);
        assert_eq!(source.width, Some(1280));
        assert_eq!(source.height, Some(720));
        assert_eq!(source.duration_seconds, Some(72.5));
        assert_eq!(source.bitrate_kbps, Some(641));
        assert_eq!(source.labels, vec!["720p".to_string()]);
    }

    #[test]
    fn drops_sources_that_are_not_fetchable() {
        let value = serde_json::json!({
            "media": [
                { "url": "blob:https://www.douyin.com/abc", "kind": "video" },
                { "url": "data:video/mp4;base64,AAAA", "kind": "video" },
                { "url": "file:///C:/video.mp4", "kind": "video" },
                { "url": "javascript:alert(1)", "kind": "video" },
                { "url": "https://cdn.example/ok.mp4", "kind": "video" }
            ]
        });

        let sources = parse_media_sources(&value);
        assert_eq!(sources.len(), 1, "only the http(s) source survives");
        assert_eq!(sources[0].url, "https://cdn.example/ok.mp4");
    }

    #[test]
    fn an_unrecognized_shape_yields_nothing_rather_than_a_guess() {
        assert!(parse_media_sources(&serde_json::json!({})).is_empty());
        assert!(parse_media_sources(&serde_json::json!({ "media": "nope" })).is_empty());
        assert!(
            parse_media_sources(&serde_json::json!({ "media": [{ "kind": "video" }] })).is_empty()
        );
    }

    #[test]
    fn unknown_field_names_do_not_invent_values() {
        let value = serde_json::json!({
            "media": [{ "url": "https://cdn/x.mp4", "kind": "mystery", "width": 0, "durationSeconds": 0.0 }]
        });
        let sources = parse_media_sources(&value);
        assert_eq!(sources[0].kind, MediaSourceKind::Unknown);
        // A zero dimension or duration is an absent one, not a real value.
        assert_eq!(sources[0].width, None);
        assert_eq!(sources[0].duration_seconds, None);
    }

    #[test]
    fn accepts_both_camel_and_snake_case_field_names() {
        let value = serde_json::json!({
            "media": [{ "url": "https://cdn/a.mp3", "kind": "audio", "mime_type": "audio/mp4", "duration_seconds": 12.0 }]
        });
        let sources = parse_media_sources(&value);
        assert_eq!(sources[0].mime_type.as_deref(), Some("audio/mp4"));
        assert_eq!(sources[0].duration_seconds, Some(12.0));
    }

    #[test]
    fn fetchability_check_is_conservative() {
        assert!(is_fetchable("https://a/b"));
        assert!(is_fetchable("http://a/b"));
        assert!(!is_fetchable("HTTPS://a/b\r\nX: y"));
        assert!(!is_fetchable("blob:https://a/b"));
        assert!(!is_fetchable(""));
    }

    #[tokio::test]
    async fn the_noop_renderer_is_never_available() {
        let renderer = NoopRenderer;
        assert!(!renderer.is_available());
        let request = RenderRequest::new("https://example.test", "true", "({})");
        let err = renderer.render(&request).await.unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::BrowserUnavailable);
    }

    #[test]
    fn a_settle_condition_is_optional() {
        let plain = RenderRequest::new("https://example.test", "true", "({})");
        assert!(plain.settle_when.is_none());
        let enriched = plain.settle_on("window.ready");
        assert_eq!(enriched.settle_when, Some("window.ready"));
    }

    #[test]
    fn a_render_is_headless_unless_a_window_is_asked_for() {
        // The default has to stay headless: only a page that demonstrably needs
        // a window should put one on screen.
        let plain = RenderRequest::new("https://example.test", "true", "({})");
        assert_eq!(plain.visibility, BrowserVisibility::Headless);
        assert_eq!(plain.in_a_window().visibility, BrowserVisibility::Windowed);
    }

    #[test]
    fn the_settle_and_total_budgets_can_be_widened_per_request() {
        let wider = RenderRequest::new("https://example.test", "true", "({})")
            .with_settle_timeout(Duration::from_secs(30))
            .with_total_timeout(Duration::from_secs(110));
        assert_eq!(wider.settle_timeout, Duration::from_secs(30));
        assert_eq!(wider.total_timeout, Duration::from_secs(110));
    }

    #[test]
    fn parse_media_sources_is_reused_from_the_renderer_module() {
        // Guards against the renderer and the parser drifting apart.
        let value =
            serde_json::json!({ "media": [{ "url": "https://cdn/a.mp4", "kind": "video" }] });
        let sources: Vec<MediaSource> = parse_media_sources(&value);
        assert_eq!(sources.len(), 1);
    }
}
