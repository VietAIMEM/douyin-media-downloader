//! Resolver layer.
//!
//! The resolver layer is the *only* place that understands a specific content
//! platform. Everything above it (services, commands, UI) works with
//! [`Creator`], [`Post`], [`MediaItem`], and the traits defined here.
//!
//! Three independent interfaces are defined:
//!
//! * [`ProfileResolver`] — creator lookup and post enumeration (with paging).
//! * [`PostResolver`] — resolving one content URL into a [`Post`].
//! * [`MediaResolver`] — turning a post's media into downloadable items.
//!
//! They are separate so a platform can implement them incrementally (Phase 02
//! fills in post + media, Phase 03 adds profiles) and so a caller only depends
//! on the capability it actually needs. [`ResolverRegistry`] is how the rest of
//! the application finds the right implementation for a URL without knowing
//! which platform it belongs to.

pub mod douyin;
pub mod fixture;
pub mod source_url;

pub use douyin::{classify, DouyinResolver, DouyinUrlParser};
pub use douyin::extract::RESOLVED_CREATOR_KEY;
pub use fixture::FixtureResolver;
pub use source_url::{SourceUrl, SourceUrlKind, SourceUrlParser};

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::errors::{AppError, AppResult};
use crate::models::{Creator, MediaItem, Platform, Post};
use crate::network::HttpClient;

/// One page of posts requested from a [`ProfileResolver`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageRequest {
    /// Opaque continuation token from a previous [`PostPage`]. `None` starts
    /// from the beginning.
    pub cursor: Option<String>,
    /// Preferred page size. Implementations may clamp this.
    pub limit: u32,
    /// Whether to read through the application's signed-in browser profile.
    ///
    /// A request about *which account* reads the page rather than which page to
    /// read, and it belongs here because the resolver is what decides how to
    /// open the page, and it is the only layer that knows the answer has to
    /// reach the browser.
    ///
    /// Carries no claim that a sign-in exists. A resolver asked for an
    /// authenticated page with no session gets Douyin's public answer, which is
    /// the honest outcome and is reported as an authentication problem rather
    /// than as an empty profile.
    #[serde(default)]
    pub authenticated: bool,
}

impl PageRequest {
    /// The first page of a listing.
    pub fn first(limit: u32) -> Self {
        Self {
            cursor: None,
            limit,
            authenticated: false,
        }
    }

    /// The page after `cursor`.
    pub fn next(cursor: impl Into<String>, limit: u32) -> Self {
        Self {
            cursor: Some(cursor.into()),
            limit,
            authenticated: false,
        }
    }

    /// Read this page through the signed-in browser profile.
    pub fn authenticated(mut self) -> Self {
        self.authenticated = true;
        self
    }
}

impl Default for PageRequest {
    fn default() -> Self {
        Self::first(30)
    }
}

/// One page of posts returned by a [`ProfileResolver`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostPage {
    pub posts: Vec<Post>,
    /// Cursor for the next page; `None` when the listing is exhausted.
    pub next_cursor: Option<String>,
    pub has_more: bool,
    /// Total post count when the platform reports it.
    pub total_known: Option<u32>,
}

/// Resolves creator profiles and enumerates their public posts.
///
/// Pagination state lives behind this interface — `PageRequest`/`PostPage`
/// carry cursors as opaque strings — so the UI never models a platform's
/// paging scheme.
#[async_trait]
pub trait ProfileResolver: Send + Sync {
    /// Short identifier used in diagnostics and logs.
    fn name(&self) -> &'static str;

    /// Platform this resolver serves.
    fn platform(&self) -> Platform;

    /// Whether this resolver can handle `url`.
    fn supports(&self, url: &str) -> bool;

    /// Resolve a profile URL into a [`Creator`].
    async fn resolve_profile(&self, url: &str) -> AppResult<Creator>;

    /// Fetch one page of the creator's posts.
    async fn list_posts(&self, creator: &Creator, page: PageRequest) -> AppResult<PostPage>;
}

/// Resolves a single content URL into a [`Post`].
#[async_trait]
pub trait PostResolver: Send + Sync {
    fn name(&self) -> &'static str;

    fn platform(&self) -> Platform;

    fn supports(&self, url: &str) -> bool;

    /// Resolve one post. Media may be empty until [`MediaResolver`] runs.
    async fn resolve_post(&self, url: &str) -> AppResult<Post>;
}

/// Normalizes a post's media into downloadable [`MediaItem`]s.
///
/// Implementations must prefer an original/clean source when the platform
/// legitimately exposes one. They must never synthesize or watermark-manipulate
/// content.
#[async_trait]
pub trait MediaResolver: Send + Sync {
    fn name(&self) -> &'static str;

    /// Platform this resolver serves.
    fn platform(&self) -> Platform;

    /// Resolve all media for `post`.
    async fn resolve_media(&self, post: &Post) -> AppResult<Vec<MediaItem>>;
}

/// Diagnostic description of a registered resolver.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolverInfo {
    pub name: String,
    pub platform: Platform,
    pub capability: String,
}

/// Look-up table mapping URLs and platforms to resolver implementations.
///
/// Registration is explicit so the wiring is visible in one place
/// (`services::build_registry`) rather than scattered across the codebase.
#[derive(Default)]
pub struct ResolverRegistry {
    parsers: Vec<Arc<dyn SourceUrlParser>>,
    profile_resolvers: Vec<Arc<dyn ProfileResolver>>,
    post_resolvers: Vec<Arc<dyn PostResolver>>,
    media_resolvers: Vec<Arc<dyn MediaResolver>>,
    /// Optional client used to follow short links. Without one, `expand`
    /// degrades to plain classification.
    http: Option<Arc<dyn HttpClient>>,
}

impl ResolverRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Give the registry a client so short links can be followed.
    pub fn with_http_client(mut self, http: Arc<dyn HttpClient>) -> Self {
        self.http = Some(http);
        self
    }

    pub fn register_parser(mut self, parser: impl SourceUrlParser + 'static) -> Self {
        self.parsers.push(Arc::new(parser));
        self
    }

    pub fn register_profile_resolver(mut self, resolver: Arc<dyn ProfileResolver>) -> Self {
        self.profile_resolvers.push(resolver);
        self
    }

    pub fn register_post_resolver(mut self, resolver: Arc<dyn PostResolver>) -> Self {
        self.post_resolvers.push(resolver);
        self
    }

    pub fn register_media_resolver(mut self, resolver: Arc<dyn MediaResolver>) -> Self {
        self.media_resolvers.push(resolver);
        self
    }

    /// Classify a URL using the first parser that accepts it.
    ///
    /// When no parser accepts the input, an explicit "unsupported platform"
    /// error is preferred over a generic parse failure, because it is far more
    /// useful to a user who pasted a link from a different site.
    pub fn parse(&self, raw: &str) -> AppResult<SourceUrl> {
        self.select_parser(raw)?.parse(raw)
    }

    /// Classify a URL, following short links to their concrete target.
    ///
    /// Expansion is delegated to the platform parser, so no caller needs to
    /// know which platforms issue shortened links. Without an HTTP client the
    /// result stays classified-only.
    pub async fn expand(&self, raw: &str) -> AppResult<SourceUrl> {
        let parser = self.select_parser(raw)?;
        match &self.http {
            Some(http) => parser.expand(raw, http).await,
            None => parser.parse(raw),
        }
    }

    /// The first parser that accepts `raw`, or the most informative error.
    fn select_parser(&self, raw: &str) -> AppResult<&Arc<dyn SourceUrlParser>> {
        let mut first_error: Option<AppError> = None;
        for parser in &self.parsers {
            match parser.parse(raw) {
                Ok(_) => return Ok(parser),
                Err(err) => {
                    if err.kind() == crate::errors::AppErrorKind::UnsupportedContent {
                        // A recognized-but-foreign URL: report it immediately.
                        first_error = Some(err);
                        break;
                    }
                    first_error.get_or_insert(err);
                }
            }
        }
        Err(first_error.unwrap_or_else(|| {
            AppError::unsupported("no parser registered for this input")
                .with_user_message("Only Douyin links are supported.")
        }))
    }

    /// Find a profile resolver for a classified URL.
    pub fn profile_resolver_for(&self, source: &SourceUrl) -> AppResult<Arc<dyn ProfileResolver>> {
        self.profile_resolvers
            .iter()
            .find(|r| r.platform() == source.platform && r.supports(&source.original))
            .cloned()
            .ok_or_else(|| unsupported_platform(source.platform, "profiles"))
    }

    /// Find a post resolver for a classified URL.
    pub fn post_resolver_for(&self, source: &SourceUrl) -> AppResult<Arc<dyn PostResolver>> {
        self.post_resolvers
            .iter()
            .find(|r| r.platform() == source.platform && r.supports(&source.original))
            .cloned()
            .ok_or_else(|| unsupported_platform(source.platform, "posts"))
    }

    /// Find a media resolver for a platform.
    ///
    /// Media resolution is not URL-based, so selection is by platform and, on
    /// ties, registration order.
    pub fn media_resolver_for(&self, platform: Platform) -> AppResult<Arc<dyn MediaResolver>> {
        self.media_resolvers
            .iter()
            .find(|r| r.platform() == platform)
            .cloned()
            .ok_or_else(|| unsupported_platform(platform, "media"))
    }

    /// Describe every registered resolver, for the diagnostics command.
    pub fn describe(&self) -> Vec<ResolverInfo> {
        let mut out = Vec::new();
        for parser in &self.parsers {
            out.push(ResolverInfo {
                name: parser.name().to_string(),
                platform: parser.platform(),
                capability: "url".into(),
            });
        }
        for resolver in &self.profile_resolvers {
            out.push(ResolverInfo {
                name: resolver.name().to_string(),
                platform: resolver.platform(),
                capability: "profile".into(),
            });
        }
        for resolver in &self.post_resolvers {
            out.push(ResolverInfo {
                name: resolver.name().to_string(),
                platform: resolver.platform(),
                capability: "post".into(),
            });
        }
        for resolver in &self.media_resolvers {
            out.push(ResolverInfo {
                name: resolver.name().to_string(),
                platform: resolver.platform(),
                capability: "media".into(),
            });
        }
        out
    }
}

fn unsupported_platform(platform: Platform, capability: &str) -> AppError {
    AppError::unsupported(format!(
        "no {capability} resolver registered for platform `{}`",
        platform.as_str()
    ))
    .with_user_message("This link is not supported yet.")
    .with_technical_detail(format!("platform={}, capability={capability}", platform.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_request_helpers_build_the_expected_values() {
        assert_eq!(PageRequest::first(10).cursor, None);
        assert_eq!(PageRequest::next("5", 10).cursor.as_deref(), Some("5"));
        assert_eq!(PageRequest::default().limit, 30);
    }

    #[test]
    fn registry_prefers_unsupported_platform_message() {
        let registry = ResolverRegistry::new().register_parser(DouyinUrlParser::new());
        let err = registry.parse("https://youtube.com/watch?v=1").unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::UnsupportedContent);
    }

    #[test]
    fn registry_classifies_douyin_urls() {
        let registry = ResolverRegistry::new().register_parser(DouyinUrlParser::new());
        let source = registry.parse("https://www.douyin.com/video/123").unwrap();
        assert!(source.is_post());
        assert_eq!(source.platform, Platform::Douyin);
    }

    #[test]
    fn registry_without_parsers_reports_unsupported() {
        let registry = ResolverRegistry::new();
        let err = registry.parse("https://www.douyin.com/video/1").unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::UnsupportedContent);
    }

    #[test]
    fn registry_reports_missing_capability() {
        let registry = ResolverRegistry::new().register_parser(DouyinUrlParser::new());
        let source = registry.parse("https://www.douyin.com/video/1").unwrap();
        assert!(registry.post_resolver_for(&source).is_err());
        assert!(registry.profile_resolver_for(&source).is_err());
    }

    #[test]
    fn registry_finds_registered_resolvers() {
        let registry = ResolverRegistry::new()
            .register_parser(DouyinUrlParser::new())
            .register_profile_resolver(Arc::new(FixtureResolver::standard()))
            .register_post_resolver(Arc::new(FixtureResolver::standard()))
            .register_media_resolver(Arc::new(FixtureResolver::standard()));
        let source = registry.parse(fixture::FIXTURE_VIDEO_URL).unwrap();
        assert!(registry.post_resolver_for(&source).is_ok());
        assert!(registry.media_resolver_for(Platform::Douyin).is_ok());
        assert_eq!(registry.describe().len(), 4);
    }

    #[tokio::test]
    async fn registry_expands_short_links_through_the_parser() {
        use crate::network::MockHttpClient;

        let http = MockHttpClient::new().with_redirect(
            "https://v.douyin.com/abc/",
            "https://www.douyin.com/video/7123456789",
        );
        let registry = ResolverRegistry::new()
            .register_parser(DouyinUrlParser::new())
            .with_http_client(Arc::new(http));

        let expanded = registry.expand("https://v.douyin.com/abc/").await.unwrap();
        assert!(expanded.is_post());
        assert_eq!(expanded.id(), Some("7123456789"));
    }

    #[tokio::test]
    async fn registry_without_a_client_keeps_short_links_classified() {
        let registry = ResolverRegistry::new().register_parser(DouyinUrlParser::new());
        let source = registry.expand("https://v.douyin.com/abc/").await.unwrap();
        assert!(source.is_short_link());
    }
}
