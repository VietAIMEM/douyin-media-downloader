//! Post domain model.

use serde::{Deserialize, Serialize};

use super::common::{new_id, ContentStatus, Id, MediaType, Metadata, Platform, Timestamp};
use super::media::MediaItem;

/// A single piece of content (video, image post, carousel...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Post {
    /// Local identifier.
    pub id: Id,
    pub platform: Platform,
    /// Identifier on the source platform (the numeric aweme id).
    pub platform_post_id: String,
    /// Local creator id, once the creator has been persisted.
    pub creator_id: Option<Id>,
    /// Creator handle as reported by the source, useful before persistence.
    pub creator_handle: Option<String>,
    /// URL the user supplied to reach this post.
    pub url: String,
    /// Canonical URL after short-link resolution, when known.
    pub canonical_url: Option<String>,
    pub caption: Option<String>,
    pub media_type: MediaType,
    pub thumbnail_url: Option<String>,
    pub published_at: Option<Timestamp>,
    /// Availability of the source content.
    pub status: ContentStatus,
    /// Media resolved so far. May be empty immediately after post resolution.
    #[serde(default)]
    pub media_items: Vec<MediaItem>,
    #[serde(default)]
    pub metadata: Metadata,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
}

impl Post {
    pub fn new(
        platform: Platform,
        platform_post_id: impl Into<String>,
        media_type: MediaType,
        url: impl Into<String>,
    ) -> Self {
        let ts = crate::models::common::now();
        Self {
            id: new_id(),
            platform,
            platform_post_id: platform_post_id.into(),
            creator_id: None,
            creator_handle: None,
            url: url.into(),
            canonical_url: None,
            caption: None,
            media_type,
            thumbnail_url: None,
            published_at: None,
            status: ContentStatus::New,
            media_items: Vec::new(),
            metadata: Metadata::new(),
            created_at: Some(ts),
            updated_at: Some(ts),
        }
    }

    /// Total number of media items attached to the post.
    pub fn media_count(&self) -> usize {
        self.media_items.len()
    }

    /// Short display title: caption if present, otherwise a placeholder built
    /// from the platform id. Never returns an empty string.
    pub fn display_title(&self) -> String {
        match self.caption.as_deref() {
            Some(c) if !c.trim().is_empty() => c.trim().to_string(),
            _ => format!("{} post {}", self.platform.as_str(), self.platform_post_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_title_prefers_caption() {
        let mut post = Post::new(Platform::Douyin, "7123", MediaType::Video, "https://d/1");
        assert!(post.display_title().contains("7123"));
        post.caption = Some("  Sunset timelapse  ".into());
        assert_eq!(post.display_title(), "Sunset timelapse");
    }

    #[test]
    fn new_post_starts_as_new_with_no_media() {
        let post = Post::new(Platform::Douyin, "1", MediaType::Image, "https://d/1");
        assert_eq!(post.status, ContentStatus::New);
        assert_eq!(post.media_count(), 0);
    }
}
