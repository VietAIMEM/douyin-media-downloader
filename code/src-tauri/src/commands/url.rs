//! URL classification and inspection commands.
//!
//! These commands let the UI validate and explain a pasted link without
//! duplicating a single line of Douyin logic in TypeScript. Classification is
//! performed by the resolver registry through the application service, so the
//! answer the UI sees is exactly what the download pipeline will see.
//!
//! All of the real work lives in [`crate::services::single_url`]; these are thin
//! adapters that unwrap state and convert errors.

use serde::Serialize;

use crate::app_state::AppState;
use crate::errors::{to_command_error, AppResult, CommandResult};
use crate::models::{Creator, Post};
use crate::resolvers::{fixture, ResolverRegistry};
use crate::services::single_url::{describe, UrlClassification};

/// Classify a pasted link. Never touches the network.
#[tauri::command]
pub fn classify_url(
    state: tauri::State<'_, AppState>,
    url: String,
) -> CommandResult<UrlClassification> {
    classify_with(state.core().resolvers(), &url).map_err(to_command_error)
}

/// Classify a link, following Douyin short links to their real target.
///
/// This is a real network request and will fail with a `network` error when the
/// machine is offline.
#[tauri::command]
pub async fn expand_url(
    state: tauri::State<'_, AppState>,
    url: String,
) -> CommandResult<UrlClassification> {
    let registry = state.core().resolvers();
    let expanded = registry.expand(&url).await.map_err(to_command_error)?;
    Ok(describe(registry, expanded))
}

/// What the resolver layer could determine about a link.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UrlPreview {
    pub classification: UrlClassification,
    /// `post` or `profile`.
    pub resolved_as: String,
    pub post: Option<Post>,
    pub creator: Option<Creator>,
    /// How far resolution got: `resolved` or `classified_only`.
    pub resolution_stage: String,
}

/// Resolve a link and return whatever domain data is available for it.
///
/// This is the read-only counterpart to the Single URL screen's analyze step:
/// it reports what a link is without creating anything. Use `analyze_url` when
/// the user intends to download, because that also persists the resolution.
#[tauri::command]
pub async fn resolve_url_metadata(
    state: tauri::State<'_, AppState>,
    url: String,
) -> CommandResult<UrlPreview> {
    let analyzed = state
        .core()
        .analyze_url(&url)
        .await
        .map_err(to_command_error)?;
    Ok(UrlPreview {
        classification: analyzed.classification,
        resolved_as: analyzed.intent,
        post: analyzed.post,
        creator: analyzed.creator,
        resolution_stage: analyzed.resolution_stage,
    })
}

/// One of the links the project ships for validating the architecture.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestFixture {
    pub label: String,
    pub url: String,
    /// The screen this link belongs on: `profile` or `post`.
    ///
    /// Both shipped links are short links, so the parser cannot tell them
    /// apart without following the redirect. Declaring the intent next to the
    /// URL keeps the UI from having to know either link itself.
    pub intent: String,
    /// How the parser classifies it (derived, not hard-coded).
    pub expected_kind: String,
}

/// The known-good test URLs, for the UI's quick-fill buttons.
#[tauri::command]
pub fn get_test_fixtures() -> CommandResult<Vec<TestFixture>> {
    let fixtures = [
        ("Profile", fixture::FIXTURE_PROFILE_URL, "profile"),
        ("Video post", fixture::FIXTURE_VIDEO_URL, "post"),
    ];

    let out = fixtures
        .into_iter()
        .map(|(label, url, intent)| TestFixture {
            label: label.to_string(),
            url: url.to_string(),
            intent: intent.to_string(),
            expected_kind: crate::resolvers::classify(url)
                .map(|source| source.kind.label().to_string())
                .unwrap_or_else(|_| "unsupported".to_string()),
        })
        .collect();

    Ok(out)
}

fn classify_with(registry: &ResolverRegistry, url: &str) -> AppResult<UrlClassification> {
    let source = registry.parse(url)?;
    Ok(describe(registry, source))
}
