//! Repository layer.
//!
//! Each submodule owns the SQL for a single aggregate and exposes plain
//! functions over a [`Connection`](rusqlite::Connection). Repositories:
//!
//! * accept and return domain models only — never raw rows;
//! * return [`AppResult`] so `rusqlite` errors are converted at the boundary;
//! * never open connections themselves, which keeps transactions composable.
//!
//! Application code should reach these through [`Database::with_conn`]
//! (or a higher-level service), not by holding a connection.

pub mod creators;
pub mod downloads;
pub mod history;
pub mod library;
pub mod media;
pub mod posts;
pub mod settings;

pub use creators as creators_repo;
pub use downloads as downloads_repo;
pub use history as history_repo;
pub use library as library_repo;
pub use media as media_repo;
pub use posts as posts_repo;
pub use settings as settings_repo;
