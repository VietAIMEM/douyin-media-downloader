//! The platform-agnostic URL boundary.
//!
//! A [`SourceUrl`] is what a URL *means* to the application, decoupled from how
//! any particular platform encodes it. Resolvers consume `SourceUrl`s instead of
//! raw strings, which is what allows a second platform to be added later
//! without touching the download, storage, or UI layers.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::errors::AppResult;
use crate::models::Platform;
use crate::network::HttpClient;

/// What a source URL points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SourceUrlKind {
    /// A creator profile.
    Profile { id: String },
    /// A single post (video or image post).
    Post { id: String },
    /// A shortened link whose target requires a network round-trip to learn.
    ShortLink { code: String },
}

impl SourceUrlKind {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Profile { .. } => "profile",
            Self::Post { .. } => "post",
            Self::ShortLink { .. } => "short_link",
        }
    }
}

/// A classified, canonicalized source URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceUrl {
    pub platform: Platform,
    /// Exactly what the user supplied (trimmed).
    pub original: String,
    /// Normalized form: canonical host, no tracking query parameters.
    pub canonical: String,
    /// Where a short link actually landed, unmodified.
    ///
    /// This is deliberately *not* the same as `canonical`. A share redirect
    /// carries parameters the platform issued and validates — a share
    /// signature, a timestamp, a "render server-side" flag — and a request
    /// built by hand without them is not the request the platform told us to
    /// make. Fetching the landing URL verbatim is what a browser does, so the
    /// resolvers do it too; `canonical` stays the clean form for display and
    /// for identity.
    ///
    /// `None` when the input was already concrete and no redirect was followed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_url: Option<String>,
    pub kind: SourceUrlKind,
}

impl SourceUrl {
    pub fn is_profile(&self) -> bool {
        matches!(self.kind, SourceUrlKind::Profile { .. })
    }

    pub fn is_post(&self) -> bool {
        matches!(self.kind, SourceUrlKind::Post { .. })
    }

    pub fn is_short_link(&self) -> bool {
        matches!(self.kind, SourceUrlKind::ShortLink { .. })
    }

    /// The platform-specific id, when the URL was specific enough to yield one.
    pub fn id(&self) -> Option<&str> {
        match &self.kind {
            SourceUrlKind::Profile { id } | SourceUrlKind::Post { id } => Some(id),
            SourceUrlKind::ShortLink { .. } => None,
        }
    }
}

/// Parses raw user input into a [`SourceUrl`] for one platform.
///
/// One implementation exists per supported platform; the registry picks the
/// first parser that accepts an input.
///
/// Follows the async trait-object style shared by the other resolver
/// interfaces so [`SourceUrlParser`] objects can be stored in the registry.
#[async_trait]
pub trait SourceUrlParser: Send + Sync {
    /// Short identifier used in diagnostics and logs.
    fn name(&self) -> &'static str;

    /// The platform this parser handles.
    fn platform(&self) -> Platform;

    /// Parse and classify `raw`, or return a validation/unsupported error
    /// describing why the input was rejected.
    fn parse(&self, raw: &str) -> AppResult<SourceUrl>;

    /// Resolve a short link into the concrete URL it points at.
    ///
    /// Defaulted to a plain [`parse`](Self::parse): only platforms that issue
    /// shortened links need to override it, and everything else can report the
    /// already-concrete result without any network access.
    async fn expand(&self, raw: &str, http: &Arc<dyn HttpClient>) -> AppResult<SourceUrl> {
        let _ = http;
        self.parse(raw)
    }

    /// Whether this parser accepts `raw`.
    ///
    /// Defaulted rather than required so implementations stay small; override
    /// only if a cheap pre-check is worth it.
    fn supports(&self, raw: &str) -> bool {
        self.parse(raw).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_reports_its_label() {
        assert_eq!(SourceUrlKind::Profile { id: "x".into() }.label(), "profile");
        assert_eq!(SourceUrlKind::Post { id: "x".into() }.label(), "post");
        assert_eq!(SourceUrlKind::ShortLink { code: "x".into() }.label(), "short_link");
    }

    #[test]
    fn kind_serializes_with_tag() {
        let json = serde_json::to_string(&SourceUrlKind::Post { id: "123".into() }).unwrap();
        assert_eq!(json, r#"{"kind":"post","id":"123"}"#);
    }
}
