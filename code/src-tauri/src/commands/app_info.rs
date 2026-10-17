//! Application information commands.

use crate::app_state::AppState;
use crate::errors::CommandResult;
use crate::services::AppInfo;

/// Runtime information about the running application: version, data locations,
/// schema version, and the registered resolvers.
///
/// The Home screen calls this on mount, so it doubles as the frontend's proof
/// that the Rust backend is reachable.
///
/// Generic over the runtime rather than fixed to the desktop one, so the
/// integration tests can drive it through the mock runtime the same way.
#[tauri::command]
pub fn get_app_info<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> CommandResult<AppInfo> {
    Ok(state
        .core()
        .app_info(update_channel_configured(&app.config().plugins)))
}

/// Whether the bundled configuration names somewhere to look for updates.
///
/// The application is installed by running an installer, and an installer that
/// carries no feed cannot be checked against — so a build made without the
/// release procedure has to say that plainly. Its Settings screen would
/// otherwise offer a check whose only possible outcome is a failure, and a
/// failure that always happens reads as a defect rather than as a build that
/// was never meant to update itself.
///
/// This reads the configuration rather than the plugin's state because the
/// question is about the build, not about the plugin: the endpoint list is
/// compiled in, and it is empty unless the release procedure supplied one.
fn update_channel_configured(plugins: &tauri::utils::config::PluginConfig) -> bool {
    plugins
        .0
        .get("updater")
        .and_then(|updater| updater.get("endpoints"))
        .and_then(|endpoints| endpoints.as_array())
        .is_some_and(|endpoints| !endpoints.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plugins(value: serde_json::Value) -> tauri::utils::config::PluginConfig {
        serde_json::from_value(value).expect("a plugin configuration")
    }

    #[test]
    fn a_build_without_an_endpoint_has_no_update_channel() {
        // What the committed configuration produces: the plugin is present but
        // names nowhere to look, which is every build that did not come from
        // the release procedure.
        assert!(!update_channel_configured(&plugins(json!({
            "updater": { "endpoints": [], "pubkey": "", "requireSignedVersion": true }
        }))));
    }

    #[test]
    fn a_build_with_an_endpoint_has_one() {
        assert!(update_channel_configured(&plugins(json!({
            "updater": { "endpoints": ["https://example.invalid/stable/latest.json"] }
        }))));
    }

    #[test]
    fn a_build_that_configures_no_updater_at_all_has_none() {
        assert!(!update_channel_configured(&plugins(json!({}))));
        assert!(!update_channel_configured(&plugins(
            json!({ "dialog": { "anything": true } })
        )));
    }

    #[test]
    fn a_malformed_endpoint_value_is_not_a_channel() {
        // Absent, null and a scalar are all "nowhere to look", and none of them
        // should be read as configured just because the key exists.
        assert!(!update_channel_configured(&plugins(json!({
            "updater": { "requireSignedVersion": true }
        }))));
        assert!(!update_channel_configured(&plugins(json!({
            "updater": { "endpoints": null }
        }))));
        assert!(!update_channel_configured(&plugins(json!({
            "updater": { "endpoints": "https://example.invalid/latest.json" }
        }))));
    }

    #[test]
    fn the_committed_configuration_carries_no_update_channel() {
        // The property this build is supposed to have, asserted against the
        // file the bundler embeds rather than against a literal written here.
        // If somebody ever commits an endpoint, the application starts offering
        // an update check, and this fails first.
        let configuration: serde_json::Value =
            serde_json::from_str(include_str!("../../tauri.conf.json"))
                .expect("tauri.conf.json must be JSON");
        let plugins: tauri::utils::config::PluginConfig =
            serde_json::from_value(configuration["plugins"].clone())
                .expect("its plugin section must be a plugin configuration");

        assert!(
            !update_channel_configured(&plugins),
            "the shipped configuration names an update endpoint, so this build would \
             check for updates against it"
        );
    }
}
