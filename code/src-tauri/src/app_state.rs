//! Managed state handed to every Tauri command.
//!
//! Commands receive `State<AppState>` and reach the application core through
//! [`AppState::core`]. Keeping the wrapper this thin means the command layer
//! cannot accidentally build its own services: there is exactly one
//! [`AppCore`], created at startup and shared by reference.

use std::sync::Arc;

use crate::services::AppCore;

/// Shared handle to the application core.
pub struct AppState {
    core: Arc<AppCore>,
}

impl AppState {
    pub fn new(core: AppCore) -> Self {
        Self {
            core: Arc::new(core),
        }
    }

    /// Access the application core.
    pub fn core(&self) -> &AppCore {
        &self.core
    }
}

impl From<Arc<AppCore>> for AppState {
    fn from(core: Arc<AppCore>) -> Self {
        Self { core }
    }
}
