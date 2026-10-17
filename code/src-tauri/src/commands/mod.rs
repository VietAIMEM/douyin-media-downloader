//! Tauri command layer.
//!
//! Commands are the *only* boundary between the frontend and the application
//! core. Each one is deliberately thin: it unwraps managed state, calls exactly
//! one core operation, and converts the result to the wire format.
//!
//! Two rules hold for every command in this module:
//!
//! 1. **No business logic.** Anything that needs more than a few lines belongs
//!    in the application core, where it can be tested without a Tauri runtime.
//! 2. **Errors are structured.** Commands return [`CommandResult`], so failures
//!    arrive in the frontend as `{ kind, code, message, details? }` rather than
//!    a stringified Rust error.

pub mod app_info;
pub mod database;
pub mod dashboard;
pub mod downloads;
pub mod history;
pub mod library;
pub mod login;
pub mod profile;
pub mod settings;
pub mod single_url;
pub mod url;
