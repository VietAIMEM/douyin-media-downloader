//! Profile workflow.
//!
//! This is the only place that knows the *sequence* of the profile pipeline:
//! validate → classify → resolve the creator → enumerate posts → persist →
//! report local download state → hand a selection to the existing download
//! pipeline.
//!
//! It sits between the commands and the resolvers for the same reason
//! [`super::single_url`] does: the orchestration is testable without a Tauri
//! runtime, and the resolvers stay unaware of the database and the queue.
//!
//! ```text
//! resolve_profile     : URL ─▶ SourceUrl ─▶ Creator ─▶ SQLite
//! list_profile_posts  : URL + cursor ─▶ PostPage ─▶ posts+creator ─▶ SQLite
//! submit_downloads    : ids ─▶ stored post ─▶ resolve media ─▶ DownloadRequest
//!                                                          ─▶ DownloadManager
//! ```
//!
//! # What this module deliberately does not do
//!
//! * **It does not download.** Every transfer is created through
//!   [`AppCore::create_download`], which is the Single URL screen's own path.
//!   There is one download engine, one naming policy, and one history.
//! * **It does not take URLs from the caller.** Submission is by stored post id;
//!   the media location is read from the database, which the resolver wrote.
//! * **It does not treat a failed post as a failed batch.** One unusable post
//!   never stops the others from being submitted.

use serde::{Deserialize, Serialize};

use crate::errors::{AppError, AppResult};
use crate::models::{
    ContentStatus, Creator, DownloadState, Id, MediaType, Post,
};
use crate::resolvers::{PageRequest, ResolverRegistry, SourceUrlKind};

use super::single_url::{describe, CreateDownloadInput, UrlClassification};
use super::AppCore;

/// Posts requested per page when the caller does not say.
pub const DEFAULT_PAGE_SIZE: u32 = 18;
/// Upper bound on a page, so a caller cannot ask for an unbounded listing.
pub const MAX_PAGE_SIZE: u32 = 60;

/// The creator behind a profile URL, plus how much of it was actually read.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileIdentity {
    pub creator: Creator,
    pub classification: UrlClassification,
    /// `resolved` when the page supplied the creator's details, `url_only` when
    /// only the identity could be established.
    pub resolution_stage: String,
    /// `http` or `browser`, when a resolution method ran.
    pub resolved_via: Option<String>,
    /// Plain-language explanation when only the identity is known.
    pub notice: Option<String>,
}

/// One post in a profile listing, with the state the user needs to see.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePostView {
    pub post: Post,
    /// Source availability folded together with local download state.
    pub status: ContentStatus,
    /// The local download state behind `status`, when there is one.
    pub download_state: Option<DownloadState>,
    /// The task currently working on this post, when one is.
    pub active_task_id: Option<Id>,
    /// Why the status reads the way it does, when the reason is not obvious.
    pub note: Option<String>,
}

/// One page of a creator's posts.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePostsPage {
    pub creator: Creator,
    pub posts: Vec<ProfilePostView>,
    /// Opaque cursor for the next page; `None` when the listing is exhausted.
    pub next_cursor: Option<String>,
    pub has_more: bool,
    /// The creator's own post count, when the source published one.
    pub total_known: Option<u32>,
    /// How many of this creator's posts are stored locally in total.
    pub loaded_total: usize,
}

/// What happened to one selected post.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDownloadOutcome {
    pub post_id: Id,
    pub platform_post_id: String,
    /// A readable name for the post, never empty.
    pub title: String,
    /// `taskCreated`, `alreadyDownloaded`, `alreadyDownloading`,
    /// `unsupported`, or `failed`.
    pub outcome: String,
    pub message: Option<String>,
    pub task_id: Option<Id>,
}

/// The result of submitting a selection.
///
/// The counts are deliberately separate, and every selected post lands in
/// exactly one of `tasksCreated` / `skipped` / `failed` / `unsupported`:
/// successfully *creating a task* is not the same as downloading media, and a
/// batch that partly succeeded must be reportable as such rather than as one
/// pass or one failure.
///
/// `resolved` is the other dimension — how many posts had usable media, whether
/// it came from the library or was resolved just now. It is not a bucket.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDownloadResult {
    /// Posts the user asked for.
    pub selected: usize,
    /// Posts whose media was obtained (already stored, or resolved now).
    pub resolved: usize,
    /// Media items queued.
    ///
    /// Items, not posts: a carousel queues one per picture, and a summary that
    /// counted the carousel as one would say "1 of 1" for fifteen downloads.
    pub tasks_created: usize,
    pub skipped: usize,
    pub failed: usize,
    /// Media items that resolved to nothing downloadable.
    pub unsupported: usize,
    /// One entry per selected post, each aggregating its own media.
    pub posts: Vec<ProfilePostResult>,
}

/// What happened to one selected post, and to each of its media items.
///
/// The post-level outcome is nested over the per-item ones rather than replacing
/// them, because the two questions are different and both are asked: "did my
/// carousel download?" and "which of its pictures failed?". A single flat bucket
/// per post could answer the first only by losing the second.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePostResult {
    pub post_id: Id,
    pub platform_post_id: String,
    pub title: String,
    /// `completed`, `queued`, `partial`, `failed` or `unsupported`.
    pub outcome: String,
    /// One entry per media item this post contributed.
    pub media: Vec<ProfileDownloadOutcome>,
}

/// A request to submit stored posts for download.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitProfileDownloadsInput {
    pub post_ids: Vec<Id>,
    /// The slides the user chose for a post, when they chose any.
    ///
    /// A post named here is submitted with exactly these items. A post left out
    /// keeps the default — every picture it has, or the single item a video
    /// post means — so a caller that has no per-slide UI is unaffected, and one
    /// that does only has to speak for the posts the user actually narrowed.
    #[serde(default)]
    pub selected_items: std::collections::HashMap<Id, Vec<Id>>,
}

impl AppCore {
    /// Resolve a profile URL into a creator, and persist it.
    ///
    /// A link that is understood but is not a profile is a `Validation` error
    /// naming what it actually was, so the screen can point the user at the
    /// screen that handles it instead of silently doing nothing.
    pub async fn resolve_profile(&self, url: &str) -> AppResult<ProfileIdentity> {
        let registry = self.resolvers();
        let source = registry.expand(url).await?;
        let classification = describe(registry, source.clone());

        if !source.is_profile() {
            return Err(not_a_profile(&source));
        }

        let resolver = registry.profile_resolver_for(&source)?;
        let creator = self.store_creator(&resolver.resolve_profile(&source.canonical).await?)?;
        let stage = resolution_stage(&creator);
        let via = resolved_via(&creator);

        tracing::info!(
            target: "profile",
            platform_id = %creator.platform_id,
            stage = %stage,
            "profile resolved"
        );

        Ok(ProfileIdentity {
            creator,
            classification,
            resolution_stage: stage.clone(),
            resolved_via: via,
            notice: (stage == "url_only").then(|| {
                "The creator's own details could not be read from the page, so this link's \
                 identity is shown. The profile can still be browsed."
                    .to_string()
            }),
        })
    }

    /// Read one page of a creator's posts, persist them, and report their state.
    ///
    /// `cursor` is the opaque token a previous page returned; `None` starts at
    /// the beginning. The page size is clamped here so a caller cannot ask for
    /// an unbounded listing.
    pub async fn list_profile_posts(
        &self,
        url: &str,
        cursor: Option<String>,
        limit: Option<u32>,
    ) -> AppResult<ProfilePostsPage> {
        let registry = self.resolvers();
        let source = registry.expand(url).await?;

        if !source.is_profile() {
            return Err(not_a_profile(&source));
        }

        let resolver = registry.profile_resolver_for(&source)?;
        // The creator is resolved first — and persisted — so every enumerated
        // post is attached to the same local creator row, whichever page it
        // arrived on.
        let resolved = resolver.resolve_profile(&source.canonical).await?;
        let creator = self.store_creator(&resolved)?;

        let limit = limit.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE);
        // Whether this listing is read as the signed-in visitor is a user
        // preference, read here — at the service layer, where settings live —
        // rather than guessed at in the resolver.
        let authenticated = self.settings().current().profile_login_enabled;
        let page = resolver
            .list_posts(
                &creator,
                PageRequest {
                    cursor,
                    limit,
                    authenticated,
                },
            )
            .await?;

        let posts = self.store_listing_posts(&creator, page.posts)?;
        let views = self.with_local_status(posts)?;
        let loaded_total = self.database().with_conn(|conn| {
            crate::database::repositories::posts::count_by_creator(conn, &creator.id)
        })? as usize;

        tracing::info!(
            target: "profile",
            platform_id = %creator.platform_id,
            returned = views.len(),
            has_more = page.has_more,
            "profile page loaded"
        );

        Ok(ProfilePostsPage {
            creator,
            posts: views,
            next_cursor: page.next_cursor,
            has_more: page.has_more,
            total_known: page.total_known,
            loaded_total,
        })
    }

    /// Submit a selection of stored posts to the existing download pipeline.
    ///
    /// Every post is handled independently: media is resolved through the
    /// resolver registry, the task is created through
    /// [`AppCore::create_download`], and the outcome is recorded per post. A
    /// post that cannot be used — unsupported content, a source that will not
    /// answer, a missing FFmpeg for a two-track video — fails on its own and
    /// leaves the rest of the batch alone.
    pub async fn submit_profile_downloads(
        &self,
        input: SubmitProfileDownloadsInput,
    ) -> AppResult<ProfileDownloadResult> {
        let ids = dedupe(input.post_ids);
        if ids.is_empty() {
            return Err(AppError::validation("no posts were selected")
                .with_user_message("Select at least one post to download."));
        }

        let mut result = ProfileDownloadResult {
            selected: ids.len(),
            resolved: 0,
            tasks_created: 0,
            skipped: 0,
            failed: 0,
            unsupported: 0,
            posts: Vec::with_capacity(ids.len()),
        };

        for post_id in ids {
            let chosen = input.selected_items.get(&post_id);
            let post_result = self
                .submit_one_post(&post_id, chosen.map(Vec::as_slice))
                .await;

            // Each *item* lands in exactly one bucket, so the summary accounts
            // for every download the batch set out to make rather than for the
            // posts it was asked about. A carousel that queued nine pictures and
            // failed on a tenth says both of those things.
            let mut obtained = false;
            for outcome in &post_result.media {
                match outcome.outcome.as_str() {
                    "taskCreated" => {
                        result.tasks_created += 1;
                        obtained = true;
                    }
                    // Passed over because the work was already done or under way
                    // — both of which mean the media is known.
                    "alreadyDownloaded" | "alreadyDownloading" => {
                        result.skipped += 1;
                        obtained = true;
                    }
                    "unsupported" => result.unsupported += 1,
                    _ => result.failed += 1,
                }
            }
            // A post counts as resolved when any of its media was obtained: the
            // request the user made about that post was met, whatever happened
            // to the rest of its pictures.
            if obtained {
                result.resolved += 1;
            }
            result.posts.push(post_result);
        }

        tracing::info!(
            target: "profile",
            selected = result.selected,
            created = result.tasks_created,
            skipped = result.skipped,
            unsupported = result.unsupported,
            failed = result.failed,
            "profile bulk submission completed"
        );

        Ok(result)
    }

    /// Handle one selected post, returning one result aggregating its media.
    ///
    /// Returning the result rather than propagating a failure is what makes the
    /// batch partial: the caller aggregates, and the user sees every reason.
    async fn submit_one_post(&self, post_id: &Id, selected: Option<&[Id]>) -> ProfilePostResult {
        let mut post = match self
            .database()
            .with_conn(|conn| crate::database::repositories::posts::get(conn, post_id))
        {
            Ok(Some(post)) => post,
            Ok(None) => {
                return ProfilePostResult::nothing(
                    post_id,
                    post_id,
                    "That post is no longer in the local library.",
                )
            }
            Err(err) => return ProfilePostResult::nothing(post_id, post_id, &err.user_message()),
        };

        // A post discovered by a listing carries no media: the listing says what
        // exists, not what it is made of. Resolving it now, through the same
        // resolver the Single URL screen uses, is what keeps one media pipeline
        // rather than two.
        if post.media_items.is_empty() {
            match self.resolve_post_media(&post).await {
                Ok(resolved_id) => match self
                    .database()
                    .with_conn(|conn| crate::database::repositories::posts::get(conn, &resolved_id))
                {
                    Ok(Some(fresh)) => post = fresh,
                    Ok(None) => {
                        return ProfilePostResult::nothing(
                            post_id,
                            &post.platform_post_id,
                            "The resolved post could not be read back.",
                        )
                    }
                    Err(err) => {
                        return ProfilePostResult::from_error(
                            post_id,
                            &post.platform_post_id,
                            &err,
                        )
                    }
                },
                Err(err) => {
                    return ProfilePostResult::from_error(
                        post_id,
                        &post.platform_post_id,
                        &err,
                    )
                }
            }
        }

        // Which items this post contributes.
        //
        // A choice the user made wins, and is read back through the post's own
        // items so the order is the post's order rather than the order a list of
        // checkboxes happened to be in — a slide's filename follows its place in
        // the post. An item they named that the post does not have is dropped
        // rather than invented.
        //
        // With no choice made: pictures are *all* of them, because a carousel is
        // fifteen downloads and not one, and anything else is the single item a
        // person means by "download this post".
        let wanted: Vec<Id> = match selected {
            // `Some`, including an empty one. "None chosen" is a choice — the
            // user emptied the post — and reading it as "no opinion, so take
            // everything" is the one interpretation a chooser must never have.
            Some(chosen) => post
                .media_items
                .iter()
                .filter(|item| chosen.contains(&item.id))
                .map(|item| item.id.clone())
                .collect(),
            _ if matches!(post.media_type, MediaType::Image | MediaType::Carousel) => post
                .media_items
                .iter()
                .filter(|item| item.media_type == MediaType::Image)
                .map(|item| item.id.clone())
                .collect(),
            _ => primary_item(&post)
                .map(|item| item.id.clone())
                .into_iter()
                .collect(),
        };

        if wanted.is_empty() {
            return ProfilePostResult {
                post_id: post.id.clone(),
                platform_post_id: post.platform_post_id.clone(),
                title: post.display_title(),
                outcome: "unsupported".into(),
                media: vec![failure(
                    &post.id,
                    &post.platform_post_id,
                    match selected {
                        // The post has pictures; the user asked for none of them.
                        // Saying "nothing downloadable" would be untrue, and
                        // downloading all of them would be disobedient.
                        Some(_) => "No pictures were chosen for this post.",
                        None => "This post does not contain downloadable media.",
                    },
                )],
            };
        }

        let mut media = Vec::with_capacity(wanted.len());
        for item_id in wanted {
            media.push(self.submit_one_item(&post, &item_id).await);
        }
        let outcome = aggregate_outcome(&media);
        tracing::info!(
            target: "profile",
            post_id = %post.id,
            items = media.len(),
            outcome = %outcome,
            "profile post submitted"
        );

        ProfilePostResult {
            post_id: post.id.clone(),
            platform_post_id: post.platform_post_id.clone(),
            title: post.display_title(),
            outcome,
            media,
        }
    }

    /// Handle one media item of a post, returning its outcome rather than an error.
    ///
    /// One picture of a carousel is a download in its own right: queued on its
    /// own, retried on its own, and reported on its own.
    async fn submit_one_item(&self, post: &Post, media_item_id: &Id) -> ProfileDownloadOutcome {
        let title = post.display_title();

        let Some(item) = post.media_items.iter().find(|item| &item.id == media_item_id) else {
            return failure(
                &post.id,
                &post.platform_post_id,
                "That item is no longer part of this post.",
            );
        };

        // Duplicate policy. An in-flight transfer is never doubled; a finished
        // one is respected while its file is still there, and re-fetched when
        // it is not (the alternative is a row claiming a file that is gone).
        let existing = match self.database().with_conn(|conn| {
            if let Some(active) =
                crate::database::repositories::downloads::active_for_media(conn, &item.id)?
            {
                return Ok(Existing::Active(active.id));
            }
            let completed =
                crate::database::repositories::downloads::completed_for_media(conn, &item.id)?;
            Ok(match completed {
                Some(task) => {
                    let present = task
                        .destination_path
                        .as_deref()
                        .map(|path| std::path::Path::new(path).is_file())
                        .unwrap_or(false);
                    if present {
                        Existing::Downloaded(task.id)
                    } else {
                        Existing::MissingFile
                    }
                }
                None => Existing::None,
            })
        }) {
            Ok(existing) => existing,
            Err(err) => return failure(&post.id, &post.platform_post_id, &err.user_message()),
        };

        match existing {
            Existing::Active(task_id) => ProfileDownloadOutcome {
                post_id: post.id.clone(),
                platform_post_id: post.platform_post_id.clone(),
                title,
                outcome: "alreadyDownloading".into(),
                message: Some("This post is already queued for download.".into()),
                task_id: Some(task_id),
            },
            Existing::Downloaded(task_id) => ProfileDownloadOutcome {
                post_id: post.id.clone(),
                platform_post_id: post.platform_post_id.clone(),
                title,
                outcome: "alreadyDownloaded".into(),
                message: Some("This post has already been downloaded.".into()),
                task_id: Some(task_id),
            },
            Existing::MissingFile | Existing::None => {
                // The task is created by the Single URL screen's own service, so
                // there is no second download path to keep in step.
                let outcome = self
                    .create_download(CreateDownloadInput {
                        post_id: post.id.clone(),
                        media_item_id: item.id.clone(),
                        variant_id: None,
                    })
                    .await;

                match outcome {
                    Ok(task) => ProfileDownloadOutcome {
                        post_id: post.id.clone(),
                        platform_post_id: post.platform_post_id.clone(),
                        title,
                        outcome: "taskCreated".into(),
                        message: matches!(existing, Existing::MissingFile).then(|| {
                            "The previously downloaded file is no longer on disk, so it was \
                             fetched again."
                                .to_string()
                        }),
                        task_id: Some(task.id),
                    },
                    Err(err) => ProfileDownloadOutcome {
                        post_id: post.id.clone(),
                        platform_post_id: post.platform_post_id.clone(),
                        title,
                        outcome: "failed".into(),
                        message: Some(err.user_message()),
                        task_id: None,
                    },
                }
            }
        }
    }

    /// Resolve a stored post's media through the resolver registry, store it,
    /// and return the post's stored id.
    ///
    /// This is the Single URL resolution path — post resolver, then media
    /// resolver — so a post discovered by a listing is resolved exactly as one
    /// pasted directly would be, including the browser fallback when the plain
    /// HTTP pages hold nothing.
    pub(super) async fn resolve_post_media(&self, post: &Post) -> AppResult<Id> {
        let registry: &ResolverRegistry = self.resolvers();
        let source = registry.parse(&post.url)?;
        if !source.is_post() {
            return Err(AppError::unsupported("the stored post URL is not a post link")
                .with_user_message("This item cannot be downloaded.")
                .with_technical_detail(format!("post={}", post.id)));
        }

        let post_resolver = registry.post_resolver_for(&source)?;
        let fetch_target = source
            .resolved_url
            .as_deref()
            .unwrap_or(source.canonical.as_str());
        let mut resolved = post_resolver.resolve_post(fetch_target).await?;

        let media_resolver = registry.media_resolver_for(source.platform)?;
        resolved.media_items = media_resolver.resolve_media(&resolved).await?;

        let merged = merge_resolution(post, resolved);
        let (stored_id, _) = self.persist_resolved_post(&merged)?;
        Ok(stored_id)
    }

    /// Upsert a creator and return it with its stored local id.
    ///
    /// The *stored* row is what comes back, not the one that was passed in — as
    /// it is for a listed post. The identity screen and the listing resolve the
    /// same profile through separate renders and read different amounts from
    /// them, so the row is the one place where what either of them learned
    /// survives; handing back only the caller's own copy would have the listing
    /// act on a creator whose post count looked absent when it is on file.
    fn store_creator(&self, creator: &Creator) -> AppResult<Creator> {
        let fallback = creator.clone();
        self.database().with_conn(|conn| {
            let id = crate::database::repositories::creators::upsert(conn, creator)?;
            Ok(
                crate::database::repositories::creators::get(conn, &id)?.unwrap_or_else(|| {
                    let mut creator = fallback.clone();
                    creator.id = id.clone();
                    creator
                }),
            )
        })
    }

    /// Persist listed posts against the creator, returning them with stored ids.
    fn store_listing_posts(&self, creator: &Creator, posts: Vec<Post>) -> AppResult<Vec<Post>> {
        if posts.is_empty() {
            return Ok(Vec::new());
        }

        let creator_id = creator.id.clone();
        self.database().with_conn(|conn| {
            let mut out = Vec::with_capacity(posts.len());
            for mut post in posts {
                post.creator_id = Some(creator_id.clone());
                let id = crate::database::repositories::posts::upsert_from_listing(conn, &post)?;
                post.id = id;
                // The stored row is the authority on what is known about the
                // post, including anything the listing did not restate.
                match crate::database::repositories::posts::get(conn, &post.id)? {
                    Some(stored) => out.push(stored),
                    None => out.push(post),
                }
            }
            Ok(out)
        })
    }

    /// Fold each post's local download state into the status the UI shows.
    fn with_local_status(&self, posts: Vec<Post>) -> AppResult<Vec<ProfilePostView>> {
        self.database().with_conn(|conn| {
            let mut out = Vec::with_capacity(posts.len());
            for post in posts {
                let latest =
                    crate::database::repositories::downloads::latest_for_post(conn, &post.id)?;
                let file_present = latest
                    .as_ref()
                    .and_then(|task| task.destination_path.as_deref())
                    .map(|path| std::path::Path::new(path).is_file())
                    .unwrap_or(false);

                let (status, note) = local_status(post.status, latest.as_ref(), file_present);
                let active_task_id = latest
                    .as_ref()
                    .filter(|task| !task.is_terminal())
                    .map(|task| task.id.clone());

                out.push(ProfilePostView {
                    status,
                    download_state: latest.as_ref().map(|task| task.state),
                    active_task_id,
                    note,
                    post,
                });
            }
            Ok(out)
        })
    }
}

/// What the database already knows about a post's download.
enum Existing {
    /// A transfer is queued, running, or paused.
    Active(Id),
    /// Finished, and the file it produced is still on disk.
    Downloaded(Id),
    /// Finished, but the file it produced is gone.
    MissingFile,
    None,
}

/// Combine what a profile listing knew about a post with what resolving it found.
///
/// Resolution *enriches* the post the listing discovered; it does not replace it
/// with a thinner one.
///
/// The platform identity is always the listing's: it is what the post was
/// selected by and what the stored row is keyed on. Everything else is taken
/// from the resolution only when the resolution actually supplied it — a post
/// resolved through the browser is read for its media, and the page it is read
/// from carries no author, so letting that absence overwrite the association
/// would detach the post from the creator the listing already knew, losing both
/// the `{creator}` folder and the creator recorded in history.
fn merge_resolution(listing: &Post, resolved: Post) -> Post {
    let mut merged = resolved;
    merged.platform_post_id = listing.platform_post_id.clone();
    merged.creator_id = merged.creator_id.or_else(|| listing.creator_id.clone());
    merged.creator_handle = merged
        .creator_handle
        .or_else(|| listing.creator_handle.clone());
    merged.caption = merged.caption.or_else(|| listing.caption.clone());
    merged.thumbnail_url = merged
        .thumbnail_url
        .or_else(|| listing.thumbnail_url.clone());
    merged.published_at = merged.published_at.or(listing.published_at);
    merged.canonical_url = merged
        .canonical_url
        .or_else(|| listing.canonical_url.clone());
    merged
}

/// The item a download should fetch for a post.
///
/// A video is what the user means by "download this post"; an image post
/// resolves to its first image. Audio-only items are the fallback rather than
/// the preference, because a video post that happens to expose a separate audio
/// track must not be downloaded as sound alone.
impl ProfilePostResult {
    /// A post that could not be handled at all, with the reason as its one item.
    fn nothing(post_id: &str, platform_post_id: &str, message: &str) -> Self {
        Self::with_outcome(post_id, platform_post_id, "failed", message)
    }

    /// The same, for a failure whose *kind* is known.
    ///
    /// A post the resolver refused because it holds nothing downloadable is not
    /// a failed download — there was never anything to download — and the
    /// summary has always told those apart. Collapsing them would report a
    /// text-only post as an error.
    fn from_error(post_id: &str, platform_post_id: &str, err: &AppError) -> Self {
        let outcome = match err.kind() {
            crate::errors::AppErrorKind::UnsupportedContent => "unsupported",
            _ => "failed",
        };
        Self::with_outcome(post_id, platform_post_id, outcome, &err.user_message())
    }

    fn with_outcome(
        post_id: &str,
        platform_post_id: &str,
        outcome: &str,
        message: &str,
    ) -> Self {
        Self {
            post_id: post_id.to_string(),
            platform_post_id: platform_post_id.to_string(),
            title: post_id.to_string(),
            outcome: outcome.to_string(),
            // The post's own verdict and its single item's have to agree: the
            // row follows the aggregate, and the list under it follows the item.
            media: vec![ProfileDownloadOutcome {
                post_id: post_id.to_string(),
                platform_post_id: platform_post_id.to_string(),
                title: platform_post_id.to_string(),
                outcome: outcome.to_string(),
                message: Some(message.to_string()),
                task_id: None,
            }],
        }
    }
}

/// What a post amounts to, given what happened to each of its items.
///
/// The three questions asked after a batch are "did it work", "is it still
/// going" and "what went wrong", so the answer names those rather than rounding
/// a carousel that half-succeeded up to a pass or down to a failure. Work that
/// has been queued is not a result — a carousel whose fifteen tasks are all
/// waiting is not "completed", and saying so would be a lie the Downloads screen
/// contradicts a second later.
fn aggregate_outcome(media: &[ProfileDownloadOutcome]) -> String {
    if media.is_empty() {
        return "unsupported".into();
    }
    let count = |wanted: &str| media.iter().filter(|outcome| outcome.outcome == wanted).count();
    let queued = count("taskCreated");
    let working = count("alreadyDownloading");
    let done = count("alreadyDownloaded");
    let bad = media.len() - queued - working - done;

    if bad == 0 && queued == 0 && working == 0 {
        return "completed".into();
    }
    if bad == 0 {
        return "queued".into();
    }
    if queued + working + done == 0 {
        return "failed".into();
    }
    "partial".into()
}

fn primary_item(post: &Post) -> Option<&crate::models::MediaItem> {
    post.media_items
        .iter()
        .find(|item| item.media_type == MediaType::Video)
        .or_else(|| {
            post.media_items
                .iter()
                .find(|item| item.media_type != MediaType::Audio)
        })
        .or_else(|| post.media_items.first())
}

/// The status a listing should show for a post.
///
/// Pure, so the mapping is testable without a database or a filesystem, and
/// stated once rather than re-derived at every call site.
///
/// The rules it encodes:
///
/// * what the *source* says about availability outranks anything local — a post
///   that is unavailable is not made available by having been downloaded once;
/// * a completed download only counts as downloaded while its file is on disk,
///   because a row is not a file;
/// * a cancelled attempt leaves the post as it was, since cancelling says
///   nothing about the content.
fn local_status(
    post_status: ContentStatus,
    latest: Option<&crate::models::DownloadTask>,
    file_present: bool,
) -> (ContentStatus, Option<String>) {
    // This screen still asks the filesystem one yes-or-no question, so a file it
    // cannot see is taken as missing rather than as unknown. The Library makes
    // the three-way distinction; this one does not yet, and leaving its answer
    // two-valued here is deliberate: it changes nothing about what this screen
    // shows today, and widening it is a separate change with its own evidence to
    // gather.
    let presence = if file_present {
        crate::storage::presence::Presence::Present
    } else {
        crate::storage::presence::Presence::Missing
    };

    // Delegated rather than repeated: the Profile Downloader and the Library
    // answer the same question about the same post, and two copies of this rule
    // would eventually disagree about whether a post counts as downloaded.
    super::library::reconcile_status(post_status, latest.map(|task| task.state), presence)
}

/// Reject a link that is not a profile, saying what it actually is.
fn not_a_profile(source: &crate::resolvers::SourceUrl) -> AppError {
    let (message, user_message) = match source.kind {
        SourceUrlKind::Post { .. } => (
            "the link is a post, not a profile",
            "This is a single post link. Open the Single URL screen to download it.",
        ),
        SourceUrlKind::ShortLink { .. } => (
            "the short link was not expanded before classification",
            "This Douyin link could not be resolved.",
        ),
        SourceUrlKind::Profile { .. } => ("unreachable", "This is not a profile link."),
    };
    AppError::validation(message)
        .with_user_message(user_message)
        .with_technical_detail(format!("kind={}", source.kind.label()))
}

/// The creator's resolution stage, as recorded by the resolver.
fn resolution_stage(creator: &Creator) -> String {
    creator
        .metadata
        .get("resolution")
        .and_then(|value| value.get("stage"))
        .and_then(|value| value.as_str())
        .unwrap_or("resolved")
        .to_string()
}

/// Which path resolved the creator, when one did.
fn resolved_via(creator: &Creator) -> Option<String> {
    creator
        .metadata
        .get("resolution")
        .and_then(|value| value.get("via"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// Distinct ids, in the order they were first seen.
fn dedupe(ids: Vec<Id>) -> Vec<Id> {
    let mut seen = std::collections::HashSet::new();
    ids.into_iter()
        .filter(|id| !id.trim().is_empty() && seen.insert(id.clone()))
        .collect()
}

fn failure(post_id: &str, platform_post_id: &str, message: &str) -> ProfileDownloadOutcome {
    ProfileDownloadOutcome {
        post_id: post_id.to_string(),
        platform_post_id: platform_post_id.to_string(),
        title: platform_post_id.to_string(),
        outcome: "failed".into(),
        message: Some(message.to_string()),
        task_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::common::now;
    use crate::models::{DownloadRequest, DownloadTask};

    fn task(state: DownloadState) -> DownloadTask {
        let mut task = DownloadTask::new(&DownloadRequest::new("https://cdn/v.mp4", MediaType::Video));
        task.state = state;
        task.created_at = Some(now());
        task
    }

    #[test]
    fn a_new_post_with_no_history_stays_new() {
        let (status, note) = local_status(ContentStatus::New, None, false);
        assert_eq!(status, ContentStatus::New);
        assert!(note.is_none());
    }

    #[test]
    fn an_in_flight_task_reads_as_downloading() {
        for state in [
            DownloadState::Queued,
            DownloadState::Downloading,
            DownloadState::Paused,
        ] {
            let (status, _) = local_status(ContentStatus::New, Some(&task(state)), false);
            assert_eq!(status, ContentStatus::Downloading, "for {state}");
        }
    }

    #[test]
    fn a_completed_download_counts_only_while_its_file_is_there() {
        let completed = task(DownloadState::Completed);
        let (status, _) = local_status(ContentStatus::New, Some(&completed), true);
        assert_eq!(status, ContentStatus::Downloaded);

        // The row exists but the file does not: that is not a download.
        let (status, note) = local_status(ContentStatus::New, Some(&completed), false);
        assert_eq!(status, ContentStatus::Ready);
        assert!(note.unwrap().contains("no longer on disk"));
    }

    #[test]
    fn a_failed_attempt_is_reported_and_retryable() {
        let (status, note) = local_status(ContentStatus::New, Some(&task(DownloadState::Failed)), false);
        assert_eq!(status, ContentStatus::Failed);
        assert!(note.unwrap().contains("retried"));
    }

    #[test]
    fn a_cancelled_attempt_leaves_the_content_status_alone() {
        let (status, note) = local_status(ContentStatus::New, Some(&task(DownloadState::Cancelled)), false);
        assert_eq!(status, ContentStatus::New);
        assert!(note.unwrap().contains("cancelled"));
    }

    #[test]
    fn source_unavailability_outranks_local_history() {
        for source in [
            ContentStatus::Unavailable,
            ContentStatus::Private,
            ContentStatus::Unsupported,
        ] {
            let (status, _) = local_status(source, Some(&task(DownloadState::Completed)), true);
            assert_eq!(status, source, "a local download must not resurrect {source}");
        }
    }

    #[test]
    fn a_posts_outcome_says_which_of_its_pictures_worked() {
        // The three questions asked after a batch are "did it work", "is it
        // still going" and "what went wrong", and a carousel answers all three
        // at once. Rounding that to a pass or a failure is what the nesting
        // exists to avoid.
        let item = |outcome: &str| ProfileDownloadOutcome {
            post_id: "p".into(),
            platform_post_id: "1".into(),
            title: "t".into(),
            outcome: outcome.into(),
            message: None,
            task_id: None,
        };

        assert_eq!(aggregate_outcome(&[item("alreadyDownloaded")]), "completed");
        // Queued work is not finished work, and saying "completed" for a
        // carousel whose pictures are all still waiting is a claim the Downloads
        // screen contradicts a second later.
        assert_eq!(
            aggregate_outcome(&[item("taskCreated"), item("taskCreated")]),
            "queued"
        );
        assert_eq!(
            aggregate_outcome(&[item("alreadyDownloaded"), item("taskCreated")]),
            "queued"
        );
        // One picture lost and another kept is neither of the two.
        assert_eq!(
            aggregate_outcome(&[item("alreadyDownloaded"), item("failed")]),
            "partial"
        );
        assert_eq!(aggregate_outcome(&[item("failed")]), "failed");
        // A post with nothing downloadable is not a failed download.
        assert_eq!(aggregate_outcome(&[item("unsupported")]), "failed");
        assert_eq!(aggregate_outcome(&[]), "unsupported");
    }

    #[test]
    fn primary_item_prefers_video_then_non_audio() {
        use crate::models::MediaItem;

        let mut post = Post::new(crate::models::Platform::Douyin, "1", MediaType::Video, "u");
        post.media_items = vec![
            MediaItem::new(post.id.clone(), MediaType::Audio, "https://cdn/a.mp3"),
            MediaItem::new(post.id.clone(), MediaType::Video, "https://cdn/v.mp4"),
        ];
        assert_eq!(primary_item(&post).unwrap().media_type, MediaType::Video);

        // A video-less post falls back to its images rather than its audio.
        let mut images = Post::new(crate::models::Platform::Douyin, "2", MediaType::Carousel, "u");
        images.media_items = vec![
            MediaItem::new(images.id.clone(), MediaType::Audio, "https://cdn/a.mp3"),
            MediaItem::new(images.id.clone(), MediaType::Image, "https://cdn/i.jpg"),
        ];
        assert_eq!(primary_item(&images).unwrap().media_type, MediaType::Image);

        let empty = Post::new(crate::models::Platform::Douyin, "3", MediaType::Video, "u");
        assert!(primary_item(&empty).is_none());
    }

    #[test]
    fn selection_ids_are_deduplicated_and_blank_ones_dropped() {
        let ids = dedupe(vec![
            "a".into(),
            "b".into(),
            "a".into(),
            "  ".into(),
        ]);
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn resolution_enriches_a_listed_post_instead_of_replacing_it() {
        use crate::models::{MediaItem, Platform};

        let mut listing = Post::new(Platform::Douyin, "7091303675863321870", MediaType::Video, "https://d/v");
        listing.creator_id = Some("creator-1".into());
        listing.creator_handle = Some("@42299195146".into());
        listing.caption = Some("第一条".into());
        listing.thumbnail_url = Some("https://img/a.jpg".into());

        // A post resolved through the browser: media, and nothing else. This is
        // exactly the shape the live path produces.
        let mut resolved = Post::new(Platform::Douyin, "0", MediaType::Video, "https://d/v");
        resolved.caption = None;
        resolved.media_items = vec![MediaItem::new("x", MediaType::Video, "https://cdn/v.mp4")];

        let merged = merge_resolution(&listing, resolved);

        assert_eq!(merged.platform_post_id, "7091303675863321870");
        assert_eq!(
            merged.creator_id.as_deref(),
            Some("creator-1"),
            "a resolution that names no creator must not detach the post from the one the listing knew"
        );
        assert_eq!(merged.creator_handle.as_deref(), Some("@42299195146"));
        assert_eq!(merged.caption.as_deref(), Some("第一条"));
        assert_eq!(merged.thumbnail_url.as_deref(), Some("https://img/a.jpg"));
        assert_eq!(merged.media_items.len(), 1, "the media is the resolution's");
    }

    #[test]
    fn a_resolution_that_knows_more_wins() {
        use crate::models::Platform;

        let mut listing = Post::new(Platform::Douyin, "7091303675863321870", MediaType::Video, "https://d/v");
        listing.creator_id = Some("from-listing".into());
        listing.caption = Some("listing caption".into());

        let mut resolved = Post::new(Platform::Douyin, "7091303675863321870", MediaType::Video, "https://d/v");
        resolved.creator_id = Some("from-resolution".into());
        resolved.caption = Some("resolved caption".into());

        let merged = merge_resolution(&listing, resolved);
        assert_eq!(merged.creator_id.as_deref(), Some("from-resolution"));
        assert_eq!(merged.caption.as_deref(), Some("resolved caption"));
    }

    #[test]
    fn a_post_link_is_rejected_with_guidance_to_the_other_screen() {
        let source = crate::resolvers::classify("https://www.douyin.com/video/7281234567890123456")
            .unwrap();
        let err = not_a_profile(&source);
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Validation);
        assert!(err.user_message().contains("Single URL"));
    }
}
