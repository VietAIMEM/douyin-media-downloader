//! An in-memory resolver backed by fixed fixtures.
//!
//! This is a *real* implementation of the three resolver traits — not a stub
//! that panics — so it can be exercised by tests and, if desired, registered as
//! an offline/demo source. It exists because Phase 02/03 own the live Douyin
//! request flows, but the rest of the application (UI, database, downloads)
//! must be developable and testable before those land.
//!
//! Fixtures are built from the two URLs the project ships for validation:
//! * `https://v.douyin.com/SMt7NsUPaGo/` (profile)
//! * `https://v.douyin.com/7yyhouNSzvA/` (video)

use async_trait::async_trait;

use crate::errors::{AppError, AppResult};
use crate::models::{
    ContentStatus, Creator, MediaItem, MediaType, Platform, Post, VariantKind,
};

use super::{MediaResolver, PageRequest, PostPage, PostResolver, ProfileResolver};

/// The profile fixture URL shipped with the project.
pub const FIXTURE_PROFILE_URL: &str = "https://v.douyin.com/SMt7NsUPaGo/";
/// The content fixture URL shipped with the project.
pub const FIXTURE_VIDEO_URL: &str = "https://v.douyin.com/7yyhouNSzvA/";

/// Resolver returning deterministic data for a fixed set of URLs.
pub struct FixtureResolver {
    creators: Vec<Creator>,
    posts: Vec<Post>,
}

impl FixtureResolver {
    /// Build the standard fixture set.
    pub fn standard() -> Self {
        let mut creator = Creator::new(
            Platform::Douyin,
            "fixture-creator",
            "Fixture Creator",
            FIXTURE_PROFILE_URL,
        );
        creator.handle = Some("@fixture".into());
        creator.description = Some("Offline fixture data for architecture validation.".into());
        creator.post_count = Some(2);

        let creator_id = creator.id.clone();

        // A video post with two variants: an original/clean source and a
        // standard transcode. Demonstrates variant preference end-to-end.
        let mut video = Post::new(
            Platform::Douyin,
            "fixture-video-0001",
            MediaType::Video,
            FIXTURE_VIDEO_URL,
        );
        video.creator_id = Some(creator_id.clone());
        video.creator_handle = creator.handle.clone();
        video.canonical_url = Some("https://www.douyin.com/video/fixture-video-0001".into());
        video.caption = Some("Fixture video".into());
        video.status = ContentStatus::Ready;
        let mut media = MediaItem::new(video.id.clone(), MediaType::Video, "https://cdn.example/standard.mp4");
        media.mime_type = Some("video/mp4".into());
        media.extension = Some("mp4".into());
        media.width = Some(720);
        media.height = Some(1280);
        media.duration_seconds = Some(15.0);
        let original = {
            let mut v = crate::models::MediaVariant::new(
                media.id.clone(),
                VariantKind::Original,
                "Original",
                "https://cdn.example/original.mp4",
            );
            v.mime_type = Some("video/mp4".into());
            v.extension = Some("mp4".into());
            v.width = Some(1080);
            v.height = Some(1920);
            v
        };
        let standard = crate::models::MediaVariant::new(
            media.id.clone(),
            VariantKind::Standard,
            "720p",
            "https://cdn.example/standard.mp4",
        );
        media.variants = vec![original, standard];
        video.media_items = vec![media];
        video.media_items[0].post_id = video.id.clone();

        // A carousel post to cover a second media type.
        let mut carousel = Post::new(
            Platform::Douyin,
            "fixture-note-0002",
            MediaType::Carousel,
            "https://www.douyin.com/note/fixture-note-0002",
        );
        carousel.creator_id = Some(creator_id);
        carousel.creator_handle = creator.handle.clone();
        carousel.caption = Some("Fixture carousel".into());
        carousel.status = ContentStatus::Ready;
        let mut images = Vec::new();
        for index in 0..3 {
            let mut image = MediaItem::new(
                carousel.id.clone(),
                MediaType::Image,
                format!("https://cdn.example/image-{index}.jpg"),
            );
            image.extension = Some("jpg".into());
            image.mime_type = Some("image/jpeg".into());
            image.metadata.insert("index".into(), serde_json::json!(index));
            images.push(image);
        }
        carousel.media_items = images;

        Self {
            creators: vec![creator],
            posts: vec![video, carousel],
        }
    }

    /// Whether this resolver owns `url`.
    pub fn supports(&self, url: &str) -> bool {
        let trimmed = url.trim();
        self.posts.iter().any(|post| {
            post.url == trimmed
                || post.canonical_url.as_deref() == Some(trimmed)
        }) || self
            .creators
            .iter()
            .any(|creator| creator.profile_url == trimmed)
    }

    /// All fixture posts.
    pub fn posts(&self) -> &[Post] {
        &self.posts
    }

    /// All fixture creators.
    pub fn creators(&self) -> &[Creator] {
        &self.creators
    }

    fn find_post(&self, url: &str) -> Option<&Post> {
        let trimmed = url.trim();
        self.posts.iter().find(|post| {
            post.url == trimmed || post.canonical_url.as_deref() == Some(trimmed)
        })
    }
}

impl Default for FixtureResolver {
    fn default() -> Self {
        Self::standard()
    }
}

#[async_trait]
impl ProfileResolver for FixtureResolver {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn platform(&self) -> Platform {
        Platform::Douyin
    }

    fn supports(&self, url: &str) -> bool {
        FixtureResolver::supports(self, url)
    }

    async fn resolve_profile(&self, url: &str) -> AppResult<Creator> {
        let trimmed = url.trim();
        if let Some(creator) = self.creators.iter().find(|c| c.profile_url == trimmed) {
            return Ok(creator.clone());
        }
        // A post URL implies its creator.
        if let Some(post) = self.find_post(trimmed) {
            if let Some(creator) = post
                .creator_id
                .as_deref()
                .and_then(|id| self.creators.iter().find(|c| c.id == id))
            {
                return Ok(creator.clone());
            }
        }
        Err(AppError::not_found("fixture creator", trimmed)
            .with_user_message("No fixture data is registered for that link."))
    }

    async fn list_posts(&self, creator: &Creator, page: PageRequest) -> AppResult<PostPage> {
        let all: Vec<Post> = self
            .posts
            .iter()
            .filter(|post| post.creator_id.as_deref() == Some(creator.id.as_str()))
            .cloned()
            .collect();

        // Cursor is a plain offset encoded as a string; this exercises the
        // pagination contract without pretending to match Douyin's cursors.
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
        let slice: Vec<Post> = all.iter().skip(offset).take(limit).cloned().collect();
        let consumed = offset + slice.len();
        let has_more = consumed < all.len();

        Ok(PostPage {
            posts: slice,
            next_cursor: has_more.then(|| consumed.to_string()),
            has_more,
            total_known: Some(all.len() as u32),
        })
    }
}

#[async_trait]
impl PostResolver for FixtureResolver {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn platform(&self) -> Platform {
        Platform::Douyin
    }

    fn supports(&self, url: &str) -> bool {
        FixtureResolver::supports(self, url)
    }

    async fn resolve_post(&self, url: &str) -> AppResult<Post> {
        self.find_post(url)
            .cloned()
            .ok_or_else(|| {
                AppError::not_found("fixture post", url.trim())
                    .with_user_message("No fixture data is registered for that link.")
            })
    }
}

#[async_trait]
impl MediaResolver for FixtureResolver {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn platform(&self) -> Platform {
        Platform::Douyin
    }

    async fn resolve_media(&self, post: &Post) -> AppResult<Vec<MediaItem>> {
        if post.media_items.is_empty() {
            // Fall back to the stored fixture for this post, if any.
            if let Some(found) = self
                .posts
                .iter()
                .find(|p| p.platform_post_id == post.platform_post_id)
            {
                return Ok(found.media_items.clone());
            }
            return Err(AppError::not_found("fixture media", &post.platform_post_id));
        }
        Ok(post.media_items.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_the_shipped_fixture_urls() {
        let resolver = FixtureResolver::standard();

        let post = resolver.resolve_post(FIXTURE_VIDEO_URL).await.unwrap();
        assert_eq!(post.media_type, MediaType::Video);
        assert_eq!(post.media_items.len(), 1);

        let creator = resolver.resolve_profile(FIXTURE_PROFILE_URL).await.unwrap();
        assert_eq!(creator.display_name, "Fixture Creator");
    }

    #[tokio::test]
    async fn media_resolution_exposes_an_original_variant() {
        let resolver = FixtureResolver::standard();
        let post = resolver.resolve_post(FIXTURE_VIDEO_URL).await.unwrap();
        let media = resolver.resolve_media(&post).await.unwrap();
        let preferred = media[0].preferred_variant();
        assert_eq!(preferred.kind, VariantKind::Original);
        assert!(preferred.is_original);
    }

    #[tokio::test]
    async fn pagination_walks_the_creator_posts() {
        let resolver = FixtureResolver::standard();
        let creator = resolver.resolve_profile(FIXTURE_PROFILE_URL).await.unwrap();

        let first = resolver
            .list_posts(&creator, PageRequest::first(1))
            .await
            .unwrap();
        assert_eq!(first.posts.len(), 1);
        assert!(first.has_more);
        assert_eq!(first.total_known, Some(2));

        let second = resolver
            .list_posts(&creator, PageRequest::next(first.next_cursor.unwrap(), 1))
            .await
            .unwrap();
        assert_eq!(second.posts.len(), 1);
        assert!(!second.has_more);
        assert!(second.next_cursor.is_none());
    }

    #[tokio::test]
    async fn unknown_url_is_not_found() {
        let resolver = FixtureResolver::standard();
        let err = resolver.resolve_post("https://v.douyin.com/unknown/").await.unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::NotFound);
    }
}
