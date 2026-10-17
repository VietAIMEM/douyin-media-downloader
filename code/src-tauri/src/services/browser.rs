//! The policy gate around browser-assisted resolution.
//!
//! Starting a browser is a bigger action than making a request, so whether it
//! happens is a user preference rather than a resolver decision. The gate keeps
//! that policy in the service layer, where settings live, and leaves the
//! resolver with one simple question — `is_available()` — that already accounts
//! for both the preference and whether a browser is actually installed.

use std::sync::Arc;

use async_trait::async_trait;

use crate::errors::AppResult;
use crate::network::{PageRenderer, RenderRequest, RenderedPage};
use crate::settings::SettingsService;

/// A [`PageRenderer`] that is unavailable when the user has switched browser
/// assistance off.
pub struct SettingGatedRenderer {
    inner: Arc<dyn PageRenderer>,
    settings: Arc<SettingsService>,
}

impl SettingGatedRenderer {
    pub fn new(inner: Arc<dyn PageRenderer>, settings: Arc<SettingsService>) -> Self {
        Self { inner, settings }
    }

    /// The wrapped renderer, for diagnostics.
    pub fn inner_name(&self) -> &'static str {
        self.inner.name()
    }
}

#[async_trait]
impl PageRenderer for SettingGatedRenderer {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn is_available(&self) -> bool {
        // Read live, so switching the preference takes effect on the next
        // resolution rather than on the next launch.
        self.settings.current().browser_assisted_resolution && self.inner.is_available()
    }

    async fn render(&self, request: &RenderRequest<'_>) -> AppResult<RenderedPage> {
        self.inner.render(request).await
    }

    fn shutdown_now(&self) {
        // Forwarded rather than dropped: the gate decides whether a browser may
        // be *started*, and a browser the user has since switched off still
        // needs closing on the way out.
        self.inner.shutdown_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::network::renderer::{MediaSource, MediaSourceKind, MockPageRenderer};

    fn settings() -> Arc<SettingsService> {
        Arc::new(SettingsService::new(Database::open_in_memory().unwrap()).unwrap())
    }

    fn gated(
        setting: bool,
        renderer: MockPageRenderer,
    ) -> (Arc<SettingsService>, SettingGatedRenderer) {
        let settings = settings();
        settings
            .update(|s| {
                s.browser_assisted_resolution = setting;
                Ok(())
            })
            .unwrap();
        let gated = SettingGatedRenderer::new(Arc::new(renderer), Arc::clone(&settings));
        (settings, gated)
    }

    #[test]
    fn the_preference_can_disable_an_otherwise_available_browser() {
        let (_settings, renderer) = gated(
            false,
            MockPageRenderer::returning(vec![MediaSource::test(
                "https://cdn/a.mp4",
                MediaSourceKind::Video,
            )]),
        );
        assert!(!renderer.is_available());
    }

    #[test]
    fn the_preference_defaults_to_on() {
        let (_settings, renderer) = gated(
            true,
            MockPageRenderer::returning(vec![MediaSource::test(
                "https://cdn/a.mp4",
                MediaSourceKind::Video,
            )]),
        );
        assert!(renderer.is_available());
    }

    #[test]
    fn a_missing_browser_stays_unavailable_even_when_the_preference_is_on() {
        let (_settings, renderer) = gated(true, MockPageRenderer::unavailable());
        assert!(!renderer.is_available());
    }

    #[test]
    fn switching_the_preference_takes_effect_without_a_restart() {
        let (settings, renderer) = gated(
            true,
            MockPageRenderer::returning(vec![MediaSource::test(
                "https://cdn/a.mp4",
                MediaSourceKind::Video,
            )]),
        );
        assert!(renderer.is_available());

        settings
            .update(|s| {
                s.browser_assisted_resolution = false;
                Ok(())
            })
            .unwrap();
        assert!(
            !renderer.is_available(),
            "the gate must read the setting live"
        );

        settings
            .update(|s| {
                s.browser_assisted_resolution = true;
                Ok(())
            })
            .unwrap();
        assert!(renderer.is_available());
    }

    #[tokio::test]
    async fn rendering_delegates_to_the_wrapped_renderer() {
        let (_settings, renderer) = gated(
            true,
            MockPageRenderer::returning(vec![MediaSource::test(
                "https://cdn/a.mp4",
                MediaSourceKind::Video,
            )]),
        );
        let request = RenderRequest::new("https://example.test", "true", "({})");
        let page = renderer.render(&request).await.unwrap();
        assert_eq!(page.media.len(), 1);
        assert_eq!(renderer.name(), "mock");
    }
}
