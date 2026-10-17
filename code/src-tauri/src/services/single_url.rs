//! Single URL workflow.
//!
//! This is the only place that knows the *sequence* of the Single URL pipeline:
//! validate → expand → classify → resolve post → normalize media → persist →
//! hand the selection to the download manager. Keeping it out of the command
//! layer means the whole flow is testable without a Tauri runtime, and keeping
//! it out of the resolver layer means the resolvers stay unaware of the
//! database and the download queue.
//!
//! ```text
//! analyze_url   : URL ─▶ SourceUrl ─▶ Post + MediaItem ─▶ SQLite
//! create_download: ids ─▶ stored media row ─▶ DownloadRequest ─▶ DownloadManager
//! ```
//!
//! # Why the frontend sends ids, not URLs
//!
//! `create_download` takes a post id and a media/variant id, and looks the
//! media URL up in the database. The frontend never supplies a URL to download
//! from, so a compromised renderer cannot point the transfer at an arbitrary
//! address, and TypeScript never has to know how Douyin encodes its media.

use serde::{Deserialize, Serialize};

use crate::errors::{AppError, AppResult};
use crate::models::{
    Creator, DownloadRequest, DownloadTask, Id, MediaItem, MediaType, MediaVariant, Platform, Post,
    TrackLayout,
};
use crate::resolvers::{ResolverRegistry, SourceUrl, SourceUrlKind};

use super::AppCore;

/// What a pasted link means to the application.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UrlClassification {
    /// Exactly what the user supplied.
    pub original: String,
    /// Normalized form after classification (and expansion, when it ran).
    pub canonical: String,
    pub platform: Platform,
    /// `profile`, `post`, or `short_link`.
    pub kind: String,
    /// Human-readable kind.
    pub label: String,
    /// Platform id, when the URL was specific enough to yield one.
    pub id: Option<String>,
    pub is_short_link: bool,
    /// Whether a network round-trip is still required to classify this link.
    pub requires_expansion: bool,
    /// Name of the resolver that would handle this link, when known.
    pub resolver: Option<String>,
}

/// The outcome of analyzing one URL.
///
/// `intent` is the field the UI branches on. A profile link is a *successful*
/// analysis with `intent = "profile"`, not a failure: the link was understood
/// correctly, it simply is not a downloadable post, and saying so plainly is
/// what keeps the Single URL screen from pretending a profile is a post.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalyzedUrl {
    pub classification: UrlClassification,
    /// `post` or `profile`.
    pub intent: String,
    pub post: Option<Post>,
    pub creator: Option<Creator>,
    /// Normalized media for a resolved post, empty otherwise.
    pub media: Vec<MediaItem>,
    /// How far resolution got: `resolved` or `classified_only`.
    pub resolution_stage: String,
    /// How a post was resolved: `http` or `browser`.
    ///
    /// Surfaced so the UI can say when a browser was used. Starting a browser is
    /// a bigger action than a request, and the user should be able to see that
    /// it happened rather than having to infer it.
    pub resolved_via: Option<String>,
    /// A plain-language note when the link is understood but not downloadable
    /// here — for example a profile link.
    pub notice: Option<String>,
}

/// A request to download one media variant of a resolved post.
///
/// Deliberately carries no URL: the media location is read from the database,
/// so the renderer cannot redirect a transfer.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateDownloadInput {
    pub post_id: Id,
    pub media_item_id: Id,
    /// Which rendition to fetch. Defaults to the item's preferred variant.
    #[serde(default)]
    pub variant_id: Option<Id>,
}

impl AppCore {
    /// Analyze a pasted URL and persist whatever was resolved.
    pub async fn analyze_url(&self, url: &str) -> AppResult<AnalyzedUrl> {
        tracing::info!(target: "single_url", "url analysis started");
        let result = self.analyze_url_inner(url).await;
        // A failure used to leave nothing behind at all. Every `?` on the way
        // down propagated the error and no line was written, so an analysis that
        // failed was invisible in the log — which made "how many of these
        // attempts failed, and why" a question the log could not answer. The
        // error the caller receives is unchanged; it is now also written down.
        if let Err(err) = &result {
            tracing::warn!(
                target: "single_url",
                kind = ?err.kind(),
                error = %err.message(),
                detail = err.technical_detail().unwrap_or("-"),
                "url analysis failed"
            );
        }
        result
    }

    async fn analyze_url_inner(&self, url: &str) -> AppResult<AnalyzedUrl> {
        let registry = self.resolvers();

        let source = registry.expand(url).await?;
        let classification = describe(registry, source.clone());

        match &source.kind {
            SourceUrlKind::Post { .. } => {
                tracing::info!(
                    target: "single_url",
                    post_id = source.id().unwrap_or("-"),
                    "post resolution started"
                );

                let post_resolver = registry.post_resolver_for(&source)?;

                // Hand the resolver the URL the platform actually issued — the
                // redirect's landing URL with its own parameters — rather than
                // the canonicalized form. Re-deriving a URL by hand discards a
                // share signature the platform generates and checks, and asking
                // for a URL the platform did not hand out is how a resolution
                // that could have worked quietly does not.
                let fetch_target = source
                    .resolved_url
                    .as_deref()
                    .unwrap_or(source.canonical.as_str());
                let mut post = post_resolver.resolve_post(fetch_target).await?;

                // The resolver attaches media with the post; the media resolver
                // is the authority on it, so it is always consulted.
                let media_resolver = registry.media_resolver_for(source.platform)?;
                let media = media_resolver.resolve_media(&post).await?;
                post.media_items = media.clone();

                // Persist, and then key the result on the *stored* post id.
                //
                // A post the application has seen before keeps its original row,
                // and with it its media and its download history; the id the
                // resolver generated names nothing. Returning that id is what
                // made Download fail with "analyze the link again" for a link
                // that had been analyzed before — the analysis succeeded, the
                // row was there, and the id handed to the next command was a
                // different one. It only showed up on a second analysis, which
                // is why the fixture tests, where every post is new, never saw
                // it.
                let (post_id, creator) = self.persist_resolved_post(&post)?;
                post.id = post_id.clone();
                for item in &mut post.media_items {
                    item.post_id = post_id.clone();
                }
                // The preview is the same media, under the stored id, so the
                // item a download is started from is the item that was stored.
                let media = post.media_items.clone();
                // Read before the post is moved into the result.
                let via = resolved_via(&post);

                tracing::info!(
                    target: "single_url",
                    post_id = %post.platform_post_id,
                    media_items = media.len(),
                    "url analysis completed"
                );

                Ok(AnalyzedUrl {
                    classification,
                    intent: "post".to_string(),
                    post: Some(post),
                    creator,
                    media,
                    resolution_stage: resolution_stage(&source),
                    resolved_via: via,
                    notice: None,
                })
            }
            SourceUrlKind::Profile { .. } => {
                let profile_resolver = registry.profile_resolver_for(&source)?;
                let creator = profile_resolver.resolve_profile(&source.canonical).await?;

                tracing::info!(target: "single_url", "link classified as a creator profile");
                Ok(AnalyzedUrl {
                    classification,
                    intent: "profile".to_string(),
                    post: None,
                    creator: Some(creator),
                    media: Vec::new(),
                    resolution_stage: "classified_only".to_string(),
                    resolved_via: None,
                    notice: Some(
                        "This is a creator profile link, not a single post. \
                         Open the Profile Downloader to browse the creator's posts."
                            .to_string(),
                    ),
                })
            }
            SourceUrlKind::ShortLink { .. } => Err(AppError::resolution(
                "short link was not expanded before analysis",
            )
            .with_user_message("This Douyin link could not be resolved.")),
        }
    }

    /// Create a download for one media variant of a resolved post.
    pub async fn create_download(&self, input: CreateDownloadInput) -> AppResult<DownloadTask> {
        let (post, item, variant) = self.resolve_selection(&input)?;

        let settings = self.settings.current();
        let extension = variant
            .extension
            .clone()
            .or_else(|| item.extension.clone())
            .unwrap_or_else(|| default_extension(item.media_type).to_string());

        let mut request = DownloadRequest::new(variant.source_url.clone(), item.media_type)
            .with_post(post.id.clone())
            .with_media(item.id.clone(), variant.id.clone())
            .with_extension(extension)
            .with_max_attempts(settings.retry_max_attempts);

        // A video whose source keeps its audio in a separate track is fetched as
        // both, so what lands on disk is the file the user expects rather than a
        // silent one. The resolver already established that both exist, so this
        // does not guess.
        if item.track_layout() == Some(TrackLayout::AudioVideo) {
            let audio = audio_track_source(&post).ok_or_else(|| {
                AppError::resolution("the post claimed an audio track that is not present")
                    .with_user_message("This post's audio track could not be found.")
                    .with_technical_detail(format!("post={}", post.id))
            })?;

            // Refused here, before a task exists, rather than after the video has
            // been downloaded: the tool that combines the two is not something a
            // retry will produce.
            if !self.muxer().is_available() {
                return Err(AppError::muxer_unavailable(
                    "this video's audio is a separate track and no FFmpeg was found",
                )
                .with_technical_detail(format!("muxer={}", self.muxer().name())));
            }

            request = request.with_audio_source(audio);
        }

        tracing::info!(
            target: "single_url",
            post_id = %post.id,
            media_item_id = %item.id,
            media_type = %item.media_type,
            with_audio = request.audio_source_url.is_some(),
            "download task creation requested"
        );
        self.downloads().enqueue(request).await
    }

    /// Refresh a failed task's media URLs before it is retried.
    ///
    /// A Douyin media URL is signed and short-lived, and when the signature has
    /// aged out *every* candidate for a track is refused together — which is
    /// what a 403 on a resource that worked minutes earlier looks like, and what
    /// a live run produced. Re-queueing that task as it stands repeats the
    /// refused URL and fails identically, which is exactly what "Retry" was
    /// doing: the only recovery the user had left was to analyse the link again,
    /// which makes a different task and a second history row for the same file.
    ///
    /// The post is re-resolved through the ordinary resolver — the one the
    /// Single URL screen uses — and the task is re-pointed at the refreshed
    /// rendition *of the same track*. It never substitutes another media item,
    /// because a fallback that quietly becomes a different video is worse than a
    /// failure that says so.
    ///
    /// Best effort on purpose. A source that is simply down cannot be refreshed,
    /// and the task is then retried exactly as it stood, so this can only ever
    /// improve a retry — never turn a retryable failure into a refusal. Returns
    /// whether anything was actually refreshed.
    pub async fn refresh_task_media(&self, task_id: &str) -> AppResult<bool> {
        let Some(task) = self.downloads().get(task_id).await? else {
            return Ok(false);
        };
        let Some(post_id) = task.post_id.clone() else {
            return Ok(false);
        };
        let load = |id: &str| {
            self.database()
                .with_conn(|conn| crate::database::repositories::posts::get(conn, id))
        };
        let Some(post) = load(&post_id)? else {
            return Ok(false);
        };

        // What the task was pointed at, so the refreshed rendition can be
        // matched to it rather than to whatever the resolver now ranks first.
        let previous_kind = post
            .media_items
            .iter()
            .flat_map(|item| item.variants.iter())
            .find(|variant| Some(&variant.id) == task.variant_id.as_ref())
            .map(|variant| variant.kind);

        // Re-resolving replaces this post's media rows, which is the point: the
        // URLs they held are the ones that were refused. A post that cannot be
        // resolved right now — the source is down, the link has stopped working
        // — leaves the task exactly as it stood, because this can only ever
        // improve a retry and must never be the reason one does not happen.
        if let Err(err) = self.resolve_post_media(&post).await {
            tracing::warn!(
                target: "single_url",
                task_id = %task.id,
                error = %err.message(),
                "the post could not be re-resolved; the task will be retried as it stands"
            );
            return Ok(false);
        }

        let Some(refreshed) = load(&post_id)? else {
            return Ok(false);
        };

        let named = task.media_item_id.as_deref().unwrap_or_default();
        let Some(item) = refreshed
            .media_items
            .iter()
            .find(|item| item.id == named)
            // The item the task named is usually gone — re-resolving assigns new
            // ids — so the track is matched by kind instead. Never by "some other
            // item", and never across media types.
            .or_else(|| {
                refreshed
                    .media_items
                    .iter()
                    .find(|item| item.media_type == task.media_type)
            })
        else {
            return Ok(false);
        };

        let Some(variant) = item
            .variants
            .iter()
            .find(|variant| Some(variant.kind) == previous_kind)
            .or_else(|| item.variants.iter().find(|variant| variant.is_original))
            .or_else(|| item.variants.first())
        else {
            return Ok(false);
        };

        let audio = if task.audio_source_url.is_some() {
            audio_track_source(&refreshed)
        } else {
            None
        };

        let mut updated = task.clone();
        updated.media_item_id = Some(item.id.clone());
        updated.variant_id = Some(variant.id.clone());
        updated.source_url = variant.source_url.clone();
        updated.audio_source_url = audio;
        if let Some(extension) = variant.extension.clone().or_else(|| item.extension.clone()) {
            updated.extension = Some(extension);
        }

        let changed = updated.source_url != task.source_url
            || updated.audio_source_url != task.audio_source_url;
        if changed {
            self.database().with_conn(|conn| {
                crate::database::repositories::downloads::update_media(conn, &updated)
            })?;
            tracing::info!(
                target: "single_url",
                task_id = %task.id,
                media_type = %item.media_type,
                "refreshed a failed task's media URLs before retrying it"
            );
        }
        Ok(changed)
    }

    /// Persist a resolved post with the media and creator the resolver attached.
    ///
    /// Returns the *stored* post id alongside the creator. The id is what the
    /// rest of the application must key on: a post that was already known keeps
    /// its original row — and therefore its media and its download history —
    /// rather than gaining a second identity for the same platform content.
    ///
    /// Shared with the profile workflow, so a post resolved in bulk is stored by
    /// exactly the same rules as one resolved from the Single URL screen.
    pub(crate) fn persist_resolved_post(&self, post: &Post) -> AppResult<(Id, Option<Creator>)> {
        let creator = post
            .metadata
            .get(crate::resolvers::RESOLVED_CREATOR_KEY)
            .and_then(|value| serde_json::from_value::<Creator>(value.clone()).ok());

        let db = self.database();
        let post = post.clone();

        db.with_conn(|conn| {
            let mut creator = creator;
            let mut post = post;

            if let Some(resolved) = creator.as_mut() {
                let creator_id = crate::database::repositories::creators::upsert(conn, resolved)?;
                resolved.id = creator_id.clone();
                post.creator_id = Some(creator_id);
            }

            let post_id = crate::database::repositories::posts::upsert(conn, &post)?;

            // Media rows are keyed to the *stored* post id, which is the
            // existing row's id when this post was already known.
            let mut media = post.media_items.clone();
            for item in &mut media {
                item.post_id = post_id.clone();
            }
            crate::database::repositories::media::replace_items_for_post(conn, &post_id, &media)?;

            Ok((post_id, creator))
        })
    }

    /// Load the post/media/variant a download request refers to.
    ///
    /// Everything is read from the database by id; a stale or forged id is a
    /// typed `NotFound` rather than an error deep inside the transfer.
    fn resolve_selection(
        &self,
        input: &CreateDownloadInput,
    ) -> AppResult<(Post, MediaItem, MediaVariant)> {
        let post = self
            .database()
            .with_conn(|conn| crate::database::repositories::posts::get(conn, &input.post_id))?
            .ok_or_else(|| {
                AppError::not_found("post", &input.post_id)
                    .with_user_message("Analyze the link again before downloading.")
            })?;

        let item = post
            .media_items
            .iter()
            .find(|item| item.id == input.media_item_id)
            .cloned()
            .ok_or_else(|| {
                AppError::not_found("media item", &input.media_item_id)
                    .with_user_message("That media is no longer part of this post.")
            })?;

        let variant = match &input.variant_id {
            Some(variant_id) => item
                .variants
                .iter()
                .find(|variant| &variant.id == variant_id)
                .cloned()
                .ok_or_else(|| {
                    AppError::not_found("media variant", variant_id)
                        .with_user_message("That media version is no longer available.")
                })?,
            None => item.preferred_variant(),
        };

        // Defence in depth: the URL came from the database, which was written
        // by the resolver, but a corrupt row must not become an arbitrary
        // transfer target.
        let scheme = variant
            .source_url
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(AppError::storage("refusing a non-http media URL")
                .with_user_message("This media resource cannot be downloaded.")
                .with_technical_detail(format!("scheme={scheme}")));
        }

        Ok((post, item, variant))
    }
}

/// Describe a classified URL for the UI.
pub fn describe(registry: &ResolverRegistry, source: SourceUrl) -> UrlClassification {
    let resolver = match &source.kind {
        SourceUrlKind::Profile { .. } => registry
            .profile_resolver_for(&source)
            .ok()
            .map(|resolver| resolver.name().to_string()),
        SourceUrlKind::Post { .. } => registry
            .post_resolver_for(&source)
            .ok()
            .map(|resolver| resolver.name().to_string()),
        SourceUrlKind::ShortLink { .. } => None,
    };

    UrlClassification {
        kind: source.kind.label().to_string(),
        label: kind_label(&source.kind).to_string(),
        id: source.id().map(str::to_string),
        canonical: source.canonical.clone(),
        platform: source.platform,
        is_short_link: source.is_short_link(),
        requires_expansion: source.is_short_link(),
        resolver,
        original: source.original,
    }
}

fn kind_label(kind: &SourceUrlKind) -> &'static str {
    match kind {
        SourceUrlKind::Profile { .. } => "Creator profile",
        SourceUrlKind::Post { .. } => "Single post",
        SourceUrlKind::ShortLink { .. } => "Short link (needs expansion)",
    }
}

/// Which path resolved a post, read from the marker the resolver attached.
///
/// `None` for a post that did not come from a resolver resolution (for example
/// one reconstructed from the database).
fn resolved_via(post: &Post) -> Option<String> {
    post.metadata
        .get("resolution")
        .and_then(|value| value.get("via"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// How far the analysis got, for the UI to label its result honestly.
fn resolution_stage(source: &SourceUrl) -> String {
    match source.kind {
        SourceUrlKind::Post { .. } => "resolved".to_string(),
        _ => "classified_only".to_string(),
    }
}

/// The source URL of a post's audio track, when it has one.
///
/// The track the item would download by default, so the file that gets combined
/// uses the same audio the user would have got by downloading it on its own.
fn audio_track_source(post: &Post) -> Option<String> {
    post.media_items
        .iter()
        .find(|item| item.media_type == MediaType::Audio)
        .map(|item| item.preferred_variant().source_url)
        .filter(|url| !url.trim().is_empty())
}

/// Fallback extension for a media type whose variant did not declare one.
fn default_extension(media_type: MediaType) -> &'static str {    match media_type {
        MediaType::Video => "mp4",
        MediaType::Image | MediaType::Carousel => "jpg",
        MediaType::Audio => "mp3",
        // A text post is saved as text, and its extension says so rather than
        // borrowing a binary one it would never produce.
        MediaType::Text => "txt",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppPaths;
    use crate::network::MockHttpClient;
    use std::sync::Arc;

    /// Boot the whole core over a mock transport, so the service can be driven
    /// end to end without the network.
    fn core_with(client: MockHttpClient) -> (tempfile::TempDir, AppCore) {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::with_data_dir(temp.path().to_path_buf());
        // No renderer and a mock muxer: these tests exercise the HTTP path and
        // the download decision, and injecting both keeps them from launching a
        // real browser or depending on FFmpeg being installed.
        let core = AppCore::bootstrap_with(
            paths,
            Some(Arc::new(client)),
            Some(Arc::new(crate::network::NoopRenderer)),
            Some(Arc::new(crate::media::MockMuxer::producing(true, true))),
        )
        .unwrap()
        .core;

        // This harness plans downloads, so the download directory has to be its
        // own. It was not, and that is not hypothetical: the preflight probe a
        // planned download writes landed in the machine's real
        // `Downloads\Douyin`, beside the user's own downloads.
        let download_dir = temp.path().join("downloads");
        std::fs::create_dir_all(&download_dir).expect("download dir");
        crate::test_isolation::redirect_downloads_to(&core, &download_dir, "the single-url harness");
        (temp, core)
    }

    #[test]
    fn the_single_url_harness_downloads_only_into_its_own_workspace() {
        // The test that would have caught it. This harness plans real downloads,
        // and before this it planned them into the machine's own download
        // folder — the preflight probe it writes was found there.
        let (temp, core) = core_with(MockHttpClient::new());
        let download_dir = std::path::PathBuf::from(core.settings().current().download_directory);

        assert!(
            download_dir.starts_with(temp.path()),
            "the harness is downloading into {download_dir:?}, which is not inside its own              workspace {:?}",
            temp.path()
        );
        crate::test_isolation::assert_isolated_destination("the single-url harness", &download_dir);
    }

    fn sample_page_client() -> MockHttpClient {
        MockHttpClient::new().with_body(
            "https://www.iesdouyin.com/share/video/7281234567890123456/",
            crate::resolvers::douyin::extract::tests::sample_share_page(),
        )
    }

    #[tokio::test]
    async fn a_post_url_resolves_to_a_persisted_post_with_media() {
        let (_temp, core) = core_with(sample_page_client());

        let analyzed = core
            .analyze_url("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap();

        assert_eq!(analyzed.intent, "post");
        assert_eq!(analyzed.resolution_stage, "resolved");
        let post = analyzed.post.as_ref().expect("post present");
        assert_eq!(post.platform_post_id, "7281234567890123456");
        assert!(!analyzed.media.is_empty());

        // It was persisted, with media, so a download can be created by id.
        let stored = core
            .database()
            .with_conn(|conn| crate::database::repositories::posts::get(conn, &post.id))
            .unwrap()
            .expect("post persisted");
        assert_eq!(stored.media_items.len(), analyzed.media.len());
    }

    #[tokio::test]
    async fn a_profile_url_is_understood_but_not_treated_as_a_post() {
        let (_temp, core) = core_with(MockHttpClient::new());

        let analyzed = core
            .analyze_url("https://www.douyin.com/user/MS4wLjABAAAAexample")
            .await
            .unwrap();

        assert_eq!(analyzed.intent, "profile");
        assert!(analyzed.post.is_none(), "a profile is not a downloadable post");
        assert!(analyzed.media.is_empty());
        assert!(analyzed.notice.as_deref().unwrap_or("").contains("Profile Downloader"));
    }

    #[tokio::test]
    async fn a_source_that_returns_no_post_data_is_reported_not_faked() {
        let (_temp, core) = core_with(MockHttpClient::new());

        let err = core
            .analyze_url("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Network);
    }

    #[tokio::test]
    async fn creating_a_download_uses_the_stored_media_url() {
        let (_temp, core) = core_with(sample_page_client());

        let analyzed = core
            .analyze_url("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap();
        let post = analyzed.post.clone().unwrap();
        let video = analyzed
            .media
            .iter()
            .find(|m| m.media_type == MediaType::Video)
            .unwrap();

        let task = core
            .create_download(CreateDownloadInput {
                post_id: post.id.clone(),
                media_item_id: video.id.clone(),
                variant_id: None,
            })
            .await
            .unwrap();

        assert_eq!(task.post_id.as_deref(), Some(post.id.as_str()));
        assert_eq!(task.extension.as_deref(), Some("mp4"));
        // The clean source is preferred, and it is a Douyin CDN URL.
        assert!(task.source_url.starts_with("https://"));
        assert!(!crate::resolvers::douyin::extract::is_watermarked(&task.source_url));
    }

    /// A task holding what a live run held after the signature aged out: a URL
    /// the source no longer serves, and media ids a later resolve replaced.
    fn make_stale(core: &AppCore, task: &DownloadTask) -> DownloadTask {
        let mut stale = task.clone();
        stale.source_url = "https://v3-cdn.douyin.com/aweme/v1/play/?video_id=expired".into();
        stale.media_item_id = Some("item-that-no-longer-exists".into());
        stale.variant_id = Some("variant-that-no-longer-exists".into());
        core.database()
            .with_conn(|conn| crate::database::repositories::downloads::update_media(conn, &stale))
            .unwrap();
        stale
    }

    #[tokio::test]
    async fn refreshing_re_points_a_stale_task_at_the_posts_current_media() {
        // The live failure this exists for: every candidate for one track came
        // back 403 together, because the signed URLs had aged out. Re-queueing
        // that task repeats the refused URL and fails identically, so the only
        // recovery the user had left was to analyse the link again — which makes
        // a second task and a second history row for the same file.
        let (_temp, core) = core_with(sample_page_client());

        let analyzed = core
            .analyze_url("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap();
        let post = analyzed.post.clone().unwrap();
        let video = analyzed
            .media
            .iter()
            .find(|item| item.media_type == MediaType::Video)
            .unwrap()
            .clone();

        let task = core
            .create_download(CreateDownloadInput {
                post_id: post.id.clone(),
                media_item_id: video.id.clone(),
                variant_id: None,
            })
            .await
            .unwrap();

        // What the source serves now, and what the stale task must be moved to.
        let fresh_url = task.source_url.clone();
        make_stale(&core, &task);

        assert!(
            core.refresh_task_media(&task.id).await.unwrap(),
            "the task's URLs must be refreshed"
        );

        let after = core.downloads().get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            after.source_url, fresh_url,
            "it must be pointed at what the post serves now"
        );
        assert_eq!(
            after.media_type, task.media_type,
            "a refresh must never become a different kind of media"
        );
        assert_eq!(
            after.post_id, task.post_id,
            "and never a different post"
        );
        assert_ne!(
            after.variant_id.as_deref(),
            Some("variant-that-no-longer-exists"),
            "the vanished variant must be replaced by a real one"
        );
        assert!(
            after.media_item_id.as_deref() != Some("item-that-no-longer-exists"),
            "and so must the vanished media item"
        );
    }

    #[tokio::test]
    async fn a_task_whose_post_cannot_be_read_is_left_exactly_as_it_stood() {
        // Best effort by contract: a refresh that cannot happen must not be the
        // reason a retry does not happen, and must not move the task anywhere.
        let (_temp, core) = core_with(sample_page_client());

        let analyzed = core
            .analyze_url("https://www.douyin.com/video/7281234567890123456")
            .await
            .unwrap();
        let post = analyzed.post.clone().unwrap();
        let video = analyzed
            .media
            .iter()
            .find(|item| item.media_type == MediaType::Video)
            .unwrap()
            .clone();

        let task = core
            .create_download(CreateDownloadInput {
                post_id: post.id.clone(),
                media_item_id: video.id.clone(),
                variant_id: None,
            })
            .await
            .unwrap();
        let stale = make_stale(&core, &task);

        // The post the task belongs to is no longer readable — the same thing
        // as a link that has stopped resolving.
        core.database()
            .with_conn(|conn| {
                conn.execute("DELETE FROM posts WHERE id = ?1", [post.id.as_str()])?;
                Ok(())
            })
            .unwrap();

        assert!(
            !core.refresh_task_media(&task.id).await.unwrap(),
            "a post that cannot be read is not a refresh"
        );

        let after = core.downloads().get(&task.id).await.unwrap().unwrap();
        assert_eq!(after.source_url, stale.source_url);
        assert_eq!(after.media_item_id, stale.media_item_id);
    }

    #[tokio::test]
    async fn analysis_hands_the_resolver_the_platforms_own_landing_url() {
        // Only the redirect's landing URL — parameters and all — serves a page.
        // Analysis can only succeed if the service forwards that URL rather
        // than the canonical form it derived from it.
        let landing = "https://www.iesdouyin.com/share/video/7281234567890123456/\
                       ?from_ssr=1&share_sign=abc123&region=US";
        let client = MockHttpClient::new()
            .with_redirect(
                crate::resolvers::fixture::FIXTURE_VIDEO_URL,
                landing,
            )
            .with_body(
                landing,
                crate::resolvers::douyin::extract::tests::sample_share_page(),
            );

        let (_temp, core) = core_with(client);
        let analyzed = core
            .analyze_url(crate::resolvers::fixture::FIXTURE_VIDEO_URL)
            .await
            .expect("the landing URL must be the one requested");

        assert_eq!(analyzed.intent, "post");
        // The link the user pasted is still what is shown back to them.
        assert_eq!(analyzed.classification.original, crate::resolvers::fixture::FIXTURE_VIDEO_URL);
        assert_eq!(
            analyzed.post.as_ref().map(|post| post.platform_post_id.as_str()),
            Some("7281234567890123456")
        );
        assert!(!analyzed.media.is_empty());
    }

    #[tokio::test]
    async fn creating_a_download_for_an_unknown_post_is_a_typed_not_found() {
        let (_temp, core) = core_with(MockHttpClient::new());
        let err = core
            .create_download(CreateDownloadInput {
                post_id: "nope".into(),
                media_item_id: "nope".into(),
                variant_id: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::NotFound);
    }
}
