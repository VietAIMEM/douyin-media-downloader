//! URL validation and classification, exercised through the public API.
//!
//! These tests cover the boundary the UI depends on: a pasted link is either
//! classified into a [`SourceUrl`] or rejected with a *typed* error the UI can
//! explain. The two URLs the project ships for validation
//! (`https://v.douyin.com/SMt7NsUPaGo/` and `https://v.douyin.com/7yyhouNSzvA/`)
//! are asserted here, since they are the links the acceptance criteria name.

use std::sync::Arc;

use douyin_downloader_lib::errors::AppErrorKind;
use douyin_downloader_lib::models::Platform;
use douyin_downloader_lib::network::{HttpClient, MockHttpClient};
use douyin_downloader_lib::resolvers::{
    classify,
    fixture::{FIXTURE_PROFILE_URL, FIXTURE_VIDEO_URL},
    DouyinResolver, DouyinUrlParser, ResolverRegistry, SourceUrlKind, SourceUrlParser,
};

fn parser() -> DouyinUrlParser {
    DouyinUrlParser::new()
}

// --- the shipped fixtures ---------------------------------------------------

#[test]
fn shipped_profile_fixture_classifies_as_a_douyin_short_link() {
    let source = parser().parse(FIXTURE_PROFILE_URL).expect("fixture must classify");

    assert_eq!(source.platform, Platform::Douyin);
    assert_eq!(source.original, FIXTURE_PROFILE_URL);
    // The shipped link is a `v.` short link; its target needs a redirect hop.
    assert_eq!(
        source.kind,
        SourceUrlKind::ShortLink {
            code: "SMt7NsUPaGo".to_string()
        }
    );
    assert!(source.is_short_link());
    assert_eq!(source.id(), None, "a short link carries no platform id yet");
}

#[test]
fn shipped_video_fixture_classifies_as_a_douyin_short_link() {
    let source = parser().parse(FIXTURE_VIDEO_URL).expect("fixture must classify");

    assert_eq!(source.platform, Platform::Douyin);
    assert_eq!(
        source.kind,
        SourceUrlKind::ShortLink {
            code: "7yyhouNSzvA".to_string()
        }
    );
}

#[test]
fn short_links_report_that_expansion_is_required() {
    // This is what drives the UI's "Expand short link" affordance.
    for url in [FIXTURE_PROFILE_URL, FIXTURE_VIDEO_URL] {
        let source = parser().parse(url).unwrap();
        assert!(source.is_short_link(), "{url} must be recognized as a short link");
        assert!(!source.is_profile() && !source.is_post());
    }
}

// --- concrete profile and post links ---------------------------------------

#[test]
fn concrete_profile_links_yield_a_creator_id() {
    let source = parser()
        .parse("https://www.douyin.com/user/MS4wLjABAAAAexample")
        .expect("profile link must classify");

    assert!(source.is_profile());
    assert_eq!(source.id(), Some("MS4wLjABAAAAexample"));
    assert_eq!(source.canonical, "https://www.douyin.com/user/MS4wLjABAAAAexample");
}

#[test]
fn concrete_video_and_note_links_yield_a_post_id() {
    for (url, id) in [
        ("https://www.douyin.com/video/7300000000000000000", "7300000000000000000"),
        ("https://www.douyin.com/note/7300000000000000001", "7300000000000000001"),
    ] {
        let source = parser().parse(url).expect("post link must classify");
        assert!(source.is_post(), "{url} must be a post");
        assert_eq!(source.id(), Some(id));
    }
}

#[test]
fn sharing_urls_are_normalized_to_canonical_forms() {
    // `share/video/<id>` and `share/user/<id>` are the forms the Douyin share
    // sheet produces; both must reduce to the same canonical shape as the web
    // URLs so downstream code sees one form.
    let share_video = parser()
        .parse("https://www.douyin.com/share/video/7300000000000000000")
        .unwrap();
    assert_eq!(share_video.canonical, "https://www.douyin.com/video/7300000000000000000");

    let share_user = parser()
        .parse("https://www.douyin.com/share/user/MS4wLjABAAAAexample")
        .unwrap();
    assert_eq!(share_user.canonical, "https://www.douyin.com/user/MS4wLjABAAAAexample");
}

#[test]
fn modal_links_resolve_to_the_post_they_open() {
    let source = parser()
        .parse("https://www.douyin.com/?modal_id=7300000000000000000")
        .expect("modal link must classify");

    assert!(source.is_post());
    assert_eq!(source.id(), Some("7300000000000000000"));
}

#[test]
fn hosts_are_matched_case_insensitively_and_a_missing_scheme_is_tolerated() {
    let upper = parser()
        .parse("HTTPS://WWW.DOUYIN.COM/video/7300000000000000000")
        .expect("upper-case host must classify");
    assert!(upper.is_post());

    let no_scheme = parser()
        .parse("www.douyin.com/video/7300000000000000000")
        .expect("scheme-less input must classify");
    assert!(no_scheme.is_post());

    let mobile = parser()
        .parse("https://m.douyin.com/video/7300000000000000000")
        .expect("mobile subdomain must classify");
    assert!(mobile.is_post());
}

// --- rejections -------------------------------------------------------------

#[test]
fn empty_input_is_a_validation_error() {
    for raw in ["", "   ", "\t\n"] {
        let err = classifier_error(raw);
        assert_eq!(err, AppErrorKind::Validation, "input {raw:?}");
    }
}

#[test]
fn text_that_is_not_a_url_is_a_validation_error() {
    let err = classifier_error("hello world");
    assert_eq!(err, AppErrorKind::Validation);
}

#[test]
fn urls_on_other_platforms_are_unsupported() {
    for raw in [
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        "https://example.com/video/123",
        // A look-alike host must not be accepted by a naive suffix check.
        "https://notdouyin.com/video/123",
    ] {
        let err = classifier_error(raw);
        assert_eq!(err, AppErrorKind::UnsupportedContent, "input {raw}");
    }
}

#[test]
fn douyin_urls_without_a_profile_or_post_path_are_rejected() {
    for raw in [
        "https://www.douyin.com/",
        "https://www.douyin.com/discover",
        "https://www.douyin.com/user/",
    ] {
        let err = classifier_error(raw);
        assert_eq!(err, AppErrorKind::Validation, "input {raw}");
    }
}

#[test]
fn identifiers_may_not_contain_separators_or_traversal() {
    for raw in [
        "https://www.douyin.com/video/..",
        "https://www.douyin.com/user/%2e%2e",
    ] {
        let err = classifier_error(raw);
        assert_eq!(err, AppErrorKind::Validation, "input {raw}");
    }
}

/// The kind of the error `classify` produced for `raw`.
fn classifier_error(raw: &str) -> AppErrorKind {
    classify(raw)
        .err()
        .unwrap_or_else(|| panic!("{raw:?} should have been rejected"))
        .kind()
}

// --- the registry -----------------------------------------------------------

#[test]
fn the_registry_classifies_links_through_a_registered_parser() {
    let registry = ResolverRegistry::new().register_parser(parser());

    let source = registry.parse(FIXTURE_VIDEO_URL).unwrap();
    assert_eq!(source.platform, Platform::Douyin);

    assert_eq!(registry.parse("").unwrap_err().kind(), AppErrorKind::Validation);
}

#[test]
fn a_registry_without_the_right_resolver_reports_that_rather_than_guessing() {
    // Only post resolution is registered; a profile link must not fall through
    // to the post resolver.
    let http: Arc<dyn HttpClient> = Arc::new(MockHttpClient::new());
    let registry = ResolverRegistry::new()
        .register_parser(parser())
        .with_http_client(Arc::clone(&http))
        .register_post_resolver(Arc::new(DouyinResolver::new(http)));

    let profile = registry
        .parse("https://www.douyin.com/user/MS4wLjABAAAAexample")
        .unwrap();
    assert!(registry.profile_resolver_for(&profile).is_err());

    let post = registry.parse("https://www.douyin.com/video/7300000000000000000").unwrap();
    assert_eq!(registry.post_resolver_for(&post).unwrap().name(), "douyin-url");
}

#[tokio::test]
async fn the_registry_expands_a_short_link_through_its_parser() {
    let client = MockHttpClient::new().with_redirect(
        FIXTURE_VIDEO_URL,
        "https://www.douyin.com/video/7300000000000000000",
    );
    let registry = ResolverRegistry::new()
        .register_parser(parser())
        .with_http_client(Arc::new(client));

    let expanded = registry.expand(FIXTURE_VIDEO_URL).await.unwrap();
    assert!(expanded.is_post());
    assert_eq!(expanded.id(), Some("7300000000000000000"));
    assert_eq!(
        expanded.original, FIXTURE_VIDEO_URL,
        "the original input is preserved for display"
    );
}

#[tokio::test]
async fn expanding_a_concrete_link_needs_no_network_round_trip() {
    // No redirect is registered, so a network call would leave the URL
    // unchanged; the important property is that the parser short-circuits.
    let registry = ResolverRegistry::new()
        .register_parser(parser())
        .with_http_client(Arc::new(MockHttpClient::new()));

    let expanded = registry
        .expand("https://www.douyin.com/video/7300000000000000000")
        .await
        .unwrap();
    assert!(expanded.is_post());
}

#[tokio::test]
async fn a_short_link_that_leads_nowhere_is_a_resolution_error() {
    // Redirects to a non-Douyin host: the parser rejects it, and the resolver
    // must translate that into a resolution failure rather than a raw
    // validation error, because the *link* was valid — its target was not.
    let client = MockHttpClient::new().with_redirect(FIXTURE_PROFILE_URL, "https://example.com/");
    let registry = ResolverRegistry::new()
        .register_parser(parser())
        .with_http_client(Arc::new(client));

    let err = registry.expand(FIXTURE_PROFILE_URL).await.unwrap_err();
    assert_eq!(err.kind(), AppErrorKind::Resolution);
}

#[tokio::test]
async fn a_network_failure_while_expanding_surfaces_as_a_network_error() {
    let client = MockHttpClient::new().with_status(FIXTURE_VIDEO_URL, 503);
    let registry = ResolverRegistry::new()
        .register_parser(parser())
        .with_http_client(Arc::new(client));

    let err = registry.expand(FIXTURE_VIDEO_URL).await.unwrap_err();
    assert_eq!(err.kind(), AppErrorKind::Network);
}
