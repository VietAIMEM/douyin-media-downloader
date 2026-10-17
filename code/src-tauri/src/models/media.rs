//! Media domain models.
//!
//! A [`MediaItem`] is one downloadable resource inside a post (a video, one
//! image of a carousel, or an audio track). A [`MediaVariant`] is an
//! alternative rendition of that same resource — for example an original
//! source and a standard-quality transcode.
//!
//! Platforms rarely expose all of this at once; resolvers fill in whatever is
//! legitimately available and leave the rest as `None`.

use serde::{Deserialize, Serialize};

use super::common::{new_id, Id, MediaType, Metadata, VariantKind};

/// Metadata key under which a resolver records a [`TrackLayout`].
pub const TRACK_LAYOUT_KEY: &str = "track_layout";

/// What tracks a media item actually has.
///
/// Platforms that stream video and audio as separate renditions make "a video"
/// and "a video with sound" genuinely different findings, and a download that
/// silently produces the first when the user asked for the second is the kind of
/// quiet wrongness this type exists to prevent.
///
/// The distinction between [`VideoOnly`](Self::VideoOnly) and
/// [`Unknown`](Self::Unknown) is deliberate: the first means the source was
/// watched and offered no audio, the second means nobody looked. Only the first
/// is a statement about the content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackLayout {
    /// Video and audio were both found.
    AudioVideo,
    /// The source offers the video without an accompanying audio track.
    VideoOnly,
    /// An audio track with no video.
    AudioOnly,
    /// Whether an audio track exists could not be determined.
    Unknown,
}

impl TrackLayout {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AudioVideo => "audio_video",
            Self::VideoOnly => "video_only",
            Self::AudioOnly => "audio_only",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a stored layout, tolerant of the older spellings this project has
    /// used so an existing row is not misread as "unknown".
    pub fn parse(value: &str) -> Self {
        match value {
            "audio_video" => Self::AudioVideo,
            "video_only" | "video_only_track" | "video_track" => Self::VideoOnly,
            "audio_only" | "audio_track" => Self::AudioOnly,
            _ => Self::Unknown,
        }
    }
}

impl std::fmt::Display for TrackLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single downloadable resource belonging to a post.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaItem {
    pub id: Id,
    pub post_id: Id,
    pub media_type: MediaType,
    /// Primary, directly downloadable URL.
    pub source_url: String,
    pub thumbnail_url: Option<String>,
    pub mime_type: Option<String>,
    /// File extension without the leading dot, e.g. `mp4`.
    pub extension: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_seconds: Option<f64>,
    pub file_size: Option<u64>,
    /// Alternative renditions, ordered by resolver preference (best first).
    pub variants: Vec<MediaVariant>,
    #[serde(default)]
    pub metadata: Metadata,
}

impl MediaItem {
    /// Construct an item with only the fields a resolver always knows.
    pub fn new(post_id: impl Into<Id>, media_type: MediaType, source_url: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            post_id: post_id.into(),
            media_type,
            source_url: source_url.into(),
            thumbnail_url: None,
            mime_type: None,
            extension: None,
            width: None,
            height: None,
            duration_seconds: None,
            file_size: None,
            variants: Vec::new(),
            metadata: Metadata::new(),
        }
    }

    /// Best variant to download by default.
    ///
    /// Prefers an original/clean source when the platform exposes one (without
    /// any watermark manipulation), then the highest-priority variant, and
    /// finally the item's own `source_url` expressed as a synthetic variant.
    pub fn preferred_variant(&self) -> MediaVariant {
        if let Some(original) = self.variants.iter().find(|v| v.is_original) {
            return original.clone();
        }
        if let Some(first) = self.variants.first() {
            return first.clone();
        }
        MediaVariant {
            id: new_id(),
            media_item_id: self.id.clone(),
            kind: VariantKind::Standard,
            label: "default".to_string(),
            source_url: self.source_url.clone(),
            mime_type: self.mime_type.clone(),
            extension: self.extension.clone(),
            width: self.width,
            height: self.height,
            file_size: self.file_size,
            bitrate_kbps: None,
            is_original: false,
        }
    }

    /// What tracks this item was found to have, if the resolver said.
    ///
    /// Read from metadata so it survives the existing schema: the layout is
    /// derived from what a resolver observed, not authored, so it does not
    /// warrant a column of its own.
    pub fn track_layout(&self) -> Option<TrackLayout> {
        self.metadata
            .get(TRACK_LAYOUT_KEY)
            .and_then(|value| value.as_str())
            .map(TrackLayout::parse)
    }

    /// Record what tracks this item was found to have.
    pub fn set_track_layout(&mut self, layout: TrackLayout) {
        self.metadata.insert(
            TRACK_LAYOUT_KEY.to_string(),
            serde_json::json!(layout.as_str()),
        );
    }
}

/// An alternative rendition of a media item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaVariant {
    pub id: Id,
    pub media_item_id: Id,
    pub kind: VariantKind,
    /// Human-readable label for the UI, e.g. `1080p` or `Original`.
    pub label: String,
    pub source_url: String,
    pub mime_type: Option<String>,
    pub extension: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub file_size: Option<u64>,
    pub bitrate_kbps: Option<u32>,
    /// True when this variant is the platform's original/clean source.
    pub is_original: bool,
}

impl MediaVariant {
    pub fn new(
        media_item_id: impl Into<Id>,
        kind: VariantKind,
        label: impl Into<String>,
        source_url: impl Into<String>,
    ) -> Self {
        Self {
            id: new_id(),
            media_item_id: media_item_id.into(),
            kind,
            label: label.into(),
            source_url: source_url.into(),
            mime_type: None,
            extension: None,
            width: None,
            height: None,
            file_size: None,
            bitrate_kbps: None,
            is_original: matches!(kind, VariantKind::Original),
        }
    }

    /// e.g. `1920x1080` when both dimensions are known.
    pub fn resolution_label(&self) -> Option<String> {
        match (self.width, self.height) {
            (Some(w), Some(h)) => Some(format!("{w}x{h}")),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_variant_prefers_original() {
        let mut item = MediaItem::new("post-1", MediaType::Video, "https://cdn/standard.mp4");
        item.variants = vec![
            MediaVariant::new(item.id.clone(), VariantKind::Standard, "Standard", "https://cdn/standard.mp4"),
            MediaVariant::new(item.id.clone(), VariantKind::Original, "Original", "https://cdn/original.mp4"),
        ];
        let chosen = item.preferred_variant();
        assert_eq!(chosen.kind, VariantKind::Original);
        assert!(chosen.is_original);
    }

    #[test]
    fn preferred_variant_falls_back_to_synthetic_default() {
        let item = MediaItem::new("post-1", MediaType::Image, "https://cdn/a.jpg");
        let chosen = item.preferred_variant();
        assert_eq!(chosen.source_url, "https://cdn/a.jpg");
        assert_eq!(chosen.kind, VariantKind::Standard);
        assert_eq!(chosen.media_item_id, item.id);
    }

    #[test]
    fn resolution_label_formats_dimensions() {
        let mut v = MediaVariant::new("m1", VariantKind::High, "High", "https://cdn/h.mp4");
        v.width = Some(1920);
        v.height = Some(1080);
        assert_eq!(v.resolution_label().as_deref(), Some("1920x1080"));
    }
}
