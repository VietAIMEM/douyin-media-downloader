//! Douyin URL classification and resolution.
//!
//! # Scope
//!
//! * **Classification**: validate and classify Douyin URLs, distinguish
//!   profiles from posts from short links, canonicalize, and expand short links
//!   by following their redirect.
//! * **Post resolution**: fetch the public share page for a post and normalize
//!   the embedded payload into a [`Post`] with its [`MediaItem`]s.
//! * **Profile listing**: still Phase 03.
//!
//! # Honesty rules
//!
//! The resolver never fabricates a result. When the source declines to return a
//! post — which is what a blocked, region-restricted, or removed post looks like
//! over the wire — it returns a typed `UnavailableContent` error rather than an
//! empty-but-successful post. When the page carries data this build cannot read,
//! that is a `Resolution` error. The distinction is what lets the UI tell a user
//! *why* a link did not work.
//!
//! The resolver also never rewrites a media URL to remove a watermark. It
//! classifies a watermarked rendition as watermarked and a clean one as clean
//! (`extract::is_watermarked`), and leaves both URLs exactly as the source
//! returned them.

pub mod extract;
pub mod player;
pub mod profile;

use std::sync::Arc;

use async_trait::async_trait;
use url::Url;

use crate::errors::{AppError, AppErrorKind, AppResult};
use crate::logging::redact_url;
use crate::models::{ContentStatus, Creator, MediaItem, MediaType, Platform, Post};
use crate::network::{FetchRequest, HttpClient, NoopRenderer, PageRenderer, RenderRequest};

use super::source_url::{SourceUrl, SourceUrlKind, SourceUrlParser};
use super::{MediaResolver, PageRequest, PostPage, PostResolver, ProfileResolver};

/// Hosts recognized as Douyin.
///
/// Compared case-insensitively and by suffix, so `www.` / `m.` / `v.`
/// subdomains and regional hosts are all covered without listing every one.
const DOUYIN_DOMAIN_SUFFIXES: &[&str] = &["douyin.com", "iesdouyin.com"];

/// Maximum length accepted for an extracted identifier.
const MAX_ID_LEN: usize = 128;

/// How many times one listing render may press the profile page's own refresh
/// control.
///
/// The page invites exactly that in its error message, and it is the recovery
/// that works; but it is still a retry of a source that may be refusing, so it
/// is bounded. Two extra attempts matches the retry policy the project applies
/// everywhere else.
const LISTING_REFRESH_LIMIT: usize = 2;

/// How long a listing render waits for the grid, including those refreshes.
///
/// Longer than the default settle budget because a refresh round-trip has to
/// complete inside it: the refresh is pressed, the page re-requests, and the
/// cards have to render before this expires.
const LISTING_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How many times a listing page may be taken before an empty grid is believed.
///
/// Douyin's page regularly fails its first listing request and answers the
/// second, so one render is not enough evidence to call a listing withheld —
/// and two turned out not to be either. The symptom was precise: "Load profile"
/// failing and the same button working when pressed again. That second press
/// *was* the retry, made by hand because the code would not make it; three
/// looks is that press, taken automatically.
///
/// Each look costs a navigation in the browser that is already running, not a
/// new browser, and a source that is genuinely refusing is asked three times and
/// then taken at its word.
const LISTING_RENDER_ATTEMPTS: usize = 3;

/// How long to wait before taking the listing page again.
///
/// The source's own listing request has to fail and be retried by the page
/// itself between looks, so a short pause is the difference between asking
/// again after that has happened and asking again during it. It is a wait for a
/// page that is still working, not a backoff against a server that refused —
/// a refusal ends the walk rather than being retried.
const LISTING_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(700);

/// How long a post render waits for the player to publish its sources.
///
/// Longer than the default: in an ordinary window the player appeared roughly
/// fifteen seconds after navigation on a live post, and a budget that close to
/// the observed time would make resolution fail on a slower page.
const POST_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(40);

/// How long the post page is given to stop changing before it is read.
///
/// A video page settles as soon as its element knows its size. A note page has
/// no such moment: its viewer fills in pictures as they are needed, and reading
/// it early is what made the same carousel report three slides on one run and
/// twenty-five on the next. This is the budget for the gallery to stop growing —
/// bounded, because a viewer that never stops still has to end the render.
const POST_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// How many times a post page is rendered before its collection is believed.
///
/// Two, and only ever two. The second render answers one of two different
/// questions, depending on what the first one produced:
///
/// * the page loaded but drew **fewer slides than it says it holds** — the same
///   question is asked again, and the better answer is kept;
/// * the page **never became ready**, which is the failure measured on the
///   installed application — the second render asks whether it is a page of
///   words instead, which is the only way a caption-only post can be read at all
///   and costs a working page nothing, because a working page never gets here.
///
/// A sign-in wall, unsupported content, a page that carried no post and a
/// vanished post are all deterministic, are not retryable, and are not asked
/// again — that is the endless loop this must not become.
///
/// The bound on time is therefore explicit: each render carries its own
/// [`RENDER_TOTAL_TIMEOUT`], so the worst case for one resolution is twice that,
/// and nothing here can extend it further.
const POST_RENDER_ATTEMPTS: u32 = 2;

/// The whole budget for one render, including the renderer's own retries.
///
/// Shared by the post path and the listing path: both are one render of one
/// page, and the ceiling should not depend on which of them asked.
const RENDER_TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(110);

/// Share-page URLs tried, in order, when resolving a post.
///
/// Douyin has moved this page between hosts more than once, and the ordering
/// matters: `iesdouyin` is the host share links redirect to and is the one that
/// currently renders server-side data. Each is fetched at most once, so the
/// worst case is bounded at three requests.
const SHARE_PAGE_HOSTS: &[&str] = &[
    "https://www.iesdouyin.com/share/video",
    "https://m.douyin.com/share/video",
    "https://www.douyin.com/video",
];

/// True when `host` belongs to Douyin.
pub fn is_douyin_host(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    DOUYIN_DOMAIN_SUFFIXES
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

/// Classifies raw user input into a [`SourceUrl`].
pub struct DouyinUrlParser;

impl DouyinUrlParser {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for DouyinUrlParser {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SourceUrlParser for DouyinUrlParser {
    fn name(&self) -> &'static str {
        "douyin-url"
    }

    fn platform(&self) -> Platform {
        Platform::Douyin
    }

    fn parse(&self, raw: &str) -> AppResult<SourceUrl> {
        classify(raw)
    }

    /// Resolve a short link by following its redirect and classifying the
    /// result.
    ///
    /// The body is never read — only the final URL matters — so a short link
    /// cannot pull a page into memory.
    async fn expand(&self, raw: &str, http: &Arc<dyn HttpClient>) -> AppResult<SourceUrl> {
        let parsed = self.parse(raw)?;
        let SourceUrlKind::ShortLink { code } = &parsed.kind else {
            return Ok(parsed);
        };

        tracing::debug!(target: "resolver", platform = "douyin", %code, "expanding short link");
        let final_url = http.resolve_redirect(&parsed.canonical).await?;
        let mut expanded = self.parse(&final_url).map_err(|err| {
            AppError::resolution("short link did not lead to a supported Douyin page")
                .with_user_message("This link does not lead to a supported Douyin page.")
                .with_technical_detail(format!(
                    "resolved={}: {}",
                    redact_url(&final_url),
                    err.message()
                ))
        })?;

        if expanded.is_short_link() {
            return Err(AppError::resolution("short link redirect loop")
                .with_user_message("This Douyin link could not be resolved.")
                .with_technical_detail(format!("resolved={}", redact_url(&final_url))));
        }

        // `original` is what the user pasted and what the UI shows back to
        // them; `canonical` carries the resolved destination; `resolved_url`
        // keeps the landing URL exactly as the server wrote it, because the
        // parameters on it (share signature, render flag) are ones the server
        // issued and validates.
        expanded.original = parsed.original;
        expanded.resolved_url = Some(final_url);
        tracing::info!(
            target: "resolver",
            platform = "douyin",
            kind = expanded.kind.label(),
            id = expanded.id().unwrap_or("-"),
            "short link expanded"
        );
        Ok(expanded)
    }
}

/// Parse `raw` into a canonical Douyin [`SourceUrl`].
///
/// Error kinds are meaningful to the UI:
/// * empty or unparseable input → `Validation`
/// * a valid URL on another platform → `UnsupportedContent`
/// * a Douyin URL that is not a profile/post/short link → `Validation`
pub fn classify(raw: &str) -> AppResult<SourceUrl> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(
            AppError::validation("empty URL").with_user_message("Paste a Douyin link to continue.")
        );
    }

    let parsed = parse_lenient(trimmed)?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();

    if !is_douyin_host(&host) {
        return Err(AppError::unsupported(format!("unsupported host `{host}`"))
            .with_user_message("Only Douyin links are supported.")
            .with_technical_detail(format!("host={host}")));
    }

    let segments: Vec<String> = parsed
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();

    // Short links live on the `v.` subdomain and carry an opaque code.
    if host.starts_with("v.") {
        let code = segments.first().ok_or_else(|| {
            AppError::validation("short link without a code")
                .with_user_message("This Douyin link is incomplete.")
        })?;
        validate_id(code)?;
        return Ok(SourceUrl {
            platform: Platform::Douyin,
            original: trimmed.to_string(),
            resolved_url: None,
            canonical: format!("https://{host}/{code}/"),
            kind: SourceUrlKind::ShortLink { code: code.clone() },
        });
    }

    // `?modal_id=...` links open a post in the feed overlay.
    if let Some(id) = query_param(&parsed, "modal_id") {
        validate_id(&id)?;
        return Ok(SourceUrl {
            platform: Platform::Douyin,
            original: trimmed.to_string(),
            resolved_url: None,
            canonical: format!("https://www.douyin.com/video/{id}"),
            kind: SourceUrlKind::Post { id },
        });
    }

    let head = segments.first().map(String::as_str).unwrap_or_default();

    // `user/<id>` and `share/user/<id>` name the same profile; the id sits at a
    // different index in each, so both are resolved here rather than in the
    // match below (where `segments[1]` is the literal `user` for the share
    // form).
    if let Some(id) = profile_id(&segments).map(str::to_string) {
        validate_id(&id)?;
        return Ok(SourceUrl {
            platform: Platform::Douyin,
            original: trimmed.to_string(),
            resolved_url: None,
            canonical: format!("https://www.douyin.com/user/{id}"),
            kind: SourceUrlKind::Profile { id },
        });
    }

    match head {
        "video" | "note" if segments.len() >= 2 => {
            let id = segments[1].clone();
            validate_id(&id)?;
            Ok(SourceUrl {
                platform: Platform::Douyin,
                original: trimmed.to_string(),
                resolved_url: None,
                canonical: format!("https://www.douyin.com/{head}/{id}"),
                kind: SourceUrlKind::Post { id },
            })
        }
        "share" if segments.len() >= 3 && (segments[1] == "video" || segments[1] == "note") => {
            let id = segments[2].clone();
            validate_id(&id)?;
            Ok(SourceUrl {
                platform: Platform::Douyin,
                original: trimmed.to_string(),
                resolved_url: None,
                canonical: format!("https://www.douyin.com/{}/{id}", segments[1]),
                kind: SourceUrlKind::Post { id },
            })
        }
        "" => Err(AppError::validation("Douyin URL with no path")
            .with_user_message("This link does not point to a Douyin profile or post.")),
        other => Err(
            AppError::validation(format!("unrecognized Douyin path `{other}`"))
                .with_user_message("This link does not point to a Douyin profile or post.")
                .with_technical_detail(format!("path={other}")),
        ),
    }
}

/// The profile id from `user/<id>` or `share/user/<id>`.
fn profile_id(segments: &[String]) -> Option<&str> {
    match segments {
        [head, id, ..] if head == "user" => Some(id.as_str()),
        [head, sub, id, ..] if head == "share" && sub == "user" => Some(id.as_str()),
        _ => None,
    }
    .filter(|id| !id.is_empty())
}

/// Parse a URL, tolerating input without a scheme (`douyin.com/video/1`).
fn parse_lenient(raw: &str) -> AppResult<Url> {
    if let Ok(url) = Url::parse(raw) {
        return Ok(url);
    }
    // Only retry when there is clearly no scheme, so genuine typos still fail.
    if !raw.contains("://") {
        if let Ok(url) = Url::parse(&format!("https://{raw}")) {
            return Ok(url);
        }
    }
    Err(AppError::validation("could not parse URL")
        .with_user_message("That does not look like a valid link.")
        .with_technical_detail(raw.chars().take(120).collect::<String>()))
}

fn query_param(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.trim().is_empty())
}

/// Reject ids that are empty, absurdly long, or contain separators.
fn validate_id(id: &str) -> AppResult<()> {
    let valid = !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(AppError::validation("malformed identifier in URL")
            .with_user_message("This Douyin link appears to be malformed.")
            .with_technical_detail(format!("len={}", id.len())))
    }
}

/// Douyin implementation of the resolver interfaces.
///
/// Holds an [`HttpClient`] so short-link expansion and post resolution work
/// without the resolver constructing its own networking stack, and a
/// [`PageRenderer`] for the content Douyin will only hand to a browser.
pub struct DouyinResolver {
    parser: DouyinUrlParser,
    http: Arc<dyn HttpClient>,
    /// Used only when the HTTP path cannot produce the post. Defaults to
    /// [`NoopRenderer`], which is never available, so a resolver built without
    /// one simply has no fallback.
    renderer: Arc<dyn PageRenderer>,
}

impl DouyinResolver {
    pub fn new(http: Arc<dyn HttpClient>) -> Self {
        Self::with_renderer(http, Arc::new(NoopRenderer))
    }

    /// Build a resolver that may fall back to rendering the page in a browser.
    pub fn with_renderer(http: Arc<dyn HttpClient>, renderer: Arc<dyn PageRenderer>) -> Self {
        Self {
            parser: DouyinUrlParser::new(),
            http,
            renderer,
        }
    }

    /// Classify a URL without touching the network.
    pub fn parse(&self, raw: &str) -> AppResult<SourceUrl> {
        self.parser.parse(raw)
    }

    /// Resolve a short link to a concrete profile or post [`SourceUrl`].
    ///
    /// Non-short links are returned unchanged. The redirect logic itself lives
    /// on [`DouyinUrlParser`], so the registry and this resolver share one
    /// implementation.
    pub async fn expand_short_link(&self, raw: &str) -> AppResult<SourceUrl> {
        self.parser.expand(raw, &self.http).await
    }

    /// Fetch and parse the aweme for `post_id`.
    ///
    /// `preferred` is the URL the platform itself handed us — the share
    /// redirect's landing URL, query parameters and all. It is tried first,
    /// because a share link's parameters are generated and validated by Douyin
    /// and a URL reconstructed by hand is not the request the platform invited.
    /// The known share hosts are then tried as a fallback, so a pasted concrete
    /// post URL (which never redirects) still works.
    ///
    /// When a page loads but carries no post, the failure is annotated with what
    /// the page actually contained — which state blob was present, whether any
    /// post-bearing key appeared, and its size. That is what makes "the source
    /// returned no post" an evidenced statement rather than a guess, and it is
    /// the first thing to look at if Douyin changes its page shape again.
    async fn fetch_aweme(
        &self,
        post_id: &str,
        preferred: Option<&str>,
    ) -> AppResult<serde_json::Value> {
        tracing::debug!(target: "resolver", platform = "douyin", post_id, "fetching post page");

        let mut candidates: Vec<String> = Vec::new();
        if let Some(url) = preferred.map(str::trim).filter(|url| !url.is_empty()) {
            candidates.push(url.to_string());
        }
        for host in SHARE_PAGE_HOSTS {
            let url = format!("{host}/{post_id}/");
            if !candidates.contains(&url) {
                candidates.push(url);
            }
        }

        let mut best: Option<AppError> = None;
        for url in &candidates {
            let request = FetchRequest::mobile_share(url);

            let attempt: AppResult<serde_json::Value> = match self.http.fetch_text(&request).await {
                Ok(html) => match extract::find_aweme_in_page(&html) {
                    Ok(aweme) => Ok(aweme),
                    Err(err) => {
                        let diagnostic = extract::describe_payload(&html);
                        tracing::warn!(
                            target: "resolver",
                            platform = "douyin",
                            post_id,
                            url = %redact_url(url),
                            diagnostic = %diagnostic,
                            "page loaded but carried no post data"
                        );
                        let prior = err.technical_detail().unwrap_or("no detail").to_string();
                        Err(err.with_technical_detail(format!(
                            "{prior} | url={} {diagnostic}",
                            redact_url(url)
                        )))
                    }
                },
                Err(err) => Err(err),
            };

            match attempt {
                Ok(aweme) => {
                    tracing::info!(
                        target: "resolver",
                        platform = "douyin",
                        post_id,
                        url = %redact_url(url),
                        "post payload retrieved"
                    );
                    return Ok(aweme);
                }
                Err(err) => {
                    // Keep the most specific failure; see `informative_rank`.
                    let more_specific = match &best {
                        None => true,
                        Some(current) => informative_rank(&err) > informative_rank(current),
                    };
                    if more_specific {
                        best = Some(err);
                    }
                }
            }
        }

        Err(best.unwrap_or_else(|| {
            AppError::unavailable("no share page host returned post data")
                .with_user_message("The source did not return this post.")
        }))
    }

    /// Resolve a post by rendering its page in a browser and reading the player.
    ///
    /// Reached only after the HTTP path has established that the served pages do
    /// not carry the post. The browser resolves; it never transfers media.
    async fn resolve_in_browser(
        &self,
        source: &SourceUrl,
        id: &str,
        http_error: AppError,
    ) -> AppResult<Post> {
        if !self.renderer.is_available() {
            // No browser to try: report what the HTTP path actually found rather
            // than replacing it with a less useful error about a browser the
            // user never asked for.
            tracing::info!(
                target: "resolver",
                platform = "douyin",
                post_id = id,
                "no page renderer available; reporting the HTTP result"
            );
            return Err(http_error);
        }

        let target = source
            .resolved_url
            .as_deref()
            .unwrap_or(source.canonical.as_str());
        tracing::info!(
            target: "resolver",
            platform = "douyin",
            post_id = id,
            renderer = self.renderer.name(),
            "falling back to browser rendering"
        );

        // Bound rather than inlined: the request borrows both conditions.
        let ready = player::post_ready_condition();
        let text_ready = player::text_post_ready_condition();
        let settle = player::post_settle_condition();

        // One render of this post, under whichever question is being asked of it.
        //
        // A function rather than a value because the question changes between
        // attempts: the first render asks whether this is a media page, and the
        // second — reached only when the first said no by never becoming ready —
        // asks whether it is a page of words instead.
        fn post_request<'a>(target: &'a str, ready: &'a str, settle: &'a str) -> RenderRequest<'a> {
            RenderRequest::new(target, ready, player::COLLECT_PLAYER_MEDIA)
                .settle_on(settle)
                // A note viewer fills in its pictures over time, so the gallery
                // needs longer to stop growing than a video element needs to
                // report its size.
                .with_settle_timeout(POST_SETTLE_TIMEOUT)
                // A signed-out Douyin video page has the same requirement as its
                // profile page: an ordinary window gets the player, a headless
                // browser is redirected to the feed with no player at all. Verified
                // against the live site. The player also takes noticeably longer to
                // appear in a window, so both budgets are widened to suit it.
                .in_a_window()
                .with_ready_timeout(POST_READY_TIMEOUT)
                .with_total_timeout(RENDER_TOTAL_TIMEOUT)
        }
        // Render, and render again if the gallery came back short.
        //
        // Measured on the installed application: of four resolutions of the same
        // nine-slide post, two returned a partially drawn gallery — one slide,
        // then two — while the page's own counter still said nine. The page had
        // been served; it simply had not finished drawing. Looking once more on
        // the same browser is what turns that into the album the page says is
        // there, and it costs a second render only in the case that was already
        // wrong.
        //
        // The best result is kept rather than the last, so a second look that
        // somehow finds less cannot make the answer worse.
        let mut best: Option<crate::network::RenderedPage> = None;
        let mut attempt = 0;
        // Whether the next attempt should ask the page whether it is words
        // rather than media. Set in exactly one place — the branch that handles a
        // render which never became ready — so a second look at a *short gallery*
        // goes on asking the media question it has already got an answer to.
        let mut ask_for_words = false;
        let rendered = loop {
            attempt += 1;
            // The second attempt can ask a different question. A page that has
            // already failed to become a media page is worth asking whether it is
            // a page of words — and asking that *only* here is what keeps the
            // answer away from every page that works: a post with a player or a
            // gallery never reaches a second attempt, because the first one
            // succeeded.
            let request = if ask_for_words {
                post_request(target, text_ready.as_str(), settle.as_str())
            } else {
                post_request(target, ready.as_str(), settle.as_str())
            };
            let page = match self.renderer.render(&request).await {
                Ok(page) => page,
                // A second look that fails does not throw away the first. The
                // first render did produce a page, and a resolution that found
                // something is worth more than an error about the retry.
                Err(err) if best.is_some() => {
                    tracing::warn!(
                        target: "resolver",
                        platform = "douyin",
                        post_id = id,
                        attempt,
                        error = %err.message(),
                        "a second look failed; keeping what the first one found"
                    );
                    break best.expect("checked just above");
                }
                // A *transient* failure is worth the render being asked for
                // again, and the difference matters: the renderer's own retry
                // looks again at the same page on the same browser, four hundred
                // milliseconds later, and a fresh browser gets no replacement at
                // all. Asking again here reaches the path that can, because the
                // browser is now one this resolution has already used.
                //
                // Gated on `is_retryable`, which is exactly "transient": a
                // sign-in wall, unsupported content, a page that carried no post
                // and a vanished post are all deterministic, are not retryable,
                // and fall straight through to the error below. Retrying those
                // is the endless loop this must not become.
                Err(err) if err.is_retryable() && attempt < POST_RENDER_ATTEMPTS => {
                    tracing::warn!(
                        target: "resolver",
                        platform = "douyin",
                        post_id = id,
                        attempt,
                        error = %err.message(),
                        "the page did not settle; asking once more, as a page of words"
                    );
                    ask_for_words = true;
                    continue;
                }
                // Keep the HTTP reason visible: if the browser also fails, the
                // reader of the log needs both halves of the story.
                Err(err) => {
                    let detail = format!(
                        "{} | after HTTP: {}",
                        err.technical_detail().unwrap_or("browser render failed"),
                        http_error.message()
                    );
                    return Err(err.with_technical_detail(detail));
                }
            };

            let (found, declared) = gallery_counts(&page);
            let better = best
                .as_ref()
                .map(|held| gallery_counts(held).0 < found)
                .unwrap_or(true);
            if better {
                best = Some(page);
            }

            let short = match declared {
                Some(total) => found < total,
                None => false,
            };
            if !short || attempt >= POST_RENDER_ATTEMPTS {
                break best.expect("a page was just stored");
            }
            tracing::warn!(
                target: "resolver",
                platform = "douyin",
                post_id = id,
                slides = found,
                declared_total = declared.unwrap_or(0),
                attempt,
                "the gallery came back short of the page's own count; looking again"
            );
        };

        // The player's list plus whatever else the page fetched for itself. The
        // audio is usually in the second group: a player that streams separate
        // renditions names some of them and pulls the rest on its own.
        let merged = player::merge_sources(rendered.media.clone(), &rendered.network_media);

        // What the page had to offer, and what was looked at. A resolution that
        // finds nothing has to be able to say *why* — no pictures on the page at
        // all is a different fact from pictures present and all of them filtered
        // out — and this is the line that tells them apart.
        if let Some(diagnostics) = rendered
            .payload
            .as_ref()
            .and_then(|payload| payload.get("diagnostics"))
        {
            let number = |key: &str| {
                diagnostics
                    .get(key)
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
            };
            tracing::info!(
                target: "resolver",
                platform = "douyin",
                post_id = id,
                images_found = number("imagesFound"),
                images_before_track = number("imagesBeforeTrack"),
                track_included = number("trackIncluded"),
                img_total = number("imgTotal"),
                img_skipped_small = number("imgSkippedSmall"),
                img_skipped_avatar = number("imgSkippedAvatar"),
                gallery = diagnostics
                    .get("gallery")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("-"),
                gallery_count = number("galleryCount"),
                slide_track = diagnostics
                    .get("slideTrack")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("-"),
                slide_track_count = number("slideTrackCount"),
                declared_total = diagnostics
                    .get("declaredTotal")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                containers = diagnostics
                    .get("containers")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("-"),
                image_hosts = diagnostics
                    .get("imageHosts")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("-"),
                og_image_host = diagnostics
                    .get("ogImageHost")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("-"),
                caption_chars = number("captionChars"),
                video_element = diagnostics
                    .get("videoElement")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                sources = merged.len(),
                "page media collected"
            );
        }

        // A page with no media at all is a failure *unless* it carried words.
        // A caption-only post reaches this point with nothing to download and
        // something to keep, and turning that into an error is what made a text
        // post indistinguishable from a broken video. The decision is made once,
        // below, where the post exists and its caption is known; this guard only
        // has to stop beforehand when there is plainly nothing here at all.
        if merged.is_empty() && caption_from_title(rendered.title.as_deref()).is_none() {
            return Err(
                AppError::unavailable("the page exposed no downloadable media")
                    .with_user_message(
                        "The page loaded in a browser, but it did not expose any media this app \
                     can download.",
                    )
                    .with_technical_detail(format!(
                        "final_url={}",
                        redact_url(&rendered.final_url)
                    )),
            );
        }

        let mut post = Post::new(
            Platform::Douyin,
            id.to_string(),
            MediaType::Video,
            source.original.clone(),
        );
        post.canonical_url = Some(source.canonical.clone());
        post.caption = caption_from_title(rendered.title.as_deref());
        post.status = ContentStatus::Ready;
        post.media_items = player::sources_to_media(&post, &merged);
        // The layout is stated from evidence rather than from the absence of it.
        // A page that requested no track at all has not been shown to lack an
        // audio track — it never fetched its media — so that case is reported as
        // unknown rather than as video-only.
        let observed_tracks = player::sources_from_network(&rendered.network_media).len();
        let evidence =
            player::TrackEvidence::from_render(rendered.network_captured, observed_tracks);
        player::apply_track_layout(&mut post.media_items, evidence);
        tracing::info!(
            target: "resolver",
            platform = "douyin",
            post_id = %id,
            track_evidence = evidence.describe(),
            observed_tracks,
            network_captured = rendered.network_captured,
            "track layout decided"
        );
        post.media_type = dominant_media_type(&post.media_items);

        // What was collected, against what the page said it held.
        //
        // This comparison is the whole point of reading the page's own counter:
        // a gallery can only be said to have been collected in full if something
        // other than the collection says how much there was. The counter is that
        // something. It is never used to *pad* a collection — a slide invented to
        // reach a total would be a fabricated download — only to say plainly
        // whether the collection matched, and to admit it when it did not.
        if let Some(declared) = rendered
            .payload
            .as_ref()
            .and_then(|payload| payload.get("diagnostics"))
            .and_then(|diagnostics| diagnostics.get("declaredTotal"))
            .and_then(serde_json::Value::as_u64)
        {
            let slides = post
                .media_items
                .iter()
                .filter(|item| item.media_type == MediaType::Image)
                .count() as u64;
            if slides == declared {
                tracing::info!(
                    target: "resolver",
                    platform = "douyin",
                    post_id = %id,
                    slides,
                    declared_total = declared,
                    "carousel collection matched the page's own count"
                );
            } else {
                tracing::warn!(
                    target: "resolver",
                    platform = "douyin",
                    post_id = %id,
                    slides,
                    declared_total = declared,
                    "carousel collection did not match the page's own count; \
                     reporting what was collected rather than claiming the album"
                );
            }
        }

        if post.media_items.is_empty() {
            // A post whose content is its words.
            //
            // Douyin publishes caption-only posts, and a page that renders one
            // has no media to hand over and is not broken — it is a text post.
            // Reporting it as "no downloadable media" is true and useless: the
            // user sees a post they cannot download, with nothing saying why.
            //
            // Only a page that offered *no media source at all* qualifies. A
            // page that did offer sources and produced no item from them is a
            // failure of this application's reading, not a text post, and
            // dressing that up as one would hide a real defect behind a
            // plausible-looking answer.
            //
            // What is preserved is what the source actually stated — the text it
            // served as the post's title, its canonical address, the creator, and
            // the publish time it printed. Nothing is inferred to fill a gap: a
            // page that stated no text is still reported as having no media,
            // because a text post invented out of a title-less page would be
            // this application's words rather than the creator's.
            let text = if merged.is_empty() {
                post.caption
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
            } else {
                None
            };
            if let Some(text) = text {
                post.media_type = MediaType::Text;
                post.metadata.insert("text".into(), serde_json::json!(text));

                // Kept exactly as the page printed it. The feed states a local
                // wall-clock time with no offset, and reading it as UTC would be
                // an eight-hour error dressed up as precision, so it is recorded
                // as the text the source stated rather than converted into an
                // instant the source never claimed.
                if let Some(published) = rendered
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("diagnostics"))
                    .and_then(|diagnostics| diagnostics.get("publishedAt"))
                    .and_then(serde_json::Value::as_str)
                {
                    post.metadata
                        .insert("publishedAt".into(), serde_json::json!(published));
                }

                tracing::info!(
                    target: "resolver",
                    platform = "douyin",
                    post_id = %post.platform_post_id,
                    text_chars = text.chars().count(),
                    "post resolution found text and no media; reporting a text post"
                );
                return Ok(post);
            }

            return Err(
                AppError::unsupported("the page's media was not a downloadable kind")
                    .with_user_message("This post does not contain downloadable media.")
                    .with_technical_detail(format!(
                        "final_url={}",
                        redact_url(&rendered.final_url)
                    )),
            );
        }

        post.metadata.insert(
            "resolution".into(),
            serde_json::json!({
                "stage": "resolved",
                "resolver": "douyin-url",
                "via": "browser",
                "phase": "02"
            }),
        );

        tracing::info!(
            target: "resolver",
            platform = "douyin",
            post_id = %post.platform_post_id,
            media_items = post.media_items.len(),
            variants = post.media_items.iter().map(|m| m.variants.len()).sum::<usize>(),
            "post resolution completed via browser"
        );
        Ok(post)
    }
}

/// Whether a failure is the kind a browser render could change.
///
/// Only "the pages responded but held no post" qualifies. A transport failure
/// would fail the same way in a browser, so rendering would cost a browser
/// launch to learn nothing.
fn needs_a_browser(err: &AppError) -> bool {
    matches!(
        err.kind(),
        AppErrorKind::UnavailableContent | AppErrorKind::Resolution
    )
}

/// What a render said about its gallery: how many slides it collected, and how
/// many the page itself said the album holds.
///
/// Read from the collector's own diagnostics rather than recounted here, so the
/// decision to look again is made on the same number that is logged and the same
/// number that becomes the reported disagreement.
fn gallery_counts(page: &crate::network::RenderedPage) -> (u64, Option<u64>) {
    let diagnostics = page
        .payload
        .as_ref()
        .and_then(|payload| payload.get("diagnostics"));
    let number = |key: &str| {
        diagnostics
            .and_then(|value| value.get(key))
            .and_then(serde_json::Value::as_u64)
    };
    (number("imagesFound").unwrap_or(0), number("declaredTotal"))
}

/// The media type a post's resolved items imply.
///
/// Reports what was actually found, and the order the three are weighed in is
/// the whole of it.
///
/// Pictures come before sound on purpose. A photo post with music has both an
/// image item and an audio item, and it is a photo post — treating the music as
/// the more important of the two is how such a post came to be described as
/// audio. And the *number* of pictures is what separates a post with a picture
/// from a carousel: answering "image" for both is how a fifteen-slide carousel
/// came to be typed as a single picture.
fn dominant_media_type(items: &[MediaItem]) -> MediaType {
    if items.is_empty() {
        return MediaType::Video;    }
    if items.iter().any(|item| item.media_type == MediaType::Video) {
        return MediaType::Video;
    }
    match items
        .iter()
        .filter(|item| item.media_type == MediaType::Image)
        .count()
    {
        0 => MediaType::Audio,
        1 => MediaType::Image,
        _ => MediaType::Carousel,
    }
}

/// Turn a page title into a caption.
///
/// Douyin titles are "<caption> - 抖音". The suffix is presentation, not content,
/// so it is removed; nothing else is inferred from the title.
fn caption_from_title(title: Option<&str>) -> Option<String> {
    let title = title?.trim();
    if title.is_empty() {
        return None;
    }
    let stripped = title
        .strip_suffix("- 抖音")
        .unwrap_or(title)
        .trim_end_matches('-')
        .trim();
    (!stripped.is_empty()).then(|| stripped.to_string())
}

/// How much a failure tells us, higher being more specific.
///
/// A page that loaded but carried no post is a definite answer about the
/// content; a connection error only says the request did not complete. Reporting
/// the definite answer is what keeps the UI honest when one host is reachable
/// and another is not.
fn informative_rank(err: &AppError) -> u8 {
    match err.kind() {
        crate::errors::AppErrorKind::UnavailableContent => 3,
        crate::errors::AppErrorKind::UnsupportedContent => 3,
        crate::errors::AppErrorKind::Resolution => 2,
        crate::errors::AppErrorKind::Network => 1,
        _ => 0,
    }
}

#[async_trait]
impl ProfileResolver for DouyinResolver {
    fn name(&self) -> &'static str {
        "douyin-url"
    }

    fn platform(&self) -> Platform {
        Platform::Douyin
    }

    fn supports(&self, url: &str) -> bool {
        self.parser.supports(url)
    }

    /// Identify the creator behind a profile URL.
    ///
    /// The profile's *identity* is knowable from the URL alone, so a creator is
    /// always returned; the display metadata is not, and comes from rendering
    /// the profile page in a browser — the same method the post resolver uses,
    /// because the unsigned HTTP routes serve an anti-bot shell with no state
    /// in it.
    ///
    /// When the browser is unavailable or the page yields no identity, the
    /// creator is returned with its platform id as the display name and a
    /// `url_only` resolution stage, rather than with an invented name or not at
    /// all. The caller can therefore always say *who* the link names, and can
    /// tell whether the details were actually read.
    async fn resolve_profile(&self, url: &str) -> AppResult<Creator> {
        let parsed = self.expand_short_link(url).await?;
        let SourceUrlKind::Profile { id } = &parsed.kind else {
            return Err(AppError::validation("not a profile URL")
                .with_user_message("This is not a Douyin profile link.")
                .with_technical_detail(format!("kind={}", parsed.kind.label())));
        };

        // What the URL alone establishes: a stable platform id and the link it
        // was found at. The id is a real value, so it stands in as the display
        // name rather than a fabricated label.
        let mut creator = Creator::new(
            Platform::Douyin,
            id.clone(),
            id.clone(),
            parsed.canonical.clone(),
        );
        creator.metadata.insert(
            "resolution".into(),
            serde_json::json!({ "stage": "url_only", "phase": "03" }),
        );

        tracing::info!(target: "resolver", platform = "douyin", profile_id = %id, "classified profile URL");

        if !self.renderer.is_available() {
            tracing::info!(
                target: "resolver",
                platform = "douyin",
                profile_id = %id,
                "no page renderer available; reporting profile identity only"
            );
            return Ok(creator);
        }

        let target = parsed
            .resolved_url
            .as_deref()
            .unwrap_or(parsed.canonical.as_str());
        // Waiting for the post grid too, but only briefly: the header is the
        // part this call is for, and a page that never lists posts (which is
        // common for a signed-out visitor) must still report its identity.
        let grid_settle = profile::grid_at_least(1);
        let request = RenderRequest::new(target, profile::PROFILE_READY, profile::COLLECT_PROFILE)
            .settle_on(grid_settle.as_str());

        match self.renderer.render(&request).await {
            Ok(rendered) => {
                let payload = rendered.payload.as_ref().and_then(profile::decode_payload);
                match payload
                    .as_ref()
                    .and_then(|value| profile::creator_from_payload(value, id, &parsed.canonical))
                {
                    Some(resolved) => {
                        creator.display_name = resolved.display_name;
                        creator.handle = resolved.handle;
                        creator.avatar_url = resolved.avatar_url;
                        creator.description = resolved.description;
                        creator.verified = resolved.verified;
                        creator.post_count = resolved.post_count;
                        creator.metadata.insert(
                            "resolution".into(),
                            serde_json::json!({
                                "stage": "resolved",
                                "resolver": "douyin-url",
                                "via": "browser",
                                "phase": "03"
                            }),
                        );
                        tracing::info!(
                            target: "resolver",
                            platform = "douyin",
                            profile_id = %id,
                            post_count = creator.post_count.map(|c| c as i64).unwrap_or(-1),
                            "profile resolution completed via browser"
                        );
                    }
                    None => {
                        // The page rendered but named nobody. The identity still
                        // stands; only the details are missing, and the reason is
                        // recorded rather than papered over.
                        tracing::warn!(
                            target: "resolver",
                            platform = "douyin",
                            profile_id = %id,
                            "profile page rendered without an identity"
                        );
                        creator.metadata.insert(
                            "resolution".into(),
                            serde_json::json!({
                                "stage": "url_only",
                                "degraded": "page_rendered_without_identity",
                                "phase": "03"
                            }),
                        );
                    }
                }
            }
            Err(err) => {
                // A render failure does not cost us the identity, so it is
                // recorded and reported as an unresolved *detail* rather than
                // turned into a failed profile lookup.
                tracing::warn!(
                    target: "resolver",
                    platform = "douyin",
                    profile_id = %id,
                    error = %err.message(),
                    "profile details could not be read from the page"
                );
                creator.metadata.insert(
                    "resolution".into(),
                    serde_json::json!({
                        "stage": "url_only",
                        "degraded": err.kind().code(),
                        "phase": "03"
                    }),
                );
            }
        }

        Ok(creator)
    }

    /// Enumerate one page of the creator's public posts.
    ///
    /// The listing is read from the rendered profile page, which is the only
    /// method available: the platform's own listing endpoint rejects an
    /// anonymous request with a signature error, and no signature is forged
    /// here. `page.cursor` is an opaque offset the resolver issued on a previous
    /// page; how the page reaches that offset — scrolling, lazily — stays
    /// entirely inside this method.
    ///
    /// A page that lists nothing is only reported as an empty listing when the
    /// profile itself says it has no posts. Otherwise the platform withheld the
    /// listing, and that is an error rather than a creator with no work.
    async fn list_posts(&self, creator: &Creator, page: PageRequest) -> AppResult<PostPage> {
        if !self.renderer.is_available() {
            return Err(AppError::browser_unavailable(
                "listing a profile's posts needs a browser and none is available",
            )
            .with_user_message(
                "Browsing a profile's posts needs a browser. Install Chrome or Edge, or turn \
                 browser-assisted resolution on in Settings.",
            ));
        }

        let offset: usize = page
            .cursor
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(|_| {
                AppError::validation("invalid pagination cursor")
                    .with_user_message("The page cursor is invalid; restart the listing.")
            })?
            .unwrap_or(0);

        let limit = page.limit.max(1) as usize;
        let target = offset.saturating_add(limit);

        // Ask the page for one card more than this page needs. A grid that can
        // supply it has a next page; one that cannot never reaches the
        // threshold, so "there is more" is observed rather than assumed from a
        // stored post count that may be out of date.
        //
        // The same predicate presses the page's own refresh control when the
        // grid has not arrived, which is the recovery that works — at most
        // `LISTING_REFRESH_LIMIT` times, inside this one render.
        let settle = profile::listing_settle(target.saturating_add(1), LISTING_REFRESH_LIMIT);
        let mut request = RenderRequest::new(
            &creator.profile_url,
            profile::PROFILE_READY,
            profile::COLLECT_PROFILE,
        )
        .settle_on(settle.as_str())
        // A signed-out Douyin profile serves its post list to an ordinary
        // browser window and refuses the identical request headlessly. This is
        // the one render that needs a window; every other path stays headless.
        .in_a_window()
        .with_settle_timeout(LISTING_SETTLE_TIMEOUT)
        // Startup + readiness + the settle budget above, with room for the
        // renderer's own single retry inside it.
        .with_total_timeout(RENDER_TOTAL_TIMEOUT);

        if page.authenticated {
            // The user asked for this listing to be read as themselves. The
            // browser that does it is the application's own profile — the one
            // the sign-in went into — and never the throwaway one, which would
            // come back with the signed-out answer and look like an empty
            // profile rather than a missing sign-in.
            request = request.with_persistent_profile();
        }

        let rendered = self.renderer.render(&request).await?;
        let mut payload = rendered
            .payload
            .as_ref()
            .and_then(profile::decode_payload)
            .ok_or_else(|| {
                AppError::unavailable("the profile page returned nothing readable")
                    .with_user_message(
                        "The profile page did not return anything this app could read.",
                    )
            })?;

        let mut posts = profile::posts_from_payload(&payload, creator, offset);

        tracing::info!(
            target: "resolver",
            platform = "douyin",
            profile_id = %creator.platform_id,
            offset,
            collected = posts.len(),
            in_works_list = posts
                .iter()
                .filter(|post| profile::in_works_list(post) == Some(true))
                .count(),
            post_list_found = profile::post_list_found(&payload),
            declared_total = creator.post_count.map(|count| count as i64).unwrap_or(-1),
            "profile listing collected"
        );

        // More looks at the same page, in the same browser, while the grid
        // comes back empty.
        //
        // Douyin's own page frequently fails its *first* listing request and
        // succeeds on the next — that is the behaviour its `刷新` control exists
        // for, and the settle predicate above presses it. When even that was not
        // enough, taking the whole page again is what a person does next, and it
        // costs one navigation in a browser that is already running rather than
        // a browser launch.
        //
        // Bounded, so a source that is genuinely refusing is asked a few times
        // and then believed — and spaced, because asking again at the same
        // instant asks at the same unlucky moment. One attempt was not enough:
        // the second press of "Load profile" succeeding is what that looked
        // like from the outside, and the second press is a retry this can make
        // for itself.
        let mut attempt = 1;
        while attempt < LISTING_RENDER_ATTEMPTS && worth_another_look(&posts, target, creator) {
            attempt += 1;
            tokio::time::sleep(LISTING_RETRY_BACKOFF).await;
            tracing::info!(
                target: "resolver",
                platform = "douyin",
                profile_id = %creator.platform_id,
                offset,
                attempt,
                held = posts.len(),
                refreshes = profile::refresh_count(&payload),
                "the listing looked unfinished; looking again"
            );

            let retry = self.renderer.render(&request).await?;
            let Some(retried) = retry.payload.as_ref().and_then(profile::decode_payload) else {
                continue;
            };
            let second = profile::posts_from_payload(&retried, creator, offset);
            // The latest look is what the empty-grid analysis below reasons
            // about; the longest is what the listing is built from. A look that
            // found less is a worse answer to the same question, not a
            // correction of a better one, so it does not replace it.
            payload = retried;
            if second.len() > posts.len() {
                tracing::info!(
                    target: "resolver",
                    platform = "douyin",
                    profile_id = %creator.platform_id,
                    offset,
                    attempt,
                    held = second.len(),
                    "a later look found more of the listing"
                );
                posts = second;
            }
        }
        if !posts.is_empty() {
            for post in &mut posts {
                post.creator_handle = creator.handle.clone();
                post.creator_id = Some(creator.id.clone());
            }
            return Ok(page_from(posts, creator, offset, limit));
        }

        // The identity the page carries is applied to posts the page did not
        // caption, so a card without text still shows who it belongs to.
        for post in &mut posts {
            post.creator_handle = creator.handle.clone();
            post.creator_id = Some(creator.id.clone());
        }

        if posts.is_empty() {
            // An empty grid has to be explained, and the profile's own post
            // count is the only evidence available for doing so. It is used
            // rather than guessed around: a claim that the listing has ended is
            // made only where the source supports it.
            let declared_remaining = creator
                .post_count
                .is_some_and(|count| (offset as u32) < count);

            // The page's own service-error banner, when it is showing one. It
            // is the source saying *its* listing request came back unusable,
            // which settles the question the empty grid leaves open.
            let page_failed = profile::page_error(&payload);
            let refreshes = profile::refresh_count(&payload);

            // The source asking for an account, in its own words or with its
            // own sign-in dialog. Checked before anything else here, because
            // every other explanation below is about a listing that was not
            // shown for some *other* reason — and a sign-in wall would
            // otherwise be reported as "the source returned nothing", which is
            // both wrong and unactionable.
            //
            // True for an anonymous visitor too: the answer to "I would show
            // you this if you signed in" is the same whoever is asking, and the
            // screen offers the sign-in action for it either way.
            if profile::login_required(&payload) {
                tracing::info!(
                    target: "resolver",
                    platform = "douyin",
                    profile_id = %creator.platform_id,
                    offset,
                    authenticated = page.authenticated,
                    "the source asked for a signed-in visitor"
                );
                return Err(AppError::authentication_required(
                    "the source would not list this profile to a signed-out visitor",
                )
                .with_user_message(
                    "Douyin asked for a signed-in visitor before it would show this profile's \
                     posts. Use Login on this screen to sign in, enable \"Use login session \
                     when loading profiles\", and load the profile again.",
                )
                .with_technical_detail(format!(
                    "sec_uid={} offset={} authenticated={} declared={:?} page_error={} \
                     refreshes={} rendered={}",
                    creator.platform_id,
                    offset,
                    page.authenticated,
                    creator.post_count,
                    page_failed.as_deref().unwrap_or("-"),
                    refreshes,
                    rendered.final_url
                )));
            }

            // A profile that states its own count as zero, at the start of the
            // listing, really has nothing to show.
            if offset == 0 && creator.post_count == Some(0) {
                return Ok(PostPage {
                    posts,
                    next_cursor: None,
                    has_more: false,
                    total_known: creator.post_count,
                });
            }

            // Past the first page with the grid empty and the page reporting no
            // failure: either the listing ended, or a source that had been
            // answering stopped. The declared count separates them — if it
            // names posts further on than have been read, the listing did not
            // end, it was withheld.
            if page_failed.is_none() && offset > 0 && !declared_remaining {
                return Ok(PostPage {
                    posts,
                    next_cursor: None,
                    has_more: false,
                    total_known: creator.post_count,
                });
            }

            tracing::warn!(
                target: "resolver",
                platform = "douyin",
                profile_id = %creator.platform_id,
                offset,
                declared_post_count = creator.post_count.map(|c| c as i64).unwrap_or(-1),
                page_error = page_failed.as_deref().unwrap_or("-"),
                refreshes,
                "the profile page listed no posts"
            );
            // The message says what was observed rather than naming a cause the
            // evidence does not establish: the endpoint answers this app's
            // browser with a signature refusal or an empty body, and the site's
            // own page shows a service error for the request it made itself.
            //
            // Retryable, because an empty grid is also what a page that has not
            // finished building looks like. Both are the same observation from
            // here, and they call for opposite responses: one is the source
            // refusing and the other is a page that had not arrived. Treating it
            // as final is what made "Load profile" fail on the first press and
            // succeed on the second — the second press was the retry the code
            // declined to make for itself.
            return Err(AppError::unavailable(
                "the profile page listed no posts for an anonymous visitor",
            )
            .with_retryable(true)
            .with_user_message(
                "The source did not return this profile's posts. Douyin's own page reports a \
                 service error for its listing request and answers this app's browser with \
                 either a signature refusal or an empty response. This app does not sign in \
                 on your behalf, so there is nothing further it can do about that.",
            )
            .with_technical_detail(format!(
                "sec_uid={} offset={} declared={:?} page_error={} refreshes={} rendered={}",
                creator.platform_id,
                offset,
                creator.post_count,
                page_failed.as_deref().unwrap_or("-"),
                refreshes,
                rendered.final_url
            )));
        }

        // `posts` is what the page actually listed, after the filtering that
        // drops site chrome; the slice is the part this page asked for.
        Ok(page_from(posts, creator, offset, limit))
    }
}

/// Whether another look at the page could still tell us something.
///
/// Two shapes reach here, and they are the same shape from different distances.
///
/// A grid that came back *empty* has plainly not answered. That retry is old,
/// and it is what made a second press of "Load profile" work where the first
/// did not.
///
/// A grid that stopped at *exactly* what this page asked for — the settle
/// predicate demanded one card more than `target` and never saw it — while the
/// profile's own count says there are posts beyond `target`, has not ended
/// either. It has not finished arriving. This is the case a person works around
/// by pressing Refresh and loading again, and the log they are acting on is
/// `raw_posts` landing on the target and going no further while the count says
/// 50.
///
/// Without a stated count the two are indistinguishable from here, so nothing
/// is claimed: the page is taken at its word and the caller is told the source
/// stopped there. A count that has already been reached says the opposite —
/// there is nothing beyond this page to wait for — and is believed too.
fn worth_another_look(posts: &[Post], target: usize, creator: &Creator) -> bool {
    if posts.is_empty() {
        return true;
    }
    posts.len() <= target && creator.post_count.is_some_and(|count| (target as u32) < count)
}

/// The slice of a listing this page asked for, with its continuation cursor.
///
/// The whole rendered grid is read and this takes the window `offset..` from it:
/// whether another page exists is then a fact about what the page actually
/// showed, not an assumption from a stored count that may be out of date.
fn page_from(listed: Vec<Post>, creator: &Creator, offset: usize, limit: usize) -> PostPage {
    let total = listed.len();
    // How much of what was collected the page itself placed in the creator's
    // works list. This is the number that explains a collected total differing
    // from the profile's own count, and it is reported rather than acted on:
    // see the note in the collector.
    let in_works_list = listed
        .iter()
        .filter(|post| profile::in_works_list(post) == Some(true))
        .count();
    let slice: Vec<Post> = listed.into_iter().skip(offset).take(limit).collect();
    let consumed = offset + slice.len();
    let has_more = total > consumed;
    let next_cursor = has_more.then(|| consumed.to_string());

    // What a pagination problem needs to be diagnosed from: which page this
    // was, what the grid actually held, how much of it this page took, where
    // the next request would continue from, and — when the listing ends — the
    // reason it ended. The last one is the difference between "the source had
    // nothing more" and "the source stopped answering", which are two different
    // bugs and produce identical output otherwise.
    let page = offset.checked_div(limit).map_or(1, |page| page + 1);
    tracing::info!(
        target: "resolver",
        platform = "douyin",
        profile_id = %creator.platform_id,
        page,
        offset_before = offset,
        raw_posts = total,
        in_works_list = in_works_list,
        returned = slice.len(),
        has_more,
        cursor_after = next_cursor.as_deref().unwrap_or("-"),
        declared_total = creator.post_count.map(|count| count as i64).unwrap_or(-1),
        stop_reason = if has_more {
            "-"
        } else {
            "the rendered grid held no posts beyond this page"
        },
        "profile listing page read"
    );

    PostPage {
        posts: slice,
        next_cursor,
        has_more,
        total_known: creator.post_count,
    }
}

#[async_trait]
impl PostResolver for DouyinResolver {
    fn name(&self) -> &'static str {
        "douyin-url"
    }

    fn platform(&self) -> Platform {
        Platform::Douyin
    }

    fn supports(&self, url: &str) -> bool {
        self.parser.supports(url)
    }

    /// Resolve one post and attach its media.
    ///
    /// The share page carries the post and its media in a single response, so
    /// media is populated here. [`MediaResolver::resolve_media`] returns what is
    /// already attached rather than fetching the page a second time.
    async fn resolve_post(&self, url: &str) -> AppResult<Post> {
        tracing::info!(target: "resolver", platform = "douyin", "post resolution started");
        let source = self.expand_short_link(url).await?;

        let id = match &source.kind {
            SourceUrlKind::Post { id } => id.clone(),
            SourceUrlKind::Profile { .. } => {
                return Err(AppError::validation("not a post URL")
                    .with_user_message(
                        "This is a profile link. Open the Profile Downloader to use it.",
                    )
                    .with_technical_detail(format!("kind={}", source.kind.label())))
            }
            SourceUrlKind::ShortLink { .. } => {
                return Err(AppError::resolution("short link unresolved")
                    .with_user_message("This Douyin link could not be resolved."))
            }
        };

        // Ask for the URL we were actually given. For a short link that is the
        // redirect's landing URL, which carries parameters the platform issued
        // and checks; for a concrete link it is the link itself. Only if the
        // page there yields nothing do we fall back to hosts we construct.
        let preferred = source
            .resolved_url
            .clone()
            .unwrap_or_else(|| url.trim().to_string());

        let aweme = match self.fetch_aweme(&id, Some(&preferred)).await {
            Ok(aweme) => aweme,
            // The pages responded but none of them contained the post. That is
            // the one situation a browser can change: the page's own JavaScript
            // is what fetches the post, and it can only run in a browser. A
            // transport failure (the machine is offline) is left alone, because
            // rendering would fail for the same reason and cost a browser launch
            // to learn it.
            Err(err) if needs_a_browser(&err) => {
                return self.resolve_in_browser(&source, &id, err).await;
            }
            Err(err) => return Err(err),
        };

        let parsed = extract::aweme_to_domain(&aweme)?;

        let mut post = parsed.post;
        // Show the link the user actually pasted, and the canonical form the
        // resolver settled on.
        post.url = source.original.clone();
        post.canonical_url = Some(source.canonical.clone());
        post.status = ContentStatus::Ready;
        post.media_items = parsed.media;
        post.metadata.insert(
            "resolution".into(),
            serde_json::json!({
                "stage": "resolved",
                "resolver": "douyin-url",
                "via": "http",
                "phase": "02"
            }),
        );

        tracing::info!(
            target: "resolver",
            platform = "douyin",
            post_id = %post.platform_post_id,
            media_type = %post.media_type,
            media_items = post.media_items.len(),
            variants = post.media_items.iter().map(|m| m.variants.len()).sum::<usize>(),
            "post resolution completed"
        );
        Ok(post)
    }
}

#[async_trait]
impl MediaResolver for DouyinResolver {
    fn name(&self) -> &'static str {
        "douyin-url"
    }

    fn platform(&self) -> Platform {
        Platform::Douyin
    }

    /// Return the post's media.
    ///
    /// Media attached by [`PostResolver::resolve_post`] is returned as-is. A
    /// post that arrived without media (for example one reconstructed from the
    /// database) is re-resolved from the share page, so this method is correct
    /// on its own rather than only as a follow-on.
    async fn resolve_media(&self, post: &Post) -> AppResult<Vec<MediaItem>> {
        if !post.media_items.is_empty() {
            return Ok(post.media_items.clone());
        }

        let aweme = self.fetch_aweme(&post.platform_post_id, None).await?;
        let parsed = extract::aweme_to_domain(&aweme)?;

        tracing::info!(
            target: "resolver",
            platform = "douyin",
            post_id = %post.platform_post_id,
            media_items = parsed.media.len(),
            "media resolution completed"
        );
        Ok(parsed.media)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{MediaType, TrackLayout};
    use crate::network::renderer::{
        BrowserVisibility, MediaSource, MediaSourceKind, MockPageRenderer, NetworkMedia,
    };
    use crate::network::MockHttpClient;

    /// A renderer that returns one clean video track and one audio track.
    fn browser_with_tracks() -> MockPageRenderer {
        MockPageRenderer::returning(vec![
            MediaSource::test(
                "https://cdn.example/media-video-avc1/?token=x",
                MediaSourceKind::Video,
            ),
            MediaSource::test(
                "https://cdn.example/media-audio-und-mp4a/?token=x",
                MediaSourceKind::Audio,
            ),
        ])
        .with_title("A caption - 抖音")
        .with_final_url("https://www.douyin.com/video/7281234567890123456")
    }

    /// The page Douyin currently serves: a marker, no post in it.
    fn config_only_client() -> MockHttpClient {
        MockHttpClient::new().with_body(
            "https://www.iesdouyin.com/share/video/7281234567890123456/",
            crate::resolvers::douyin::extract::tests::sample_live_config_only_page(),
        )
    }

    // The two fixtures the project ships for architecture validation.
    const PROFILE_TEST_URL: &str = "https://v.douyin.com/SMt7NsUPaGo/";
    const VIDEO_TEST_URL: &str = "https://v.douyin.com/7yyhouNSzvA/";

    #[test]
    fn accepts_the_shipped_profile_fixture_as_a_short_link() {
        let parsed = classify(PROFILE_TEST_URL).unwrap();
        assert_eq!(parsed.platform, Platform::Douyin);
        assert!(parsed.is_short_link());
        assert_eq!(
            parsed.kind,
            SourceUrlKind::ShortLink {
                code: "SMt7NsUPaGo".into()
            }
        );
        assert_eq!(parsed.canonical, "https://v.douyin.com/SMt7NsUPaGo/");
    }

    #[test]
    fn accepts_the_shipped_video_fixture_as_a_short_link() {
        let parsed = classify(VIDEO_TEST_URL).unwrap();
        assert!(parsed.is_short_link());
        assert_eq!(
            parsed.kind,
            SourceUrlKind::ShortLink {
                code: "7yyhouNSzvA".into()
            }
        );
    }

    #[test]
    fn classifies_full_profile_url() {
        let parsed = classify("https://www.douyin.com/user/MS4wLjABAAAAabc-123").unwrap();
        assert!(parsed.is_profile());
        assert_eq!(parsed.id(), Some("MS4wLjABAAAAabc-123"));
        assert_eq!(
            parsed.canonical,
            "https://www.douyin.com/user/MS4wLjABAAAAabc-123"
        );
    }

    #[test]
    fn classifies_full_video_url() {
        let parsed = classify("https://www.douyin.com/video/7281234567890123456").unwrap();
        assert!(parsed.is_post());
        assert_eq!(parsed.id(), Some("7281234567890123456"));
    }

    #[test]
    fn classifies_note_url_as_post() {
        let parsed = classify("https://www.douyin.com/note/712345").unwrap();
        assert!(parsed.is_post());
        assert_eq!(parsed.canonical, "https://www.douyin.com/note/712345");
    }

    #[test]
    fn accepts_modal_id_query_links() {
        let parsed =
            classify("https://www.douyin.com/discover?modal_id=7281234567890123456").unwrap();
        assert!(parsed.is_post());
        assert_eq!(parsed.id(), Some("7281234567890123456"));
        assert_eq!(
            parsed.canonical,
            "https://www.douyin.com/video/7281234567890123456"
        );
    }

    #[test]
    fn accepts_legacy_share_paths() {
        // The id must survive: `share/user/<id>` and `user/<id>` differ by one
        // segment, and reading the wrong one yields the literal "user".
        let profile = classify("https://www.iesdouyin.com/share/user/98765").unwrap();
        assert!(profile.is_profile());
        assert_eq!(profile.id(), Some("98765"));
        assert_eq!(profile.canonical, "https://www.douyin.com/user/98765");

        let video = classify("https://www.iesdouyin.com/share/video/12345").unwrap();
        assert!(video.is_post());
        assert_eq!(video.id(), Some("12345"));
        assert_eq!(video.canonical, "https://www.douyin.com/video/12345");
    }

    #[test]
    fn tolerates_missing_scheme() {
        let parsed = classify("douyin.com/video/7281234567890123456").unwrap();
        assert!(parsed.is_post());
    }

    #[test]
    fn empty_input_is_a_validation_error() {
        let err = classify("   ").unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Validation);
    }

    #[test]
    fn non_url_input_is_a_validation_error() {
        let err = classify("not a url at all").unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Validation);
    }

    #[test]
    fn other_platforms_are_unsupported() {
        let err = classify("https://www.youtube.com/watch?v=abc").unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::UnsupportedContent);
        let err = classify("https://example.com/video/1").unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::UnsupportedContent);
    }

    #[test]
    fn douyin_but_non_content_path_is_a_validation_error() {
        let err = classify("https://www.douyin.com/about").unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Validation);
    }

    #[test]
    fn malformed_identifier_is_rejected() {
        assert!(classify("https://www.douyin.com/video/").is_err());
        assert!(classify("https://www.douyin.com/user/..").is_err());
    }

    #[test]
    fn lookalike_hosts_are_not_douyin() {
        // `notdouyin.com` must not match the `douyin.com` suffix rule.
        let err = classify("https://notdouyin.com/video/1").unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::UnsupportedContent);
        assert!(!is_douyin_host("evildouyin.com"));
        assert!(is_douyin_host("v.douyin.com"));
        assert!(is_douyin_host("DOUYIN.COM"));
    }

    #[test]
    fn parser_reports_its_platform() {
        assert_eq!(DouyinUrlParser::new().platform(), Platform::Douyin);
    }

    #[tokio::test]
    async fn resolve_post_expands_short_links_via_http() {
        let client = MockHttpClient::new().with_redirect(
            VIDEO_TEST_URL,
            "https://www.iesdouyin.com/share/video/7281234567890123456/",
        );
        let resolver = DouyinResolver::new(Arc::new(client));
        // The share page is not stubbed, so resolution stops at the fetch; the
        // expansion itself is what this asserts.
        let source = resolver.expand_short_link(VIDEO_TEST_URL).await.unwrap();
        assert_eq!(source.id(), Some("7281234567890123456"));
        assert_eq!(source.original, VIDEO_TEST_URL);
    }

    #[tokio::test]
    async fn profile_link_used_as_post_is_rejected_with_guidance() {
        let resolver = DouyinResolver::new(Arc::new(MockHttpClient::new()));
        let err = resolver
            .resolve_post("https://www.douyin.com/user/MS4wLjAB")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Validation);
        assert!(err.user_message().contains("Profile Downloader"));
    }

    #[tokio::test]
    async fn a_page_with_no_readable_state_is_reported_rather_than_faked() {
        // Every candidate host answers with the anti-bot shell: no state blob
        // at all. The resolver must report that the page was unreadable rather
        // than inventing a post.
        let client = MockHttpClient::new()
            .with_body(
                "https://www.iesdouyin.com/share/video/7281234567890123456/",
                "<html><body><script>var glb;/* shell */</script></body></html>",
            )
            .with_body(
                "https://m.douyin.com/share/video/7281234567890123456/",
                "<html></html>",
            )
            .with_body(
                "https://www.douyin.com/video/7281234567890123456/",
                "<html></html>",
            );

        let resolver = DouyinResolver::new(Arc::new(client));
        let err = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Resolution);
    }

    // --- the live page shape -------------------------------------------------

    #[tokio::test]
    async fn the_live_config_only_page_is_reported_as_unavailable_with_evidence() {
        // This is the shape Douyin actually serves today: the marker is there,
        // the page renders, and the post data is simply absent. Reporting
        // "unavailable" (not a fabricated post, not a generic failure) is the
        // correct behaviour, and the diagnostic must say what was missing.
        let client = MockHttpClient::new().with_body(
            "https://www.iesdouyin.com/share/video/7281234567890123456/",
            crate::resolvers::douyin::extract::tests::sample_live_config_only_page(),
        );

        let resolver = DouyinResolver::new(Arc::new(client));
        let err = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap_err();

        assert_eq!(err.kind(), crate::errors::AppErrorKind::UnavailableContent);
        assert!(err.user_message().contains("Douyin"));

        // The technical detail carries the evidence a maintainer needs: which
        // state blob was present and which post keys were absent.
        let detail = err.technical_detail().unwrap_or_default();
        assert!(
            detail.contains("state_blobs=[_ROUTER_DATA]"),
            "got {detail}"
        );
        assert!(detail.contains("aweme_keys=[]"), "got {detail}");
    }

    #[tokio::test]
    async fn the_anti_bot_shell_is_a_resolution_failure_not_an_unavailable_post() {
        // A desktop-UA shell has no embedded state at all. That is a page the
        // app could not read, which is a different statement from "the post is
        // unavailable", and it keeps a distinct error kind.
        let client = MockHttpClient::new().with_body(
            "https://www.iesdouyin.com/share/video/7281234567890123456/",
            crate::resolvers::douyin::extract::tests::sample_antibot_shell_page(),
        );

        let resolver = DouyinResolver::new(Arc::new(client));
        let err = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Resolution);
    }

    // --- the platform's own URL is asked for first ---------------------------

    #[tokio::test]
    async fn expand_keeps_the_landing_url_verbatim_next_to_the_canonical_form() {
        let landing = "https://www.iesdouyin.com/share/video/7281234567890123456/\
                       ?from_ssr=1&share_sign=abc123&region=US";
        let client = MockHttpClient::new().with_redirect(VIDEO_TEST_URL, landing);
        let resolver = DouyinResolver::new(Arc::new(client));

        let resolved = resolver.expand_short_link(VIDEO_TEST_URL).await.unwrap();

        // The clean form is still what identity and display use.
        assert_eq!(
            resolved.canonical,
            "https://www.douyin.com/video/7281234567890123456"
        );
        // And the landing URL — parameters and all — is preserved separately.
        assert_eq!(resolved.resolved_url.as_deref(), Some(landing.trim()));
        assert_eq!(resolved.original, VIDEO_TEST_URL);
    }

    #[tokio::test]
    async fn a_concrete_post_url_has_no_landing_url() {
        let resolver = DouyinResolver::new(Arc::new(MockHttpClient::new()));
        let resolved = resolver
            .expand_short_link("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap();
        assert!(resolved.resolved_url.is_none());
    }

    #[tokio::test]
    async fn the_landing_url_is_fetched_before_any_reconstructed_host() {
        // Only the verbatim landing URL serves a page. Every reconstructed host
        // has nothing registered, so resolution can only succeed if the landing
        // URL is tried first — which is the whole point of keeping it.
        let landing = "https://www.iesdouyin.com/share/video/7281234567890123456/\
                       ?from_ssr=1&share_sign=abc123";
        let client = MockHttpClient::new()
            .with_redirect(VIDEO_TEST_URL, landing)
            .with_body(
                landing,
                crate::resolvers::douyin::extract::tests::sample_share_page(),
            );

        let resolver = DouyinResolver::new(Arc::new(client));
        let post = resolver.resolve_post(VIDEO_TEST_URL).await.unwrap();

        assert_eq!(post.platform_post_id, "7281234567890123456");
        assert!(!post.media_items.is_empty());
        assert_eq!(post.url, VIDEO_TEST_URL);
    }

    #[tokio::test]
    async fn a_resolved_post_carries_normalized_media() {
        let client = MockHttpClient::new().with_body(
            "https://www.iesdouyin.com/share/video/7281234567890123456/",
            crate::resolvers::douyin::extract::tests::sample_share_page(),
        );
        let resolver = DouyinResolver::new(Arc::new(client));

        let post = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap();

        assert_eq!(post.platform_post_id, "7281234567890123456");
        assert_eq!(post.media_type, MediaType::Video);
        assert_eq!(
            post.caption.as_deref(),
            Some("Sunset timelapse {golden hour}")
        );
        // The fixture carries a video and a music track.
        assert_eq!(post.media_items.len(), 2);
        let video = post
            .media_items
            .iter()
            .find(|item| item.media_type == MediaType::Video)
            .expect("video item");
        assert!(!video.variants.is_empty());

        // resolve_media returns what resolve_post already attached.
        let media = resolver.resolve_media(&post).await.unwrap();
        assert_eq!(media.len(), post.media_items.len());
    }

    // --- the browser fallback -----------------------------------------------
    //
    // The rule under test throughout: the browser is used only when the served
    // pages contain no post, and only when the user has one available.

    #[tokio::test]
    async fn http_success_never_starts_a_browser() {
        let renderer = Arc::new(browser_with_tracks());
        let client = MockHttpClient::new().with_body(
            "https://www.iesdouyin.com/share/video/7281234567890123456/",
            crate::resolvers::douyin::extract::tests::sample_share_page(),
        );
        let resolver = DouyinResolver::with_renderer(Arc::new(client), renderer.clone());

        let post = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap();

        assert_eq!(
            post.metadata["resolution"]["via"],
            serde_json::json!("http")
        );
        assert_eq!(
            renderer.calls(),
            0,
            "a page that already carried the post must not cost a browser launch"
        );
    }

    #[tokio::test]
    async fn a_page_without_the_post_falls_back_to_the_browser() {
        let renderer = Arc::new(browser_with_tracks());
        let resolver =
            DouyinResolver::with_renderer(Arc::new(config_only_client()), renderer.clone());

        let post = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap();

        assert_eq!(renderer.calls(), 1);
        assert_eq!(
            post.metadata["resolution"]["via"],
            serde_json::json!("browser")
        );
        // The caption comes from the page title, with the site suffix removed.
        assert_eq!(post.caption.as_deref(), Some("A caption"));
        // Both tracks are represented, and the layout is stated rather than
        // implied.
        assert_eq!(post.media_items.len(), 2);
        let video = post
            .media_items
            .iter()
            .find(|item| item.media_type == MediaType::Video)
            .expect("video item");
        let audio = post
            .media_items
            .iter()
            .find(|item| item.media_type == MediaType::Audio)
            .expect("audio item");
        // The video's audio arrives as its own rendition, so the video item says
        // so: `create_download` reads that to fetch and mux both tracks. The
        // audio item is a standalone audio track.
        assert_eq!(
            video.metadata["track_layout"],
            serde_json::json!("audio_video")
        );
        assert_eq!(
            audio.metadata["track_layout"],
            serde_json::json!("audio_only")
        );
    }

    #[tokio::test]
    async fn an_unavailable_browser_reports_the_http_reason_and_is_not_started() {
        let renderer = Arc::new(MockPageRenderer::unavailable());
        let resolver =
            DouyinResolver::with_renderer(Arc::new(config_only_client()), renderer.clone());

        let err = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap_err();

        assert_eq!(renderer.calls(), 0);
        // The user is told what the source did, not about a browser they never
        // asked for and may not want.
        assert_eq!(err.kind(), AppErrorKind::UnavailableContent);
    }

    #[tokio::test]
    async fn a_browser_that_fails_keeps_the_http_reason_in_the_detail() {
        let renderer = Arc::new(MockPageRenderer::failing(AppError::browser_unavailable(
            "the browser would not start",
        )));
        let resolver =
            DouyinResolver::with_renderer(Arc::new(config_only_client()), renderer.clone());

        let err = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap_err();

        assert_eq!(renderer.calls(), 1);
        assert_eq!(err.kind(), AppErrorKind::BrowserUnavailable);
        // Both halves of the story survive into the diagnostic.
        let detail = err.technical_detail().unwrap_or_default();
        assert!(detail.contains("after HTTP"), "got {detail}");
    }

    #[tokio::test]
    async fn a_browser_that_finds_no_media_is_reported_as_unavailable() {
        let renderer = Arc::new(MockPageRenderer::returning(Vec::new()));
        let resolver =
            DouyinResolver::with_renderer(Arc::new(config_only_client()), renderer.clone());

        let err = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap_err();

        assert_eq!(err.kind(), AppErrorKind::UnavailableContent);
        assert!(err.user_message().contains("browser"));
    }

    #[tokio::test]
    async fn a_transport_failure_does_not_start_a_browser() {
        // Nothing is registered, so every host fails to connect. Rendering would
        // fail for the same reason, so it must not be attempted.
        let renderer = Arc::new(browser_with_tracks());
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());

        let err = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap_err();

        assert_eq!(err.kind(), AppErrorKind::Network);
        assert_eq!(renderer.calls(), 0);
    }

    #[tokio::test]
    async fn sources_the_browser_returns_are_normalized_not_passed_through() {
        // Only fetchable URLs reach the post; the renderer's parser is the last
        // place a page can influence what gets downloaded.
        let renderer = Arc::new(MockPageRenderer::returning(vec![
            MediaSource::test("blob:https://www.douyin.com/abc", MediaSourceKind::Video),
            MediaSource::test(
                "https://cdn.example/media-video-avc1/",
                MediaSourceKind::Video,
            ),
        ]));
        let resolver =
            DouyinResolver::with_renderer(Arc::new(config_only_client()), renderer.clone());

        let post = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap();

        assert_eq!(post.media_items.len(), 1);
        assert!(post.media_items[0]
            .variants
            .iter()
            .all(|variant| variant.source_url.starts_with("https://")));
    }

    // --- tracks the page fetched for itself -----------------------------------

    #[tokio::test]
    async fn audio_only_the_page_fetched_is_added_to_what_the_player_named() {
        // The player names the video. The page fetched the audio itself, so a
        // resolver that read only the player would report a silent video — which
        // is exactly the bug this covers.
        let renderer = Arc::new(
            MockPageRenderer::returning(vec![MediaSource::test(
                "https://cdn.example/media-video-avc1/?t=1",
                MediaSourceKind::Video,
            )])
            .with_title("A caption - 抖音")
            .with_network_media(vec![
                NetworkMedia::test("https://cdn.example/media-audio-und-mp4a/?t=1", "video/mp4"),
                // Traffic that is media but is not a track of this post.
                NetworkMedia::test("https://ad.example/promo.mp4", "video/mp4"),
            ]),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(config_only_client()), renderer.clone());

        let post = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap();

        assert_eq!(
            post.media_items.len(),
            2,
            "a second track must appear as its own item"
        );

        let video = post
            .media_items
            .iter()
            .find(|item| item.media_type == MediaType::Video)
            .expect("video item");
        let audio = post
            .media_items
            .iter()
            .find(|item| item.media_type == MediaType::Audio)
            .expect("audio item");

        assert_eq!(video.track_layout(), Some(TrackLayout::AudioVideo));
        assert_eq!(audio.track_layout(), Some(TrackLayout::AudioOnly));
        assert!(audio.source_url.contains("media-audio"));

        // Unrelated traffic is not offered as a rendition of the post.
        assert!(
            video
                .variants
                .iter()
                .all(|v| !v.source_url.contains("ad.example")),
            "an unrelated clip must not become a rendition"
        );
        assert!(post
            .media_items
            .iter()
            .all(|item| !item.source_url.contains("ad.example")));
    }

    #[tokio::test]
    async fn a_render_that_did_not_watch_the_network_does_not_claim_video_only() {
        // Nobody looked, so the honest answer is that it could not be
        // determined — not that the source offers no audio.
        let renderer = Arc::new(
            MockPageRenderer::returning(vec![MediaSource::test(
                "https://cdn.example/media-video-avc1/",
                MediaSourceKind::Video,
            )])
            .with_title("A caption - 抖音")
            .without_network_capture(),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(config_only_client()), renderer.clone());

        let post = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap();

        assert_eq!(post.media_items.len(), 1);
        assert_eq!(
            post.media_items[0].track_layout(),
            Some(TrackLayout::Unknown)
        );
    }

    /// The live failure this change exists for.
    ///
    /// This is the shape a real render produced: the network was watched, the
    /// player named a video, and the page requested no track at all — typically
    /// because playback never started. Reading that as "the source has no audio"
    /// is what allowed a silent download to be presented as a complete one.
    #[tokio::test]
    async fn a_watched_render_that_requested_no_track_does_not_claim_video_only() {
        let renderer = Arc::new(
            MockPageRenderer::returning(vec![MediaSource::test(
                "https://cdn.example/media-video-avc1/",
                MediaSourceKind::Video,
            )])
            .with_title("A caption - 抖音"),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(config_only_client()), renderer.clone());

        let post = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap();

        assert_eq!(post.media_items.len(), 1);
        assert_eq!(
            post.media_items[0].track_layout(),
            Some(TrackLayout::Unknown),
            "no track request is not evidence that the source lacks audio"
        );
    }

    /// The other half of the same rule: a real observation does support the claim.
    /// The other half of the same rule: a real observation does support the claim.
    #[tokio::test]
    async fn a_render_that_requested_a_track_and_no_audio_reports_video_only() {
        let renderer = Arc::new(
            MockPageRenderer::returning(vec![MediaSource::test(
                "https://cdn.example/media-video-avc1/",
                MediaSourceKind::Video,
            )])
            .with_network_media(vec![NetworkMedia::test(
                "https://cdn.example/media-video-avc1/?t=1",
                "video/mp4",
            )])
            .with_title("A caption - 抖音"),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(config_only_client()), renderer.clone());

        let post = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap();

        // One video item, holding both the player's rendition and the observed one.
        assert_eq!(post.media_items.len(), 1);
        assert_eq!(
            post.media_items[0].track_layout(),
            Some(TrackLayout::VideoOnly)
        );
    }

    // --- text posts ----------------------------------------------------------

    /// A client that has a body for one post URL and no post data in it, so the
    /// browser takes over — the same path a caption-only post travels.
    fn post_page_client(post_url: &str) -> MockHttpClient {        let share = post_url.replace("www.douyin.com", "www.iesdouyin.com/share");
        let share = format!("{}/", share.trim_end_matches('/'));
        config_only_client()
            .with_body(
                post_url,
                crate::resolvers::douyin::extract::tests::sample_live_config_only_page(),
            )
            .with_body(
                &share,
                crate::resolvers::douyin::extract::tests::sample_live_config_only_page(),
            )
    }

    /// A note page that carried words and nothing else, as one renders live.
    fn text_page(title: &str, published: Option<&str>) -> MockPageRenderer {
        let diagnostics = match published {
            Some(when) => serde_json::json!({
                "imagesFound": 0, "imgTotal": 68, "gallery": "[data-e2e*=\"note\"]",
                "videoElement": false, "publishedAt": when
            }),
            None => serde_json::json!({
                "imagesFound": 0, "imgTotal": 68, "gallery": "[data-e2e*=\"note\"]",
                "videoElement": false, "publishedAt": null
            }),
        };
        MockPageRenderer::returning(Vec::new())
            .with_title(title)
            .with_payload(serde_json::json!({ "media": [], "diagnostics": diagnostics }))
    }

    #[tokio::test]
    async fn a_page_that_carried_words_and_no_media_is_a_text_post() {
        // The reported shape. A caption-only post used to arrive as "this post
        // does not contain downloadable media" — true, and no answer at all to
        // what the user actually had, which was a post with words in it.
        let renderer = Arc::new(text_page(
            "开机大吉！顺顺利利！ - 抖音",
            Some("2026-04-18T15:23:11"),
        ));
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029855")),
            renderer,
        );

        let post = resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029855")
            .await
            .expect("a text post is a success, not a failure");

        assert_eq!(post.media_type, MediaType::Text);
        // No fake video and no fake picture: nothing is offered to download.
        assert!(post.media_items.is_empty(), "a text post has no media items");
        assert_eq!(
            post.metadata["text"],
            serde_json::json!("开机大吉！顺顺利利！"),
            "the words, without the site's own suffix"
        );
        // The publish time as the page printed it, and no instant invented from
        // a wall-clock time that stated no offset.
        assert_eq!(
            post.metadata["publishedAt"],
            serde_json::json!("2026-04-18T15:23:11")
        );
        assert_eq!(post.published_at, None);
        // And its address, so the file can say where it came from.
        assert_eq!(
            post.canonical_url.as_deref(),
            Some("https://www.douyin.com/note/7629996475679029855")
        );
    }

    #[tokio::test]
    async fn a_page_with_neither_media_nor_words_is_still_a_failure() {
        // A text post is not a way to excuse a page nobody could read. With no
        // media and no title there is nothing to keep, and saying so is right.
        let renderer = Arc::new(MockPageRenderer::returning(Vec::new()));
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029856")),
            renderer,
        );

        let err = resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029856")
            .await
            .expect_err("nothing to keep");

        assert_eq!(err.kind(), AppErrorKind::UnavailableContent);
    }

    #[tokio::test]
    async fn a_page_whose_sources_produced_no_item_is_not_called_a_text_post() {
        // The trap this avoids: a page that *did* offer media and from which no
        // item could be made is a failure of this application's reading. Turning
        // that into a text post would hide a real defect behind a plausible
        // answer, and the user would never learn their post was not collected.
        let renderer = Arc::new(
            MockPageRenderer::returning(vec![
                // Offered by the player, but not a fetchable resource, so it
                // cannot become an item.
                MediaSource::test("blob:https://www.douyin.com/abc", MediaSourceKind::Video),
            ])
            .with_title("a caption that looks like a text post - 抖音"),
        );
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/video/7281234567890123457")),
            renderer,
        );

        let err = resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123457")
            .await
            .expect_err("a failed reading is not a text post");

        assert_eq!(err.kind(), AppErrorKind::UnsupportedContent);
    }

    // --- carousel collection -------------------------------------------------

    /// A picture source, as the page collector reports one.
    fn page_picture(url: &str, width: u32, height: u32) -> MediaSource {
        let mut source = MediaSource::test(url, MediaSourceKind::Image);
        source.width = Some(width);
        source.height = Some(height);
        source
    }

    /// A browser render of a note page, with the page-state the collector
    /// reports for it.
    fn gallery_render(
        pictures: Vec<MediaSource>,
        declared_total: Option<u64>,
    ) -> MockPageRenderer {
        MockPageRenderer::returning(pictures)
            .with_title("一个相册 - 抖音")
            .with_payload(serde_json::json!({
                "media": [],
                "diagnostics": {
                    "imagesFound": 4,
                    "gallery": "[data-e2e*=\"note\"]",
                    "slideTrack": "DIV",
                    "slideTrackCount": 4,
                    "declaredTotal": declared_total,
                    "videoElement": false
                }
            }))
    }

    #[tokio::test]
    async fn a_gallery_that_stated_a_bigger_album_does_not_gain_slides() {
        // The page says nine and four were collected. Four is what this
        // application saw, and padding the other five to match a number the page
        // stated would be inventing downloads for pictures nobody has. The
        // disagreement is reported, and the collection is not dressed up.
        let renderer = Arc::new(gallery_render(
            vec![
                page_picture("https://p3-cdn.douyinpic.com/s1~noop.jpeg", 1080, 1920),
                page_picture("https://p3-cdn.douyinpic.com/s2~noop.jpeg", 1080, 1920),
                page_picture("https://p3-cdn.douyinpic.com/s3~noop.jpeg", 1080, 1920),
                page_picture("https://p3-cdn.douyinpic.com/s4~noop.jpeg", 1080, 1920),
            ],
            Some(9),
        ));
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029861")),
            renderer,
        );

        let post = resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029861")
            .await
            .expect("resolved");

        assert_eq!(
            post.media_items.len(),
            4,
            "the count is what was collected, never what the page declared"
        );
        assert_eq!(post.media_type, MediaType::Carousel);
    }

    #[tokio::test]
    async fn a_gallery_with_no_stated_total_is_taken_at_what_it_shows() {
        // No counter on the page means no claim of completeness is available to
        // make — and none is made.
        let renderer = Arc::new(gallery_render(
            vec![
                page_picture("https://p3-cdn.douyinpic.com/s1~noop.jpeg", 1080, 1920),
                page_picture("https://p3-cdn.douyinpic.com/s2~noop.jpeg", 1080, 1920),
            ],
            None,
        ));
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029862")),
            renderer,
        );

        let post = resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029862")
            .await
            .expect("resolved");

        assert_eq!(post.media_items.len(), 2);
        assert_eq!(post.media_type, MediaType::Carousel);
    }

    #[tokio::test]
    async fn resolving_the_same_post_twice_yields_the_same_slides_in_the_same_order() {
        // Re-resolution is what the chooser, the retry path and a refresh all
        // do. If it renumbered the pictures, "the third one" would mean
        // something different after every refresh and a selection made before
        // one would point at another picture after it.
        let pictures = || {
            vec![
                page_picture("https://p3-cdn.douyinpic.com/a~noop.jpeg", 1080, 1920),
                page_picture("https://p3-cdn.douyinpic.com/b~tplv-x.webp", 1080, 1920),
                // The same picture as the first, served differently — it must
                // collapse into it both times, and in the same place.
                page_picture("https://p3-cdn.douyinpic.com/a~tplv-y.webp", 2160, 3840),
                page_picture("https://p3-cdn.douyinpic.com/c~noop.jpeg", 1080, 1920),
            ]
        };
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029863")),
            Arc::new(gallery_render(pictures(), Some(3))),
        );

        let first = resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029863")
            .await
            .expect("first");
        let second = resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029863")
            .await
            .expect("second");

        let identity = |post: &Post| {
            post.media_items
                .iter()
                .map(|item| {
                    (
                        crate::resolvers::douyin::player::logical_slide_key(&item.source_url),
                        item.metadata.get("slide").cloned(),
                        item.width,
                        item.height,
                    )
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(identity(&first), identity(&second), "the same slides, the same order");
        assert_eq!(first.media_items.len(), 3, "the duplicate rendition is one slide");
        // And the larger rendition of the repeated picture is the one kept, both
        // times, so identity does not cost resolution.
        assert_eq!(first.media_items[0].width, Some(2160));
    }

    /// Three pictures, standing in for a gallery with more in it.
    fn three_pictures() -> Vec<MediaSource> {
        (1..=3)
            .map(|index| {
                page_picture(
                    &format!("https://p3-cdn.douyinpic.com/s{index}~noop.jpeg"),
                    1080,
                    1920,
                )
            })
            .collect()
    }

    /// A page-state claiming a given number of slides was found against a given
    /// declared total.
    fn gallery_state(found: u64, declared: u64) -> serde_json::Value {
        serde_json::json!({
            "media": [],
            "diagnostics": {
                "imagesFound": found,
                "imagesBeforeTrack": found,
                "trackIncluded": found,
                "gallery": "[data-e2e*=\"note\"]",
                "slideTrack": "DIV",
                "slideTrackCount": declared,
                "declaredTotal": declared,
                "videoElement": false
            }
        })
    }

    #[tokio::test]
    async fn a_gallery_that_came_back_short_is_looked_at_once_more() {
        // Measured on the installed application: of four resolutions of the same
        // nine-slide post, two returned a partially drawn gallery while the page
        // still said nine. Rendering once more is what turns that into the album
        // the page says is there.
        let renderer = Arc::new(
            MockPageRenderer::returning(three_pictures()).with_payload_sequence(vec![
                gallery_state(1, 3),
                gallery_state(3, 3),
            ]),
        );
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029864")),
            renderer.clone(),
        );

        let post = resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029864")
            .await
            .expect("resolved");

        assert_eq!(renderer.calls(), 2, "the short render was followed by one more");
        assert_eq!(post.media_items.len(), 3);
    }

    #[tokio::test]
    async fn a_gallery_that_matches_its_own_count_is_not_rendered_twice() {
        // The other half: an extra render is paid for only when the page said
        // something was missing. A page that agrees with itself is read once.
        let renderer = Arc::new(
            MockPageRenderer::returning(three_pictures())
                .with_payload_sequence(vec![gallery_state(3, 3), gallery_state(3, 3)]),
        );
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029865")),
            renderer.clone(),
        );

        let post = resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029865")
            .await
            .expect("resolved");

        assert_eq!(renderer.calls(), 1);
        assert_eq!(post.media_items.len(), 3);
    }

    #[tokio::test]
    async fn looking_again_never_makes_the_answer_worse() {
        // A second look that somehow found less must not replace the first. The
        // best result is kept, not the last.
        let renderer = Arc::new(
            MockPageRenderer::returning(three_pictures()).with_payload_sequence(vec![
                gallery_state(2, 3),
                gallery_state(1, 3),
            ]),
        );
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029866")),
            renderer.clone(),
        );

        let post = resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029866")
            .await
            .expect("resolved");

        // Two renders, and the first one's count is what stands.
        assert_eq!(renderer.calls(), 2);
        assert_eq!(post.media_items.len(), 3);
    }

    /// The failure the renderer raises when a page never settles. Retryable by
    /// construction, and the one measured on the installed application.
    fn page_never_settled() -> AppError {
        AppError::browser_unavailable("the page never became ready in the browser")
            .with_user_message("The page did not finish loading.")
            .with_retryable(true)
    }

    #[tokio::test]
    async fn a_transient_render_failure_is_rendered_again() {
        // Measured: one render in three of the same carousel simply never became
        // ready, while the others resolved in seconds. The renderer's own retry
        // looks again at the same page on the same browser; asking the renderer
        // for the page a second time is what reaches the path that can also
        // replace the browser, and it is what turns that run into a resolution.
        let renderer = Arc::new(
            MockPageRenderer::returning(three_pictures())
                .with_payload(gallery_state(3, 3))
                .after_failing(page_never_settled()),
        );
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029867")),
            renderer.clone(),
        );

        let post = resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029867")
            .await
            .expect("the second render is the one that worked");

        assert_eq!(renderer.calls(), 2);
        assert_eq!(post.media_items.len(), 3);
    }

    #[tokio::test]
    async fn a_sign_in_wall_is_not_rendered_again() {
        // The other half, and the one that matters most: a source asking for a
        // sign-in gives the same answer however many times it is asked, and a
        // render loop that kept asking is exactly the endless wait this must not
        // become. Retried once means twice; not retried means once.
        let renderer = Arc::new(
            MockPageRenderer::returning(three_pictures())
                .with_payload(gallery_state(3, 3))
                .after_failing(
                    AppError::authentication_required("this post needs a signed-in session")
                        .with_user_message("Sign in to read this post."),
                ),
        );
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029868")),
            renderer.clone(),
        );

        let err = resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029868")
            .await
            .expect_err("a sign-in wall is an answer, not a hiccup");

        assert_eq!(renderer.calls(), 1, "it was asked once and believed");
        assert_eq!(err.kind(), AppErrorKind::AuthenticationRequired);
    }

    #[tokio::test]
    async fn a_page_that_never_settles_keeps_its_own_error_after_the_last_attempt() {
        // Bounded, and the failure the user finally sees is the real one rather
        // than something invented about the retry.
        let renderer = Arc::new(
            MockPageRenderer::returning(three_pictures())
                .with_payload(gallery_state(3, 3))
                .after_failing(page_never_settled())
                .after_failing(page_never_settled()),
        );
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029869")),
            renderer.clone(),
        );

        let err = resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029869")
            .await
            .expect_err("two failed renders is a failed resolution");

        assert_eq!(renderer.calls(), 2, "and no more than the bound allows");
        assert_eq!(err.kind(), AppErrorKind::BrowserUnavailable);
        assert!(
            err.technical_detail()
                .map(|detail| detail.contains("after HTTP"))
                .unwrap_or(false),
            "the error still carries why the plain request failed too"
        );
    }

    #[tokio::test]
    async fn a_short_gallery_is_not_treated_as_a_failed_render() {
        // The two are deliberately distinct: a page that loaded and drew four of
        // nine slides is a collection problem, and is retried by the count
        // comparison; a page that never loaded is a render problem. Conflating
        // them would spend the render budget on the wrong question.
        let renderer = Arc::new(
            MockPageRenderer::returning(three_pictures()).with_payload_sequence(vec![
                gallery_state(1, 3),
                gallery_state(3, 3),
            ]),
        );
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029870")),
            renderer.clone(),
        );

        let post = resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029870")
            .await
            .expect("resolved");

        assert_eq!(renderer.calls(), 2);
        assert_eq!(post.media_items.len(), 3);
        // Nothing was treated as a render failure: the first look answered.
        assert!(post.metadata.contains_key("resolution"));
    }

    #[tokio::test]
    async fn a_working_page_is_never_asked_whether_it_is_a_page_of_words() {
        // The whole reason the text question is asked on a *second* attempt and
        // not folded into the first: a page that resolves today must be read
        // exactly as it is read today. Adding the text case to the readiness
        // predicate was tried and measured, and it made every note page become
        // ready on its title a second in, before its gallery had drawn.
        let renderer = Arc::new(
            MockPageRenderer::returning(three_pictures())
                .with_payload_sequence(vec![gallery_state(3, 3), gallery_state(3, 3)]),
        );
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029871")),
            renderer.clone(),
        );

        resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029871")
            .await
            .expect("resolved");

        assert_eq!(renderer.calls(), 1);
        let asked = renderer.ready_conditions();
        assert!(
            !asked[0].contains("__dyTextReady"),
            "the only page read was read as a media page: {}",
            asked[0]
        );
    }

    #[tokio::test]
    async fn a_page_that_never_settles_is_asked_a_different_question() {
        // And the converse, stated where it can be read: the retry asks whether
        // the page is words. That is the *only* path by which a caption-only post
        // can be reached, because it satisfies none of the media predicates.
        let renderer = Arc::new(
            MockPageRenderer::returning(three_pictures())
                .with_payload(gallery_state(3, 3))
                .after_failing(page_never_settled()),
        );
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029872")),
            renderer.clone(),
        );

        resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029872")
            .await
            .expect("the second render is the one that worked");

        let asked = renderer.ready_conditions();
        assert_eq!(asked.len(), 2);
        assert!(!asked[0].contains("__dyTextReady"));
        assert!(
            asked[1].contains("__dyTextReady"),
            "the second attempt asks about words: {}",
            asked[1]
        );
    }

    #[tokio::test]
    async fn a_short_gallery_is_asked_the_same_question_again() {
        // A page that loaded and drew four of nine slides is a *collection*
        // problem, not a kind question: the second look must not wander off
        // asking whether it is a page of words.
        let renderer = Arc::new(
            MockPageRenderer::returning(three_pictures()).with_payload_sequence(vec![
                gallery_state(1, 3),
                gallery_state(3, 3),
            ]),
        );
        let resolver = DouyinResolver::with_renderer(
            Arc::new(post_page_client("https://www.douyin.com/note/7629996475679029873")),
            renderer.clone(),
        );

        resolver
            .resolve_post("https://www.douyin.com/note/7629996475679029873")
            .await
            .expect("resolved");

        let asked = renderer.ready_conditions();
        assert_eq!(asked.len(), 2);
        assert_eq!(asked[0], asked[1], "the same question, asked twice");
    }

    // --- profile resolution --------------------------------------------------

    const PROFILE_ID: &str = "MS4wLjABAAAAexample";
    fn profile_url() -> String {
        format!("https://www.douyin.com/user/{PROFILE_ID}")
    }

    /// A profile page that named its creator and listed two posts.
    fn profile_page_payload() -> serde_json::Value {
        serde_json::json!({
            "creator": {
                "name": "郭虎",
                "handle": "42299195146",
                "signature": "导演",
                "avatarUrl": "https://p3.douyinpic.com/avatar.jpeg",
                "verified": true,
                "postCount": 50
            },
            "posts": [
                {
                    "id": "7091303675863321870",
                    "kind": "video",
                    "url": "https://www.douyin.com/video/7091303675863321870",
                    "caption": "第一条",
                    "thumbnailUrl": "https://p3.douyinpic.com/a.jpeg",
                    "hasImage": true
                },
                {
                    "id": "7473536022312144155",
                    "kind": "note",
                    "url": "https://www.douyin.com/note/7473536022312144155",
                    "caption": "第二条",
                    "thumbnailUrl": "https://p3.douyinpic.com/b.jpeg",
                    "hasImage": true
                }
            ]
        })
    }

    fn resolver_with(payload: serde_json::Value) -> DouyinResolver {
        let renderer = Arc::new(MockPageRenderer::returning(Vec::new()).with_payload(payload));
        DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer)
    }

    #[tokio::test]
    async fn a_profile_link_is_identified_from_the_url_alone_without_a_browser() {
        // No renderer: the identity is still knowable, and the display name is
        // the platform id rather than an invented one.
        let resolver = DouyinResolver::new(Arc::new(MockHttpClient::new()));
        let creator = resolver.resolve_profile(&profile_url()).await.unwrap();

        assert_eq!(creator.platform_id, PROFILE_ID);
        assert_eq!(creator.display_name, PROFILE_ID);
        assert!(creator.handle.is_none());
        assert_eq!(
            creator.metadata["resolution"]["stage"],
            serde_json::json!("url_only")
        );
    }

    #[tokio::test]
    async fn a_rendered_profile_supplies_the_creators_details() {
        let resolver = resolver_with(profile_page_payload());
        let creator = resolver.resolve_profile(&profile_url()).await.unwrap();

        assert_eq!(creator.display_name, "郭虎");
        assert_eq!(creator.handle.as_deref(), Some("@42299195146"));
        assert_eq!(creator.description.as_deref(), Some("导演"));
        assert_eq!(creator.post_count, Some(50));
        assert!(creator.verified);
        assert_eq!(
            creator.avatar_url.as_deref(),
            Some("https://p3.douyinpic.com/avatar.jpeg")
        );
        assert_eq!(
            creator.metadata["resolution"]["stage"],
            serde_json::json!("resolved")
        );
        assert_eq!(
            creator.metadata["resolution"]["via"],
            serde_json::json!("browser")
        );
    }

    #[tokio::test]
    async fn a_short_profile_link_is_expanded_before_it_is_resolved() {
        let landing =
            format!("https://www.douyin.com/user/{PROFILE_ID}?previous_page=app_code_link");
        let renderer =
            Arc::new(MockPageRenderer::returning(Vec::new()).with_payload(profile_page_payload()));
        let client = MockHttpClient::new()
            .with_redirect(crate::resolvers::fixture::FIXTURE_PROFILE_URL, &landing);
        let resolver = DouyinResolver::with_renderer(Arc::new(client), renderer.clone());

        let creator = resolver
            .resolve_profile(crate::resolvers::fixture::FIXTURE_PROFILE_URL)
            .await
            .unwrap();

        // The short link's code is not an identity; the expanded one is.
        assert_eq!(creator.platform_id, PROFILE_ID);
        assert_eq!(creator.display_name, "郭虎");
        assert_eq!(renderer.calls(), 1);
    }

    #[tokio::test]
    async fn a_page_that_names_nobody_degrades_to_the_identity_and_says_so() {
        // The shape a signed-out visitor actually gets is a rendered shell with
        // no profile data in it. That must not become a fabricated creator.
        let resolver = resolver_with(serde_json::json!({ "creator": {}, "posts": [] }));
        let creator = resolver.resolve_profile(&profile_url()).await.unwrap();

        assert_eq!(creator.display_name, PROFILE_ID);
        assert_eq!(
            creator.metadata["resolution"]["stage"],
            serde_json::json!("url_only")
        );
        assert_eq!(
            creator.metadata["resolution"]["degraded"],
            serde_json::json!("page_rendered_without_identity")
        );
    }

    #[tokio::test]
    async fn a_render_failure_does_not_cost_the_identity() {
        let renderer = Arc::new(MockPageRenderer::failing(AppError::browser_unavailable(
            "the browser would not start",
        )));
        let resolver = DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer);

        let creator = resolver.resolve_profile(&profile_url()).await.unwrap();
        assert_eq!(creator.platform_id, PROFILE_ID);
        assert_eq!(
            creator.metadata["resolution"]["degraded"],
            serde_json::json!("browser_unavailable")
        );
    }

    #[tokio::test]
    async fn a_post_link_is_not_accepted_as_a_profile() {
        let resolver = DouyinResolver::new(Arc::new(MockHttpClient::new()));
        let err = resolver
            .resolve_profile("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), AppErrorKind::Validation);
        assert!(err.user_message().contains("profile"));
    }

    // --- profile listing -----------------------------------------------------

    fn listed_creator() -> Creator {
        let mut creator = Creator::new(Platform::Douyin, PROFILE_ID, "郭虎", profile_url());
        creator.post_count = Some(2);
        creator
    }

    #[tokio::test]
    async fn every_douyin_page_render_asks_for_an_ordinary_window() {
        // Both signed-out Douyin pages this app renders — a creator's profile
        // and a post — are served only to an ordinary browsing window. Verified
        // against the live site: the identical request is refused headlessly,
        // the video page being redirected to the feed with no player at all.
        // So every render this resolver asks for wants a window, and a
        // regression here would silently break both flows.
        let renderer =
            Arc::new(MockPageRenderer::returning(Vec::new()).with_payload(four_post_payload()));
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());

        resolver
            .list_posts(&listed_creator(), PageRequest::first(2))
            .await
            .unwrap();
        assert_eq!(
            renderer.last_visibility(),
            Some(BrowserVisibility::Windowed)
        );

        let renderer = Arc::new(browser_with_tracks());
        let resolver =
            DouyinResolver::with_renderer(Arc::new(config_only_client()), renderer.clone());
        resolver
            .resolve_post("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap();
        assert_eq!(
            renderer.last_visibility(),
            Some(BrowserVisibility::Windowed)
        );
    }

    #[tokio::test]
    async fn the_post_render_gives_the_player_a_widened_budget() {
        // The player took about fifteen seconds to appear in a real window, so
        // the ready budget has to be comfortably larger than that.
        assert!(POST_READY_TIMEOUT >= std::time::Duration::from_secs(30));
        assert!(POST_READY_TIMEOUT.as_secs() < 110);
    }

    #[tokio::test]
    async fn the_listing_request_carries_the_refresh_retry_and_a_bounded_budget() {
        // The predicate the resolver hands the renderer has to be the one that
        // can recover, and its retry has to be bounded. This is asserted on the
        // built request rather than on the script text alone, so a change to
        // the wiring is caught too.
        let settle = profile::listing_settle(19, super::LISTING_REFRESH_LIMIT);
        assert!(settle.contains("刷新"));
        assert!(settle.contains("__dyListingRefresh<2"));
        assert_eq!(super::LISTING_REFRESH_LIMIT, 2);
    }

    #[tokio::test]
    async fn a_listing_page_returns_normalized_posts() {
        let resolver = resolver_with(profile_page_payload());
        let creator = listed_creator();
        let page = resolver
            .list_posts(&creator, PageRequest::first(18))
            .await
            .unwrap();

        assert_eq!(page.posts.len(), 2);
        assert!(!page.has_more);
        assert!(page.next_cursor.is_none());
        assert_eq!(page.total_known, Some(2));

        let first = &page.posts[0];
        assert_eq!(first.platform_post_id, "7091303675863321870");
        assert_eq!(first.media_type, MediaType::Video);
        assert_eq!(first.caption.as_deref(), Some("第一条"));
        assert_eq!(first.status, ContentStatus::New);
        // A listing states that a post exists, never that its media is known.
        assert!(first.media_items.is_empty());
        assert_eq!(first.creator_id.as_deref(), Some(creator.id.as_str()));
    }

    #[tokio::test]
    async fn a_listing_of_mixed_content_keeps_each_post_its_own_and_repeats_none() {
        // A real profile is not one kind of thing. The listing must carry each
        // post's own type, address and identity across pages, and must not hand
        // the same post over twice — a duplicate would put the same picture in
        // the download queue twice under two names.
        let mixed = || {
            serde_json::json!({
                "creator": { "name": "郭虎", "postCount": 4 },
                "posts": [
                    card("7091303675863321881", "video"),
                    card("7091303675863321882", "note"),
                    card("7091303675863321883", "video"),
                    card("7091303675863321884", "note")
                ]
            })
        };
        let renderer = Arc::new(
            MockPageRenderer::returning(Vec::new())
                .with_payload_sequence(vec![mixed(), mixed()]),
        );
        let resolver = DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer);
        let creator = creator_with_declared_posts(4);

        let first = resolver
            .list_posts(&creator, PageRequest::first(2))
            .await
            .unwrap();
        assert_eq!(first.posts[0].media_type, MediaType::Video);
        assert_eq!(first.posts[1].media_type, MediaType::Carousel);
        assert_eq!(
            first.posts[1].url,
            "https://www.douyin.com/note/7091303675863321882",
            "a note keeps its own address rather than being rewritten as a video"
        );

        let cursor = first.next_cursor.clone().expect("a cursor");
        let second = resolver
            .list_posts(&creator, PageRequest::next(&cursor, 2))
            .await
            .unwrap();

        let mut ids: Vec<String> = first
            .posts
            .iter()
            .chain(second.posts.iter())
            .map(|post| post.platform_post_id.clone())
            .collect();
        let total = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), total, "no post is listed twice across the pages");
        // And the types survive the second page too.
        assert_eq!(second.posts[0].media_type, MediaType::Video);
        assert_eq!(second.posts[1].media_type, MediaType::Carousel);
    }

    #[tokio::test]
    async fn the_cursor_walks_the_listing_and_then_stops() {
        // The grid holds four cards; a page at a time is read from it. Whether
        // another page exists is observed from the grid rather than assumed.
        let renderer = Arc::new(
            MockPageRenderer::returning(Vec::new())
                .with_payload_sequence(vec![four_post_payload(), four_post_payload()]),
        );
        let resolver = DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer);
        let creator = listed_creator();

        let first = resolver
            .list_posts(&creator, PageRequest::first(2))
            .await
            .unwrap();
        assert_eq!(first.posts.len(), 2);
        assert!(first.has_more, "the grid still held more than the slice");
        let cursor = first.next_cursor.clone().expect("a cursor");

        let second = resolver
            .list_posts(&creator, PageRequest::next(&cursor, 2))
            .await
            .unwrap();
        assert_eq!(second.posts.len(), 2);
        // The first page's posts are not repeated on the second.
        assert_eq!(second.posts[0].platform_post_id, "7091303675863321872");
        assert!(!second.has_more);
        assert!(second.next_cursor.is_none());
    }

    #[tokio::test]
    async fn a_listing_that_fails_its_first_look_is_taken_once_more() {
        // The shape the live source actually has: the first look at the grid
        // comes back empty, the next one has the posts. One render is not
        // enough evidence to call a listing withheld.
        let renderer = Arc::new(
            MockPageRenderer::returning(Vec::new()).with_payload_sequence(vec![
                serde_json::json!({ "creator": { "name": "郭虎" }, "posts": [] }),
                profile_page_payload(),
            ]),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());

        let page = resolver
            .list_posts(&listed_creator(), PageRequest::first(18))
            .await
            .expect("the second look must be used");
        assert_eq!(page.posts.len(), 2);
        assert_eq!(
            renderer.calls(),
            2,
            "an empty first look is followed by one more"
        );
    }

    #[tokio::test]
    async fn a_refusing_listing_is_asked_twice_and_then_believed() {
        // Bounded: a source that refuses twice is taken at its word rather than
        // asked again and again.
        let renderer = Arc::new(
            MockPageRenderer::returning(Vec::new())
                .with_payload(serde_json::json!({ "creator": { "name": "郭虎" }, "posts": [] })),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());
        let mut creator = listed_creator();
        creator.post_count = Some(50);

        let err = resolver
            .list_posts(&creator, PageRequest::first(18))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), AppErrorKind::UnavailableContent);
        assert_eq!(
            renderer.calls(),
            LISTING_RENDER_ATTEMPTS,
            "the source is asked exactly as often as the budget allows, then believed"
        );
    }

    /// A rendered listing holding `count` cards.
    ///
    /// Only the cards matter here: a listing's *declared* count comes from the
    /// creator the caller resolved, not from the page being listed.
    fn listing_payload(count: usize) -> serde_json::Value {
        let posts: Vec<serde_json::Value> = (0..count)
            .map(|index| card(&format!("70913036758633218{index:02}"), "video"))
            .collect();
        serde_json::json!({ "creator": { "name": "郭虎" }, "posts": posts })
    }

    fn creator_declaring(declared: u32) -> Creator {
        let mut creator = listed_creator();
        creator.post_count = Some(declared);
        creator
    }

    /// The second page of an 18-a-page listing.
    fn second_page() -> PageRequest {
        PageRequest {
            cursor: Some("18".into()),
            limit: 18,
            authenticated: false,
        }
    }

    #[tokio::test]
    async fn a_grid_that_stopped_short_is_looked_at_again() {
        // The exact shape a live run produced, and the thing the user worked
        // around by hand: the grid stops at precisely what the page asked for —
        // `raw_posts` landing on the target and going no further — while the
        // profile's own count says there are posts beyond it. The listing has
        // not ended; it has not finished arriving. One more look is the whole
        // of what pressing Refresh and loading again was doing.
        let renderer = Arc::new(
            MockPageRenderer::returning(Vec::new()).with_payload_sequence(vec![
                listing_payload(36),
                listing_payload(54),
            ]),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());

        let page = resolver
            .list_posts(&creator_declaring(50), second_page())
            .await
            .expect("the second look must be used");

        assert_eq!(renderer.calls(), 2, "a stalled grid is worth one more look");
        assert_eq!(page.posts.len(), 18, "this page is still one page of 18");
        assert!(page.has_more, "the grid had more once it had arrived");
        assert_eq!(page.next_cursor.as_deref(), Some("36"));
    }

    #[tokio::test]
    async fn a_short_grid_with_no_stated_count_is_taken_at_its_word() {
        // Nothing to weigh a short grid against, so nothing is claimed: the
        // page is believed and the caller is told the source stopped there.
        // Looking again would be spending a render on a hunch.
        let renderer = Arc::new(
            MockPageRenderer::returning(Vec::new()).with_payload(listing_payload(36)),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());
        let mut creator = listed_creator();
        creator.post_count = None;

        let page = resolver
            .list_posts(&creator, second_page())
            .await
            .expect("a short grid is still a page, not an error");

        assert_eq!(renderer.calls(), 1, "there is nothing to weigh it against");
        assert!(!page.has_more, "the source is taken at its word");
    }

    #[tokio::test]
    async fn a_grid_that_cleared_the_target_is_not_looked_at_again() {
        // The ordinary case, which must not pay for the stalled one: a grid
        // holding more than this page needs has plainly finished arriving.
        let renderer = Arc::new(
            MockPageRenderer::returning(Vec::new()).with_payload(listing_payload(54)),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());

        let page = resolver
            .list_posts(&creator_declaring(50), second_page())
            .await
            .expect("a page");

        assert_eq!(renderer.calls(), 1, "no second look was needed");
        assert!(page.has_more);
    }

    #[tokio::test]
    async fn a_grid_that_stays_short_is_asked_the_budget_and_then_believed() {
        // Bounded. A source that never finishes arriving is asked as often as
        // the budget allows and then believed — which is where "the source
        // stopped early" comes from, and what keeps the retry from becoming a
        // loop against a profile that really does end there.
        let renderer = Arc::new(
            MockPageRenderer::returning(Vec::new()).with_payload(listing_payload(36)),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());

        let page = resolver
            .list_posts(&creator_declaring(50), second_page())
            .await
            .expect("a short grid is a page, not an error");

        assert_eq!(renderer.calls(), LISTING_RENDER_ATTEMPTS);
        assert!(!page.has_more, "after the budget the source is believed");
        assert_eq!(page.posts.len(), 18, "and what it did send is kept");
    }

    #[tokio::test]
    async fn a_later_look_that_finds_less_does_not_replace_a_longer_one() {
        // A second look is a worse answer to the same question, not a
        // correction of the better one. The longest look is what the listing is
        // built from, or a flaky render would quietly lose posts the source had
        // already handed over.
        let renderer = Arc::new(
            MockPageRenderer::returning(Vec::new()).with_payload_sequence(vec![
                listing_payload(36),
                listing_payload(20),
                listing_payload(20),
            ]),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());

        let page = resolver
            .list_posts(&creator_declaring(50), second_page())
            .await
            .expect("a page");

        assert_eq!(renderer.calls(), LISTING_RENDER_ATTEMPTS);
        // The tell is the page's own width. Had the shorter look replaced the
        // longer one, the grid would hold 20 and this slice would be 2 posts
        // wide instead of 18 — posts the source had already handed over, lost
        // to a flaky render.
        assert_eq!(
            page.posts.len(),
            18,
            "the longest look is what the listing is built from"
        );
        assert!(!page.has_more, "a grid of 36 has nothing beyond the 36 read");
    }

    #[tokio::test]
    async fn a_listing_that_works_first_time_is_only_taken_once() {
        // The second look must not be a cost paid on every listing.
        let renderer =
            Arc::new(MockPageRenderer::returning(Vec::new()).with_payload(profile_page_payload()));
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());

        resolver
            .list_posts(&listed_creator(), PageRequest::first(18))
            .await
            .unwrap();
        assert_eq!(
            renderer.calls(),
            1,
            "a listing that arrived is not re-fetched"
        );
    }

    #[tokio::test]
    async fn a_page_that_never_reveals_more_ends_the_listing_instead_of_looping() {
        // The grid holds four cards and never grows, so scrolling stops helping
        // after the second page. Walking the cursor must therefore terminate:
        // no cursor is offered again, no post is repeated, and the loop cannot
        // run forever asking a page for cards it does not have.
        let resolver = resolver_with(four_post_payload());
        let creator = listed_creator();
        let mut cursor: Option<String> = None;
        let mut seen: Vec<String> = Vec::new();
        let mut pages = 0;

        loop {
            let request = match &cursor {
                Some(value) => PageRequest::next(value.clone(), 2),
                None => PageRequest::first(2),
            };
            let page = resolver.list_posts(&creator, request).await.unwrap();
            pages += 1;

            for post in &page.posts {
                assert!(
                    !seen.contains(&post.platform_post_id),
                    "post {} was returned twice",
                    post.platform_post_id
                );
                seen.push(post.platform_post_id.clone());
            }

            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
            assert!(pages < 10, "the listing must terminate on its own");
        }

        assert_eq!(seen.len(), 4, "every card was returned exactly once");
        assert_eq!(
            pages, 2,
            "two pages, and no cursor offered after the grid ran out"
        );
    }

    #[tokio::test]
    async fn a_temporary_page_failure_is_reported_and_a_later_attempt_succeeds() {
        // A page that fails once and works on the next look. The failure is the
        // caller's to surface; the recovery is what stops one bad render from
        // ending the listing for good.
        let renderer = Arc::new(
            MockPageRenderer::failing(AppError::browser_unavailable("the page was not usable"))
                .with_payload(profile_page_payload()),
        );
        let resolver = DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer);
        let creator = listed_creator();

        let err = resolver
            .list_posts(&creator, PageRequest::first(18))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), AppErrorKind::BrowserUnavailable);

        let page = resolver
            .list_posts(&creator, PageRequest::first(18))
            .await
            .expect("the second look must succeed");
        assert_eq!(page.posts.len(), 2);
    }

    #[tokio::test]
    async fn a_mid_listing_refusal_is_not_reported_as_the_end_of_the_listing() {
        // The profile states fifty posts. A grid that comes back empty part-way
        // through has not reached the end of anything — the source stopped
        // answering, and saying "no more posts" would be a claim the evidence
        // does not support.
        let resolver =
            resolver_with(serde_json::json!({ "creator": { "name": "郭虎" }, "posts": [] }));
        let mut creator = listed_creator();
        creator.post_count = Some(50);

        let err = resolver
            .list_posts(&creator, PageRequest::next("1", 1))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), AppErrorKind::UnavailableContent);
    }

    #[tokio::test]
    async fn a_listing_that_reaches_its_declared_count_ends_without_an_error() {
        // Two posts declared, two already read: an empty grid here is the end,
        // and the count is what makes that a supported statement.
        let resolver =
            resolver_with(serde_json::json!({ "creator": { "name": "郭虎" }, "posts": [] }));
        let mut creator = listed_creator();
        creator.post_count = Some(2);

        let page = resolver
            .list_posts(&creator, PageRequest::next("2", 2))
            .await
            .expect("the known end of a listing is not a failure");
        assert!(page.posts.is_empty());
        assert!(!page.has_more);
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn an_empty_grid_with_no_declared_count_ends_the_listing() {
        // Nothing to go on: earlier pages were read successfully, so the
        // conservative reading is that the listing ran out. No error is
        // invented, and no further page is offered.
        let resolver =
            resolver_with(serde_json::json!({ "creator": { "name": "郭虎" }, "posts": [] }));
        let mut creator = listed_creator();
        creator.post_count = None;

        let page = resolver
            .list_posts(&creator, PageRequest::next("1", 1))
            .await
            .unwrap();
        assert!(page.posts.is_empty());
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn a_page_reporting_its_own_service_error_never_reads_as_the_end_of_a_listing() {
        // The page's own banner says its listing request failed. Even where the
        // declared count would otherwise let us call this the end, the source
        // is telling us it does not know — so the listing did not end, it was
        // withheld.
        let resolver = resolver_with(serde_json::json!({
            "creator": { "name": "郭虎" },
            "posts": [],
            "pageError": "服务异常，重新刷新拉取数据"
        }));
        let mut creator = listed_creator();
        creator.post_count = Some(2);

        let err = resolver
            .list_posts(&creator, PageRequest::next("2", 2))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), AppErrorKind::UnavailableContent);
        // And the source's own words survive into the diagnostic.
        assert!(err
            .technical_detail()
            .unwrap_or_default()
            .contains("page_error=服务异常"));
    }

    #[tokio::test]
    async fn a_quiet_empty_grid_at_the_declared_count_still_ends_the_listing() {
        // The same request without that banner: now the count is the best
        // evidence there is, and it says the listing is finished.
        let resolver =
            resolver_with(serde_json::json!({ "creator": { "name": "郭虎" }, "posts": [] }));
        let mut creator = listed_creator();
        creator.post_count = Some(2);

        let page = resolver
            .list_posts(&creator, PageRequest::next("2", 2))
            .await
            .expect("nothing here says the source failed");
        assert!(page.posts.is_empty());
        assert!(!page.has_more);
    }

    #[tokio::test]
    async fn a_page_past_the_end_of_a_listing_ends_it_rather_than_failing() {
        // The grid holds four cards; page one returns them all, and asking for
        // more must terminate cleanly instead of reporting a refusal.
        let resolver = resolver_with(four_post_payload());
        let creator = listed_creator();

        let page = resolver
            .list_posts(&creator, PageRequest::next("4", 2))
            .await
            .unwrap();
        assert!(page.posts.is_empty());
        assert!(!page.has_more);
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn a_repeated_card_inside_a_page_is_only_listed_once() {
        let mut payload = profile_page_payload();
        let posts = payload["posts"].as_array_mut().unwrap();
        posts.push(posts[0].clone());

        let resolver = resolver_with(payload);
        let page = resolver
            .list_posts(&listed_creator(), PageRequest::first(18))
            .await
            .unwrap();
        assert_eq!(page.posts.len(), 2);
    }

    #[tokio::test]
    async fn a_listing_that_is_empty_at_first_is_taken_again_and_then_believed() {
        // The reported bug, exactly: the page's own listing request fails the
        // first time and the same button works when pressed again. That second
        // press is a retry, and this is the code making it for itself.
        let empty = serde_json::json!({ "creator": { "name": "郭虎" }, "posts": [] });
        let renderer = Arc::new(
            MockPageRenderer::returning(Vec::new()).with_payload_sequence(vec![
                empty.clone(),
                empty,
                profile_page_payload(),
            ]),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());

        let page = resolver
            .list_posts(&creator_with_declared_posts(50), PageRequest::first(18))
            .await
            .expect(
                "a later look must find the listing rather than the first empty one being believed",
            );

        assert!(
            !page.posts.is_empty(),
            "the posts from the third look are returned"
        );
        assert_eq!(
            renderer.calls(),
            3,
            "the page must be taken again while the grid is empty, up to the bound"
        );
    }

    #[tokio::test]
    async fn a_listing_that_stays_empty_stops_at_the_bound() {
        // The other half: a source that is genuinely refusing is asked the
        // bounded number of times and then taken at its word, rather than being
        // asked forever.
        let empty = serde_json::json!({ "creator": { "name": "郭虎" }, "posts": [] });
        let renderer = Arc::new(
            MockPageRenderer::returning(Vec::new()).with_payload_sequence(vec![
                empty.clone(),
                empty.clone(),
                empty,
            ]),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());

        let err = resolver
            .list_posts(&creator_with_declared_posts(50), PageRequest::first(18))
            .await
            .expect_err("an empty grid beyond the bound is a refusal");

        assert!(err.is_retryable());
        assert_eq!(
            renderer.calls(),
            LISTING_RENDER_ATTEMPTS,
            "the number of looks is bounded, not open-ended"
        );
    }

    /// A creator the source says has posts, without building one by hand.
    fn creator_with_declared_posts(count: u32) -> Creator {
        let mut creator = listed_creator();
        creator.post_count = Some(count);
        creator
    }

    /// A profile page with an empty grid that is asking for a sign-in.
    fn sign_in_walled_payload() -> serde_json::Value {
        let mut payload = profile_page_payload();
        payload["posts"] = serde_json::json!([]);
        payload["loginRequired"] = serde_json::json!(true);
        payload
    }

    #[tokio::test]
    async fn an_authenticated_listing_asks_for_the_saved_sign_in_profile() {
        use crate::network::renderer::BrowserProfile;

        let renderer = Arc::new(
            MockPageRenderer::returning(Vec::new()).with_payload(profile_page_payload()),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());

        resolver
            .list_posts(
                &creator_with_declared_posts(50),
                PageRequest::first(18).authenticated(),
            )
            .await
            .expect("an authenticated listing");

        assert_eq!(
            renderer.last_profile(),
            Some(BrowserProfile::Persistent),
            "a listing the user asked to read as themselves must use the profile the sign-in \
             went into"
        );
    }

    #[tokio::test]
    async fn an_anonymous_listing_stays_on_the_throwaway_profile() {
        use crate::network::renderer::BrowserProfile;

        let renderer = Arc::new(
            MockPageRenderer::returning(Vec::new()).with_payload(profile_page_payload()),
        );
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());

        resolver
            .list_posts(&creator_with_declared_posts(50), PageRequest::first(18))
            .await
            .expect("an anonymous listing");

        assert_eq!(
            renderer.last_profile(),
            Some(BrowserProfile::Ephemeral),
            "the default must stay anonymous — an account is the user's decision, not a \
             convenient default"
        );
    }

    #[tokio::test]
    async fn a_page_that_asks_for_a_sign_in_is_reported_as_that_and_not_as_an_empty_profile() {
        // The failure this rules out is the quiet one: an empty grid reported as
        // "this creator has no posts", when the truth is that Douyin asked a
        // question and nobody answered it.
        let resolver = resolver_with(sign_in_walled_payload());

        let err = resolver
            .list_posts(&creator_with_declared_posts(50), PageRequest::first(18))
            .await
            .expect_err("a sign-in wall is not an empty profile");

        assert_eq!(err.kind(), AppErrorKind::AuthenticationRequired);
        assert!(
            err.user_message().contains("Login"),
            "the message has to name the thing that fixes it: {}",
            err.user_message()
        );
        // And it must not be retried: asking again gives the same answer, and a
        // retry loop against a sign-in wall is exactly what must not happen.
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn an_empty_grid_without_a_sign_in_prompt_stays_a_withheld_listing() {
        // The other side of the same coin: the classification has to be
        // evidence from the page, not a guess from the grid being empty. With no
        // sign-in prompt anywhere, this is still the source withholding a
        // listing, and it must not be reported as an authentication problem the
        // user cannot do anything about.
        let mut payload = profile_page_payload();
        payload["posts"] = serde_json::json!([]);
        payload["loginRequired"] = serde_json::json!(false);
        let resolver = resolver_with(payload);

        let err = resolver
            .list_posts(&creator_with_declared_posts(50), PageRequest::first(18))
            .await
            .expect_err("an empty grid is not an empty profile");

        assert_eq!(
            err.kind(),
            AppErrorKind::UnavailableContent,
            "an empty grid with no sign-in prompt is the source withholding a listing, not a \
             sign-in problem"
        );
        assert!(
            err.is_retryable(),
            "and it stays retryable, because a half-built page looks the same from here"
        );
    }

    #[tokio::test]
    async fn a_page_that_lists_nothing_is_refused_not_reported_as_an_empty_profile() {
        // The platform declining to list a creator's posts is not the same
        // statement as the creator having none, and must not be presented as one.
        let resolver =
            resolver_with(serde_json::json!({ "creator": { "name": "郭虎" }, "posts": [] }));
        let creator = {
            let mut creator = listed_creator();
            creator.post_count = Some(50);
            creator
        };

        let err = resolver
            .list_posts(&creator, PageRequest::first(18))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), AppErrorKind::UnavailableContent);
        assert!(err.user_message().contains("did not return"));
        // Marked retryable, because an empty grid is also what a page that has
        // not finished building looks like, and treating the two as one is what
        // made the first "Load profile" press fail and the second succeed — the
        // second press was the retry the code declined to make for itself.
        assert!(
            err.is_retryable(),
            "an empty grid must be offered a second look, or a slow page is reported as a refusal"
        );
    }

    #[tokio::test]
    async fn a_creator_with_no_posts_reports_an_empty_listing() {
        // Zero is a real answer when the profile itself says zero.
        let resolver = resolver_with(
            serde_json::json!({ "creator": { "name": "x", "postCount": 0 }, "posts": [] }),
        );
        let creator = {
            let mut creator = listed_creator();
            creator.post_count = Some(0);
            creator
        };

        let page = resolver
            .list_posts(&creator, PageRequest::first(18))
            .await
            .unwrap();
        assert!(page.posts.is_empty());
        assert!(!page.has_more);
        assert_eq!(page.total_known, Some(0));
    }

    #[tokio::test]
    async fn listing_without_a_browser_is_a_typed_error_naming_the_remedy() {
        let resolver = DouyinResolver::new(Arc::new(MockHttpClient::new()));
        let err = resolver
            .list_posts(&listed_creator(), PageRequest::first(18))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), AppErrorKind::BrowserUnavailable);
        assert!(err.user_message().contains("Chrome"));
    }

    #[tokio::test]
    async fn a_malformed_cursor_is_rejected_rather_than_guessed_at() {
        let resolver = resolver_with(profile_page_payload());
        let err = resolver
            .list_posts(&listed_creator(), PageRequest::next("not-a-number", 18))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), AppErrorKind::Validation);
    }

    #[tokio::test]
    async fn browsing_never_starts_a_browser_when_the_page_cannot_be_read() {
        // The renderer is unavailable, so no browser process is launched and the
        // caller is told what to do about it.
        let renderer = Arc::new(MockPageRenderer::unavailable());
        let resolver =
            DouyinResolver::with_renderer(Arc::new(MockHttpClient::new()), renderer.clone());

        let _ = resolver
            .list_posts(&listed_creator(), PageRequest::first(18))
            .await;
        assert_eq!(renderer.calls(), 0);
    }

    /// Four cards, as a profile grid holds once it has loaded.
    fn four_post_payload() -> serde_json::Value {
        serde_json::json!({
            "creator": { "name": "郭虎", "postCount": 4 },
            "posts": [
                card("7091303675863321870", "video"),
                card("7091303675863321871", "note"),
                card("7091303675863321872", "video"),
                card("7091303675863321873", "video")
            ]
        })
    }

    fn card(id: &str, kind: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "kind": kind,
            "url": format!("https://www.douyin.com/{kind}/{id}"),
            "caption": format!("post {id}"),
            "thumbnailUrl": "https://p3.douyinpic.com/t.jpeg",
            "hasImage": true
        })
    }
}
