//! Saying something useful when the window cannot be created.
//!
//! # Why this exists
//!
//! Tauri creates the webview inside its own event-loop handler and, when that
//! fails, **panics** — `tauri/src/app.rs`, `panic!("Failed to setup app: {e}")`.
//! The error is never returned to [`crate::run`], so the `unwrap_or_else` there
//! never sees it and [`crate::startup_failure`] never runs. What a user gets is
//! a Rust panic naming a path inside `.cargo\registry`, which describes the
//! failure to nobody: it does not say what went wrong, whether anything was
//! lost, or what to do next.
//!
//! # The one failure this recognises
//!
//! WebView2 lets several processes share one user-data folder **only when they
//! use identical browser options**. If the profile is already held by a process
//! using different options, creating the environment fails with
//! `HRESULT(0x8007139F)`, `ERROR_INVALID_STATE`.
//!
//! That was measured rather than assumed, against a throwaway profile on `C:`:
//!
//! | First instance | Second instance | Result |
//! | --- | --- | --- |
//! | `--remote-debugging-port=9333` | no extra arguments | **fails**, `0x8007139F` |
//! | `--remote-debugging-port=9333` | `--remote-debugging-port=9333` | starts |
//! | no extra arguments | no extra arguments | starts |
//!
//! Two consequences follow, and they shape what this module does *not* do:
//!
//! * A **normal** launch adds no browser arguments, so every normal launch uses
//!   the same options and none of them can block another. Measured: with six
//!   WebView2 children still holding the real profile, a normal launch started
//!   normally. A user who simply opens the application twice is not affected.
//! * Nothing needs cleaning up afterwards. A graceful close *and* a forced kill
//!   were each measured to leave **zero** WebView2 children behind, and a restart
//!   immediately after a forced kill started normally.
//!
//! So there is no orphan to reap and no automatic recovery worth the risk.
//! Killing another process on the strength of its name would be exactly the
//! mistake this must avoid. What is left is the honest, useful thing: explain
//! the failure, say that nothing was lost, and give the safe way out.

/// The WebView2 status for "this user-data folder is in use with other options".
///
/// `ERROR_INVALID_STATE` as an `HRESULT`, which is how it appears in the
/// message Tauri panics with.
const ERROR_INVALID_STATE: &str = "0x8007139F";

/// The phrase Tauri uses when the webview could not be created.
///
/// Matched alongside the status rather than on its own, so a future wording
/// change in Tauri still leaves the status to match on — and an unrelated panic
/// that merely mentions a webview is not mistaken for this one.
const CREATE_FAILED: &str = "failed to create webview";

/// What the user is told, and what they can do about it.
///
/// Deliberately specific about the cause, honest that nothing was touched, and
/// ordered so the cheap remedy comes before the disruptive one.
pub const PROFILE_IN_USE_HELP: &str = "\
Douyin Downloader could not open its window.

Its WebView2 profile is already open in another process that is using different
browser settings, and Windows will not let two processes share one profile unless
they were started the same way. The usual cause is a second copy of this
application started with a different WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS — a
debugger port, for instance — or started from a different build.

Nothing has been changed or deleted. Your downloads, settings, download history
and saved sign-in are untouched.

To recover:
  1. Close any other copy of Douyin Downloader, then start this one again.
  2. If it still will not start, restart the computer. That clears any WebView2
     process still holding the profile.

If neither helps, the profile at
%LOCALAPPDATA%\\com.douyindownloader.desktop\\EBWebView may be damaged. Renaming
that folder -- not deleting it -- is safe and keeps it recoverable.";

/// Whether a panic message is this application failing to open its window
/// because the WebView2 profile is held elsewhere.
///
/// Case-insensitive on the status, because `HRESULT` is printed in upper case by
/// one layer and could be printed in lower case by another.
pub fn is_profile_in_use(panic_message: &str) -> bool {
    panic_message.contains(CREATE_FAILED)
        && panic_message
            .to_ascii_lowercase()
            .contains(&ERROR_INVALID_STATE.to_ascii_lowercase())
}

/// What to print for a startup panic, or `None` to let the default hook run.
///
/// Returns `Some` only for the failure above. Everything else — a genuine bug in
/// this program, an out-of-memory abort — must keep whatever the default hook
/// says, backtrace note and all, because that is written for whoever has to
/// debug it.
pub fn explanation_for(panic_message: &str) -> Option<&'static str> {
    is_profile_in_use(panic_message).then_some(PROFILE_IN_USE_HELP)
}

/// The text of a panic payload, when it carries one.
pub fn panic_text(payload: &(dyn std::any::Any + Send)) -> Option<&str> {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
}

/// Turn that one startup panic into an explanation; leave every other panic be.
///
/// Installed before the application is built, because that is when Tauri may
/// panic. A process that cannot open its window has nothing to run afterwards,
/// so the hook ends the process rather than letting it unwind into a state with
/// no window and a half-started runtime.
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = panic_text(info.payload());
        match message.and_then(explanation_for) {
            Some(help) => {
                eprintln!("{help}");
                std::process::exit(1);
            }
            None => default_hook(info),
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panic this actually produces, copied from a real run.
    const REAL_PANIC: &str = "Failed to setup app: runtime error: failed to create webview: \
        WebView2 error: WindowsError(Error { code: HRESULT(0x8007139F), message: \
        \"The group or resource is not in the correct state to perform the requested operation.\" })";

    #[test]
    fn the_failure_this_was_written_for_is_recognised() {
        assert!(is_profile_in_use(REAL_PANIC));
        assert!(explanation_for(REAL_PANIC).is_some());
    }

    #[test]
    fn the_help_says_what_happened_what_survived_and_what_to_do() {
        let help = explanation_for(REAL_PANIC).expect("recognised");
        // Names the cause, so it is not a shrug.
        assert!(help.contains("WebView2 profile"), "{help}");
        // Says nothing was lost, because that is the first thing a user fears.
        assert!(help.contains("has been changed or deleted"), "{help}");
        // And gives an ordered remedy.
        assert!(help.contains("Close any other copy"), "{help}");
        assert!(help.contains("restart the computer"), "{help}");
        // The last resort is a rename, never a delete.
        assert!(help.contains("Renaming"), "{help}");
        assert!(help.contains("not deleting"), "{help}");
    }

    #[test]
    fn an_ordinary_panic_is_left_to_the_default_hook() {
        // A bug in this program must keep its backtrace note and its location;
        // swallowing it would make the program harder to fix, not easier to use.
        assert!(explanation_for("index out of bounds: the len is 3 but the index is 5").is_none());
        assert!(explanation_for("called `Option::unwrap()` on a `None` value").is_none());
    }

    #[test]
    fn a_different_webview_failure_is_not_mistaken_for_this_one() {
        // Same window, same phrase, different cause: the profile is fine and the
        // runtime is simply missing. Telling the user to close another copy
        // would send them after something that is not there.
        let missing_runtime = "Failed to setup app: runtime error: failed to create webview: \
            WebView2 error: WindowsError(Error { code: HRESULT(0x80070490), message: \
            \"Element not found.\" })";
        assert!(!is_profile_in_use(missing_runtime));
        assert!(explanation_for(missing_runtime).is_none());
    }

    #[test]
    fn the_status_alone_is_not_enough() {
        // The same status can come from anywhere; without the webview-creation
        // phrase this must not claim to know what happened.
        assert!(!is_profile_in_use("database error: HRESULT(0x8007139F)"));
    }

    #[test]
    fn the_status_is_matched_however_it_is_cased() {
        let lower = REAL_PANIC.to_ascii_lowercase();
        assert!(is_profile_in_use(&lower));
    }

    #[test]
    fn a_panic_payload_is_read_in_both_of_the_shapes_rust_uses() {
        // `panic!("literal")` and `panic!("{}", value)` produce different types,
        // and only one of them is a `&str`.
        let borrowed: &(dyn std::any::Any + Send) = &"a literal";
        assert_eq!(panic_text(borrowed), Some("a literal"));

        let owned: &(dyn std::any::Any + Send) = &String::from("formatted");
        assert_eq!(panic_text(owned), Some("formatted"));

        let unusual: &(dyn std::any::Any + Send) = &42u32;
        assert_eq!(panic_text(unusual), None);
    }
}
