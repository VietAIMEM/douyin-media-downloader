//! Creator (profile) domain model.

use serde::{Deserialize, Serialize};

use super::common::{new_id, Id, Metadata, Platform, Timestamp};

/// A content creator / profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Creator {
    /// Local identifier.
    pub id: Id,
    pub platform: Platform,
    /// Identifier on the source platform (e.g. Douyin `sec_uid` or numeric uid).
    pub platform_id: String,
    /// Public handle, when the platform exposes one.
    pub handle: Option<String>,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub description: Option<String>,
    pub verified: bool,
    /// Number of public posts, when known.
    pub post_count: Option<u32>,
    /// Canonical profile URL.
    pub profile_url: String,
    #[serde(default)]
    pub metadata: Metadata,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
}

impl Creator {
    pub fn new(
        platform: Platform,
        platform_id: impl Into<String>,
        display_name: impl Into<String>,
        profile_url: impl Into<String>,
    ) -> Self {
        let ts = crate::models::common::now();
        Self {
            id: new_id(),
            platform,
            platform_id: platform_id.into(),
            handle: None,
            display_name: display_name.into(),
            avatar_url: None,
            description: None,
            verified: false,
            post_count: None,
            profile_url: profile_url.into(),
            metadata: Metadata::new(),
            created_at: Some(ts),
            updated_at: Some(ts),
        }
    }

    /// Name used for `{creator}` in folder/filename templates, falling back to
    /// the platform id when the display name is missing.
    pub fn folder_name(&self) -> &str {
        if self.display_name.trim().is_empty() {
            &self.platform_id
        } else {
            &self.display_name
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_name_falls_back_to_platform_id() {
        let mut creator = Creator::new(Platform::Douyin, "MS4wLjAB", "   ", "https://www.douyin.com/user/MS4wLjAB");
        assert_eq!(creator.folder_name(), "MS4wLjAB");
        creator.display_name = "Alice".into();
        assert_eq!(creator.folder_name(), "Alice");
    }
}
