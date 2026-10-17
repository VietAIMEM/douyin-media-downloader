//! Error-model integration tests.
//!
//! The error model has one job that is easy to get wrong and expensive when it
//! is: it must carry enough detail for a developer to diagnose a failure while
//! showing a user nothing but a safe, actionable sentence. These tests pin that
//! separation down, and pin the wire format the frontend branches on.

use std::io;

use douyin_downloader_lib::errors::{to_command_error, AppError, AppErrorKind};

/// Every kind, paired with the code the frontend matches on.
const ALL_KINDS: &[(AppErrorKind, &str)] = &[
    (AppErrorKind::Validation, "validation_error"),
    (AppErrorKind::Network, "network_error"),
    (AppErrorKind::Resolution, "resolution_error"),
    (AppErrorKind::Download, "download_error"),
    (AppErrorKind::Storage, "storage_error"),
    (AppErrorKind::Database, "database_error"),
    (AppErrorKind::UnsupportedContent, "unsupported_content"),
    (AppErrorKind::UnavailableContent, "unavailable_content"),
    (AppErrorKind::NotFound, "not_found"),
    (AppErrorKind::Unknown, "unknown_error"),
];

#[test]
fn every_kind_has_a_stable_code_and_a_default_user_message() {
    for (kind, code) in ALL_KINDS {
        assert_eq!(kind.code(), *code, "code for {kind:?}");

        let message = kind.default_user_message();
        assert!(!message.trim().is_empty(), "{kind:?} needs a default message");
        // A default message must be a sentence a user can read.
        assert!(
            message.chars().next().unwrap().is_uppercase(),
            "{kind:?} default message should read as a sentence: {message}"
        );
    }
}

#[test]
fn codes_are_unique_so_the_frontend_can_branch_on_them() {
    let mut codes: Vec<&str> = ALL_KINDS.iter().map(|(_, code)| *code).collect();
    codes.sort_unstable();
    let unique = codes.len();
    codes.dedup();
    assert_eq!(codes.len(), unique, "two error kinds share a code");
}

#[test]
fn an_error_without_an_override_falls_back_to_the_kind_message() {
    let err = AppError::network("connection timed out after 30s");

    assert_eq!(err.kind(), AppErrorKind::Network);
    assert_eq!(err.user_message(), AppErrorKind::Network.default_user_message());
    // The technical message is preserved for logs.
    assert_eq!(err.message(), "connection timed out after 30s");
}

#[test]
fn an_explicit_user_message_wins_over_the_default() {
    let err = AppError::validation("host `example.com` is not supported")
        .with_user_message("Only Douyin links are supported.");

    assert_eq!(err.user_message(), "Only Douyin links are supported.");
    assert!(
        !err.user_message().contains("example.com"),
        "the user message must not carry the internal phrasing"
    );
}

#[test]
fn technical_detail_is_kept_separate_from_the_user_message() {
    let err = AppError::storage("could not create directory")
        .with_user_message("The application could not prepare its data folder.")
        .with_technical_detail("C:/Users/someone/AppData/Roaming/DouyinDownloader")
        .with_source(io::Error::new(io::ErrorKind::PermissionDenied, "access denied"));

    // Developer side.
    assert_eq!(
        err.technical_detail(),
        Some("C:/Users/someone/AppData/Roaming/DouyinDownloader")
    );
    assert!(err.message().contains("could not create directory"));
    assert!(
        std::error::Error::source(&err).is_some(),
        "the originating error must stay in the chain for logging"
    );

    // User side.
    let serialized = err.to_serialized();
    assert_eq!(serialized.message, "The application could not prepare its data folder.");
    assert!(!serialized.message.contains("AppData"));
    assert!(!serialized.message.contains("access denied"));
    assert!(!serialized.message.contains("permission"));
}

#[test]
fn the_serialized_form_exposes_technical_detail_only_in_debug_builds() {
    let err = AppError::database("PRAGMA failed")
        .with_user_message("The local database could not be updated.")
        .with_technical_detail("sqlite3 errcode 11: database disk image is malformed");

    let serialized = err.to_serialized();
    assert_eq!(serialized.kind, AppErrorKind::Database);
    assert_eq!(serialized.code, "database_error");

    if cfg!(debug_assertions) {
        let details = serialized.details.as_deref().expect("debug builds carry detail");
        assert!(details.contains("malformed"));
        // ...and it is never the message the user would see.
        assert_ne!(serialized.message, details);
    } else {
        assert!(
            serialized.details.is_none(),
            "release builds must not ship internal detail"
        );
    }
}

#[test]
fn the_wire_format_is_the_shape_the_frontend_declares() {
    let err = AppError::validation("empty URL")
        .with_user_message("Paste a Douyin link to continue.")
        .with_technical_detail("input_len=0");

    let value = serde_json::to_value(err.to_serialized()).expect("serializes");

    assert_eq!(value["kind"], "validation");
    assert_eq!(value["code"], "validation_error");
    assert_eq!(value["message"], "Paste a Douyin link to continue.");
    // `kind` is the snake_case enum value, not the code with `_error` appended;
    // the frontend's union type depends on that distinction.
    assert_ne!(value["kind"], value["code"]);

    if !cfg!(debug_assertions) {
        assert!(
            value.get("details").is_none(),
            "the field is omitted entirely, not null"
        );
    }
}

#[test]
fn the_command_conversion_preserves_kind_and_code() {
    // This is the function every Tauri command uses on its error path.
    let err = AppError::unsupported("unsupported host `example.com`");
    let serialized = to_command_error(err);

    assert_eq!(serialized.kind, AppErrorKind::UnsupportedContent);
    assert_eq!(serialized.code, "unsupported_content");
    assert_eq!(
        serialized.message,
        AppErrorKind::UnsupportedContent.default_user_message()
    );
}

#[test]
fn errors_display_as_a_code_and_a_technical_message_for_logs() {
    let err = AppError::download("no space left on device");
    let rendered = err.to_string();

    assert!(rendered.contains("download_error"));
    assert!(rendered.contains("no space left on device"));

    let serialized = err.to_serialized();
    assert_eq!(serialized.to_string(), format!("{}: {}", serialized.code, serialized.message));
}

#[test]
fn not_found_names_the_entity_to_help_a_developer_without_naming_it_to_the_user() {
    let err = AppError::not_found("post", "7300000000000000000");

    assert_eq!(err.kind(), AppErrorKind::NotFound);
    assert!(
        err.message().contains("7300000000000000000"),
        "the id is useful in a log line"
    );
    assert!(
        !err.user_message().contains("7300000000000000000"),
        "an internal id is not useful to a user: {}",
        err.user_message()
    );
}

// --- how a message reads -----------------------------------------------------
//
// A message is written in source that wraps at eighty columns, and a wrap inside
// a string literal used to leave its indentation *in the sentence*: a long run
// of spaces in the middle of a sentence the user reads. Five shipped that way.
// The wrap is not the problem; wrapping without the line continuation is.

/// Decode a Rust string literal the way the compiler does.
///
/// Only the part that matters here is modelled: a backslash at the end of a line
/// swallows the newline **and the indentation that follows it**. That detail is
/// the whole point — it is what separates a wrapped message from a broken one,
/// and a check on the raw source text could not tell them apart.
fn decode(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\n') => {
                while matches!(chars.peek(), Some(' ') | Some('\t')) {
                    chars.next();
                }
            }
            Some('\r') => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                while matches!(chars.peek(), Some(' ') | Some('\t')) {
                    chars.next();
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// The text of the first string literal `haystack` starts with, if it is a
/// literal at all.
fn first_literal(haystack: &str) -> Option<String> {
    let open = haystack.find('"')?;
    // A `)` before the first quote means the argument is a variable or a call,
    // not a literal this check can read.
    if haystack[..open].contains(')') {
        return None;
    }
    let mut escaped = false;
    for (offset, c) in haystack[open + 1..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => return Some(decode(&haystack[open + 1..open + 1 + offset])),
            _ => {}
        }
    }
    None
}

/// Every `.rs` file under `src/`, with production code only.
///
/// `#[cfg(test)]` is the conventional marker for the test module at the end of a
/// file; a panic message inside one is read by whoever is debugging, not by a
/// user, and is not this check's business.
fn production_sources() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read src").filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, found);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&root, &mut files);
    assert!(!files.is_empty(), "no sources found under {root:?}");

    files
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).expect("read source");
            let production = match text.find("\n#[cfg(test)]") {
                Some(cut) => text[..cut].to_string(),
                None => text,
            };
            (path.to_string_lossy().into_owned(), production)
        })
        .collect()
}

#[test]
fn no_user_facing_message_carries_the_indentation_of_its_own_line_wrap() {
    let mut checked = 0;
    let mut offenders = Vec::new();

    for (path, production) in production_sources() {
        let mut rest = production.as_str();
        while let Some(at) = rest.find(".with_user_message(") {
            rest = &rest[at + ".with_user_message(".len()..];
            let Some(message) = first_literal(rest) else {
                continue;
            };
            checked += 1;
            if message.contains("   ") {
                offenders.push(format!("{path}: {message:?}"));
            }
        }
    }

    assert!(checked > 20, "only {checked} messages were found; the scan is broken, not the code");
    assert!(
        offenders.is_empty(),
        "a user-facing message runs its own indentation into the sentence:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn every_kind_reads_as_one_clean_sentence() {
    // The behavioural half: whatever the source looks like, this is the text a
    // user is actually shown for each kind of failure.
    let kinds = [
        AppErrorKind::Validation,
        AppErrorKind::Network,
        AppErrorKind::Resolution,
        AppErrorKind::Download,
        AppErrorKind::Storage,
        AppErrorKind::Database,
        AppErrorKind::UnsupportedContent,
        AppErrorKind::UnavailableContent,
        AppErrorKind::BrowserUnavailable,
        AppErrorKind::MuxerUnavailable,
        AppErrorKind::AuthenticationRequired,
        AppErrorKind::NotFound,
        AppErrorKind::Unknown,
    ];

    for kind in kinds {
        let message = kind.default_user_message();
        assert!(!message.trim().is_empty(), "{kind:?} has no message");
        assert!(
            !message.contains("  "),
            "{kind:?} reads with a gap in it: {message:?}"
        );
        assert_eq!(message.trim(), message, "{kind:?} is padded: {message:?}");
        assert!(
            message.ends_with('.') || message.ends_with('!') || message.ends_with('?'),
            "{kind:?} is not a sentence: {message:?}"
        );
    }
}
