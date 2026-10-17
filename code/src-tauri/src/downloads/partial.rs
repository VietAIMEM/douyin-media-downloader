//! Resource identity for a partial download.
//!
//! A `.part` file only means "some bytes of *something*". Correct offsets and a
//! matching total do not prove that the bytes on disk and the bytes the source is
//! about to send belong to the same version of the resource: if the content at
//! the URL changed while a task was paused — with the same length, which CDN
//! re-encodes can easily be — a resumed transfer would splice two versions into
//! one file and call it a download.
//!
//! # The policy
//!
//! When the transfer begins, whatever validators the response carried are saved
//! beside the partial. When a resume is offered, the request carries `If-Range`
//! with the stored validator, so the *server* decides whether the range is safe
//! — that is what `If-Range` is for, and it is authoritative where a local
//! comparison is a guess. `If-Range` accepts either an ETag or a `Last-Modified`
//! date, and only over `http`/`https`.
//!
//! A `206` to an `If-Range` request is therefore the server's own guarantee that
//! the entity is unchanged — with one caveat that is checked rather than trusted:
//! some origins answer `206` without honouring `If-Range`. So when the response
//! also carries a validator, it is compared against the stored one; a mismatch
//! discards the partial. The starting offset and the resource total are checked
//! against what the partial was written under as well, which is what catches a
//! response that carries no validator at all and contradicts the size the
//! partial was fetched under.
//!
//! A response with **no** validator — no ETag, no `Last-Modified` — establishes
//! nothing. The partial is discarded and the transfer restarts from zero: a
//! clean re-download costs a fraction of what accepting the partial could cost,
//! and "probably the same file" is not a property a download can be built on.
//!
//! # Only a strong validator counts
//!
//! Weak `ETag`s (`W/"…"`) are **not** stored — see
//! [`ResourceValidator::can_authorize_a_range`]. A weak tag compares equal for
//! representations that are merely semantically equivalent, so two
//! byte-different encodings share one, and `If-Range` is defined not to accept
//! one at all. Where an origin offers a weak tag *and* a `Last-Modified`, the
//! date is used instead; where it offers only the weak tag, the partial is not
//! resumable.
//!
//! `Last-Modified` is accepted **only when it can be deduced to be strong**, and
//! the deduction is the one RFC 9110 §8.8.2.2 allows a client: the response's
//! own `Date`, at least a second later than the `Last-Modified`. Both fields come
//! from the same origin and so share a clock, and that gap rules out a second
//! modification inside the second the date names — because one of those two
//! responses would then have carried a `Date` equal to its `Last-Modified`. See
//! [`last_modified_is_strong`].
//!
//! Where that deduction cannot be made — no `Date`, an unreadable one, or one
//! inside the same second — the date is *implicitly weak*, and an implicitly
//! weak validator cannot carry a resume. The test is applied to every response,
//! including the `206` that answers a resume, so an ambiguous date is refused in
//! both directions rather than only the first. The residue is what the deduction
//! cannot reach: an origin whose clock is wrong, or that republishes a
//! representation without moving its `Last-Modified`, defeats it. That is why a
//! strong `ETag` is preferred and why an origin offering one never depends on
//! any of this.
//!
//! # What is stored, and what is not
//!
//! The validator value and the resource's total size. The URL is *not* stored:
//! the task already holds it, and a signed media URL's query string is a
//! credential that must not be duplicated into another file or logged. The
//! metadata lives next to the partial and is removed with it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which header a validator came from, and what it is worth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResourceValidator {
    /// `ETag`, and only when it is *strong*.
    ///
    /// A weak tag is never stored: see [`ResourceValidator::can_authorize_a_range`].
    ETag { value: String },
    /// `Last-Modified`, and only when it can be shown to be *strong*.
    ///
    /// The weaker half of the pair. A date is implicitly weak; it becomes usable
    /// here only through the deduction in [`last_modified_is_strong`], which
    /// needs a `Date` at least a second later in the same response. Where that
    /// cannot be established the date is not stored at all, and the partial is
    /// not resumable — the alternative, resuming on a date nobody can vouch for,
    /// is how two versions end up in one file.
    LastModified { value: String },
}

/// Whether an `ETag` header value is marked weak (`W/"…"`).
///
/// Compared bytewise rather than sliced: the value comes off the wire and
/// slicing a `&str` at a fixed offset panics on a multi-byte boundary. The
/// `W/` prefix is case-sensitive in RFC 9110, but an origin that writes `w/`
/// means the same thing, and this is a check that must not fail open.
fn is_weak_etag(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].eq_ignore_ascii_case(&b'W') && bytes[1] == b'/'
}

/// Parse an HTTP-date.
///
/// The two obsolete formats RFC 9110 still requires a *recipient* to accept are
/// deliberately not handled: they are vanishingly rare, and refusing one costs a
/// restart rather than an unsound resume.
fn parse_http_date(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc2822(value.trim())
        .ok()
        .map(|parsed| parsed.with_timezone(&chrono::Utc))
}

/// Whether a `Last-Modified` may be treated as a *strong* validator.
///
/// RFC 9110 §8.8.2.2 is explicit that a date used as a validator is
/// "implicitly weak unless it is possible to deduce that it is strong", and the
/// only deduction open to a client is the response's own `Date`: the two fields
/// come from the same origin, so they share a clock, and a `Date` at least a
/// second later than the `Last-Modified` rules out a second modification inside
/// the second the date names — because one of those two responses would then
/// have carried a `Date` equal to its `Last-Modified`.
///
/// Anything else leaves it implicitly weak: no `Date`, an unreadable one, or one
/// inside the same second. A weak validator cannot carry a resume, and §13.1.5
/// says so directly — an `If-Range` carrying a date is only permitted when the
/// date is strong in this sense.
fn last_modified_is_strong(last_modified: &str, date: Option<&str>) -> bool {
    let (Some(modified), Some(date)) = (
        parse_http_date(last_modified),
        date.and_then(parse_http_date),
    ) else {
        return false;
    };
    date.signed_duration_since(modified) >= chrono::Duration::seconds(1)
}

impl ResourceValidator {
    /// The header name to send this validator in, for `If-Range`.
    pub fn header_name(&self) -> &'static str {
        match self {
            Self::ETag { .. } => "if-range",
            Self::LastModified { .. } => "if-range",
        }
    }

    /// The validator value as it goes on the wire.
    pub fn value(&self) -> &str {
        match self {
            Self::ETag { value } | Self::LastModified { value } => value,
        }
    }

    /// A short word for logs: the *type* is useful, the value is not.
    ///
    /// An ETag can be a hash of the content and a `Last-Modified` date is not
    /// sensitive — but neither belongs in a log line that also identifies the
    /// task, when the type is what explains the decision.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ETag { .. } => "etag",
            Self::LastModified { .. } => "last-modified",
        }
    }

    /// Whether this validator may authorize appending to an existing partial.
    ///
    /// `If-Range` requires a **strong** validator, and the reason is the whole
    /// safety argument: a weak tag is defined to compare equal for
    /// representations that are merely *semantically* equivalent — two
    /// byte-different encodings of the same content share one. Appending across
    /// that comparison would splice two byte sequences into one file and call
    /// it a download.
    ///
    /// It is not only our own comparison that matters. RFC 9110 §13.1.5 forbids
    /// a client from putting a weak tag in `If-Range` at all, and requires a
    /// server that receives one to *ignore the condition* — which would leave
    /// the range entirely unprotected while looking, from here, exactly like a
    /// server that honoured it.
    ///
    /// So a weak tag is not a validator: the partial cannot be attributed to a
    /// resource and the transfer restarts from zero.
    pub fn can_authorize_a_range(&self) -> bool {
        match self {
            Self::ETag { value } => !is_weak_etag(value),
            Self::LastModified { value } => !value.trim().is_empty(),
        }
    }

    /// Read a validator out of a response, preferring the strong form.
    ///
    /// An `ETag` outranks `Last-Modified`: it is defined to change when the
    /// entity changes, where a timestamp can lie about second-scale changes.
    /// A *weak* `ETag` does not outrank it — it is not usable for a
    /// continuation at all, so the response's `Last-Modified` is taken instead
    /// of storing something that could only mislead.
    ///
    /// A `Last-Modified` is taken only when it can be *deduced* to be strong;
    /// see [`last_modified_is_strong`]. The test is applied to every response,
    /// including the `206` that answers a resume, so a date that is fresh enough
    /// to be ambiguous is refused in both directions rather than only the first.
    ///
    /// `None` is not "no opinion" — it is the answer that a partial cannot be
    /// attributed to this response, and the caller restarts.
    pub fn from_response(response: &reqwest::Response) -> Option<Self> {
        let header = |name: reqwest::header::HeaderName| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        if let Some(value) = header(reqwest::header::ETAG) {
            if !is_weak_etag(&value) {
                return Some(Self::ETag { value });
            }
        }
        if let Some(value) = header(reqwest::header::LAST_MODIFIED) {
            let date = header(reqwest::header::DATE);
            if last_modified_is_strong(&value, date.as_deref()) {
                return Some(Self::LastModified { value });
            }
        }
        None
    }

    /// Compare against a validator read from a later response.
    ///
    /// `None` means "cannot be compared" — different kinds, or the later
    /// response carried nothing. That is deliberately *not* a match: it is the
    /// caller's job to decide what a comparison that cannot be made means, and
    /// this module's policy is that it means restart.
    pub fn matches(&self, other: Option<&Self>) -> Option<bool> {
        match (self, other) {
            (_, None) => None,
            (Self::ETag { value: a }, Some(Self::ETag { value: b })) => Some(a == b),
            (
                Self::LastModified { value: a },
                Some(Self::LastModified { value: b }),
            ) => Some(a == b),
            _ => None,
        }
    }
}

/// The stored side of the contract: a partial file and what it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialMetadata {
    pub validator: Option<ResourceValidator>,
    /// The resource's total size, when the source stated it.
    pub total_bytes: Option<u64>,
    /// The offset this attempt started writing at — 0 for a fresh transfer,
    /// the resume offset for a continued one. Informational: the file's live
    /// length is read from the file itself, and it grows while the transfer
    /// runs.
    pub offset_bytes: u64,
}

impl PartialMetadata {
    /// Read the metadata that accompanies `partial`.
    ///
    /// `None` covers every way this can fail — no file, malformed JSON — and
    /// every one of them means the same thing: the partial's identity is not
    /// established, so it may not be resumed.
    pub fn read(partial: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(metadata_path(partial)).ok()?;
        // Malformed metadata refuses to authorize anything: the caller
        // restarts cleanly.
        serde_json::from_str(&raw).ok()
    }

    /// Write metadata for `partial`, atomically.
    ///
    /// Written through a sibling temporary file and renamed, so a crash cannot
    /// leave a half-written JSON file that [`Self::read`] would silently treat
    /// as "no metadata" — and thereby authorize a restart that would be safe
    /// anyway, but would also mask the fact that the metadata was unreadable.
    pub async fn write(&self, partial: &Path) -> std::io::Result<()> {
        let target = metadata_path(partial);
        let staging = target.with_extension("meta.tmp");
        let body = serde_json::to_string(self).map_err(std::io::Error::other)?;
        tokio::fs::write(&staging, body).await?;
        tokio::fs::rename(&staging, &target).await
    }

    /// Whether this metadata authorizes continuing a partial of `actual_len`
    /// bytes.
    ///
    /// The validator is the identity, so it must exist. The file's length is
    /// the offset a resume would continue from, and it grows while the
    /// transfer runs — that is not drift, so it is *not* required to equal
    /// `offset_bytes` (which records where this attempt started writing). What
    /// would be drift: a file longer than the resource itself, which is
    /// corruption no validator can explain away.
    /// Because a sidecar outlives the build that wrote it, the validator is
    /// re-checked for *usability* here rather than only being screened when it
    /// was read off a response. A sidecar written before weak tags were refused
    /// is still on disk somewhere, and it must not authorize anything.
    pub fn authorizes(&self, actual_len: u64) -> bool {
        if !self
            .validator
            .as_ref()
            .is_some_and(ResourceValidator::can_authorize_a_range)
        {
            return false;
        }
        match self.total_bytes {
            Some(total) => actual_len <= total,
            None => true,
        }
    }
}

/// Where the metadata for a partial lives.
fn metadata_path(partial: &Path) -> PathBuf {
    let mut name = partial
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.push_str(".meta.json");
    partial.with_file_name(name)
}

/// Remove a partial file and its metadata.
pub async fn remove_partial_and_metadata(partial: &Path) {
    let _ = crate::network::remove_partial_file(partial).await;
    let _ = tokio::fs::remove_file(metadata_path(partial)).await;
}

/// Remove just the metadata, when the partial itself is being kept.
pub async fn remove_metadata(partial: &Path) {
    let _ = tokio::fs::remove_file(metadata_path(partial)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn metadata_round_trips_through_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let partial = temp.path().join("clip.mp4.part");
        std::fs::write(&partial, b"partial-bytes").unwrap();

        let metadata = PartialMetadata {
            validator: Some(ResourceValidator::ETag {
                value: "\"abc123\"".into(),
            }),
            total_bytes: Some(1024),
            offset_bytes: 12,
        };
        metadata.write(&partial).await.unwrap();

        let read_back = PartialMetadata::read(&partial).expect("readable");
        assert_eq!(read_back, metadata);
    }

    #[tokio::test]
    async fn metadata_prefers_the_etag_and_falls_back_to_last_modified() {
        // A response with both: the ETag wins.
        let both = http::Response::builder()
            .header("etag", "\"strong\"")
            .header("last-modified", "Wed, 21 Oct 2026 07:28:00 GMT")
            .body(reqwest::Body::from(""))
            .unwrap();
        let response = reqwest::Response::from(both);
        assert_eq!(
            ResourceValidator::from_response(&response),
            Some(ResourceValidator::ETag {
                value: "\"strong\"".into()
            })
        );

        // A response with neither: nothing is claimed.
        let neither = http::Response::builder().body(reqwest::Body::from("")).unwrap();
        let response = reqwest::Response::from(neither);
        assert_eq!(ResourceValidator::from_response(&response), None);

        // A timestamp that the response's own `Date` establishes as strong, with
        // no ETag to prefer: the date is taken.
        let stamp_only = http::Response::builder()
            .header("last-modified", "Wed, 21 Oct 2026 07:28:00 GMT")
            .header("date", "Wed, 21 Oct 2026 07:28:01 GMT")
            .body(reqwest::Body::from(""))
            .unwrap();
        let response = reqwest::Response::from(stamp_only);
        assert_eq!(
            ResourceValidator::from_response(&response),
            Some(ResourceValidator::LastModified {
                value: "Wed, 21 Oct 2026 07:28:00 GMT".into()
            })
        );
    }

    // --- how strong a Last-Modified has to be --------------------------------

    /// A response carrying only a `Last-Modified`, with `date` beside it.
    fn dated_response(last_modified: &str, date: Option<&str>) -> reqwest::Response {
        let mut builder = http::Response::builder().header("last-modified", last_modified);
        if let Some(date) = date {
            builder = builder.header("date", date);
        }
        reqwest::Response::from(builder.body(reqwest::Body::from("")).unwrap())
    }

    const MODIFIED: &str = "Wed, 21 Oct 2026 07:28:00 GMT";

    #[test]
    fn a_date_a_second_later_makes_a_last_modified_strong() {
        // The deduction RFC 9110 §8.8.2.2 allows a client: both fields came from
        // the same origin, so they share a clock, and a `Date` at least a second
        // past the `Last-Modified` rules out a second modification inside the
        // second the date names.
        assert_eq!(
            ResourceValidator::from_response(&dated_response(
                MODIFIED,
                Some("Wed, 21 Oct 2026 07:28:01 GMT")
            )),
            Some(ResourceValidator::LastModified {
                value: MODIFIED.into()
            })
        );
        // And a wide gap is just as good.
        assert!(ResourceValidator::from_response(&dated_response(
            MODIFIED,
            Some("Thu, 22 Oct 2026 09:15:00 GMT")
        ))
        .is_some());
    }

    #[test]
    fn a_last_modified_that_cannot_be_shown_strong_is_not_a_validator() {
        // No `Date` at all: nothing to deduce from.
        assert_eq!(ResourceValidator::from_response(&dated_response(MODIFIED, None)), None);
        // The same second: the ambiguous case the rule exists for — two
        // responses one second apart would carry equal `Date` and
        // `Last-Modified`, and could not be told apart.
        assert_eq!(
            ResourceValidator::from_response(&dated_response(
                MODIFIED,
                Some("Wed, 21 Oct 2026 07:28:00 GMT")
            )),
            None
        );
        // A `Date` *before* the `Last-Modified` is nonsense, and nonsense is not
        // a stronger claim than silence.
        assert_eq!(
            ResourceValidator::from_response(&dated_response(
                MODIFIED,
                Some("Wed, 21 Oct 2026 07:27:00 GMT")
            )),
            None
        );
        // An unreadable date cannot be compared to anything.
        assert_eq!(
            ResourceValidator::from_response(&dated_response(MODIFIED, Some("not a date"))),
            None
        );
        assert_eq!(
            ResourceValidator::from_response(&dated_response("not a date", Some("not a date either"))),
            None
        );
    }

    #[test]
    fn a_strong_etag_needs_no_date_beside_it() {
        // The ETag path is unaffected: an entity tag carries its own strength,
        // which is why it is preferred.
        let response = reqwest::Response::from(
            http::Response::builder()
                .header("etag", "\"strong\"")
                .header("last-modified", MODIFIED)
                .body(reqwest::Body::from(""))
                .unwrap(),
        );
        assert_eq!(
            ResourceValidator::from_response(&response),
            Some(ResourceValidator::ETag {
                value: "\"strong\"".into()
            })
        );
    }

    #[tokio::test]
    async fn a_partial_and_its_metadata_are_removed_together() {
        let temp = tempfile::tempdir().unwrap();
        let partial = temp.path().join("a.part");
        std::fs::write(&partial, b"x").unwrap();
        PartialMetadata {
            validator: None,
            total_bytes: None,
            offset_bytes: 1,
        }
        .write(&partial)
        .await
        .unwrap();

        remove_partial_and_metadata(&partial).await;

        assert!(!partial.exists());
        assert!(!metadata_path(&partial).exists());
    }

    #[test]
    fn authorization_requires_identity_and_fits_the_resource() {
        let identified = PartialMetadata {
            validator: Some(ResourceValidator::ETag { value: "\"v1\"".into() }),
            total_bytes: Some(100),
            offset_bytes: 0,
        };
        // A file that grew while the transfer ran is normal, not drift.
        assert!(identified.authorizes(40));
        assert!(identified.authorizes(100), "complete is fine — resume is just a no-op");
        // Longer than the resource: corruption.
        assert!(!identified.authorizes(101));

        // Without a validator the bytes could be anything.
        let anonymous = PartialMetadata {
            validator: None,
            total_bytes: Some(100),
            offset_bytes: 40,
        };
        assert!(!anonymous.authorizes(40));

        // An unknown total cannot bound the file; identity alone decides.
        let unbounded = PartialMetadata {
            validator: Some(ResourceValidator::ETag { value: "\"v1\"".into() }),
            total_bytes: None,
            offset_bytes: 0,
        };
        assert!(unbounded.authorizes(40));
    }

    #[test]
    fn comparison_between_different_kinds_is_not_a_match() {
        let etag = ResourceValidator::ETag {
            value: "\"a\"".into(),
        };
        let stamp = ResourceValidator::LastModified {
            value: "Wed, 21 Oct 2026 07:28:00 GMT".into(),
        };
        assert_eq!(etag.matches(Some(&stamp)), None, "incomparable");
        assert_eq!(etag.matches(None), None, "nothing to compare against");
        assert_eq!(etag.matches(Some(&etag)), Some(true));
    }

    #[test]
    fn the_validator_type_is_logged_but_not_its_value() {
        let etag = ResourceValidator::ETag {
            value: "\"9f86d081884c7d65\"".into(),
        };
        assert_eq!(etag.kind(), "etag");
        let described = format!("{:?}", etag.kind());
        assert!(!described.contains("9f86d081"), "the value must not leak");
    }

    // --- what may authorize a range ------------------------------------------

    /// A response carrying only a weak `ETag`.
    fn weak_etag_response() -> reqwest::Response {
        reqwest::Response::from(
            http::Response::builder()
                .header("etag", "W/\"abc\"")
                .body(reqwest::Body::from(""))
                .unwrap(),
        )
    }

    #[test]
    fn a_weak_etag_is_never_stored_as_a_validator() {
        // Not "stored and then refused": never stored at all, so nothing later
        // can mistake it for an identity.
        assert_eq!(ResourceValidator::from_response(&weak_etag_response()), None);
    }

    #[test]
    fn a_weak_etag_gives_way_to_the_last_modified_beside_it() {
        // A date is the weaker signal, but it is a signal; a weak tag is not.
        // The date still has to clear the strength test, which is why the
        // response carries a `Date` a second later.
        let response = reqwest::Response::from(
            http::Response::builder()
                .header("etag", "W/\"abc\"")
                .header("last-modified", "Wed, 21 Oct 2026 07:28:00 GMT")
                .header("date", "Wed, 21 Oct 2026 07:28:01 GMT")
                .body(reqwest::Body::from(""))
                .unwrap(),
        );
        assert_eq!(
            ResourceValidator::from_response(&response),
            Some(ResourceValidator::LastModified {
                value: "Wed, 21 Oct 2026 07:28:00 GMT".into()
            })
        );

        // And where that date is not strong either, the weak tag does not get a
        // second chance: there is no validator at all.
        let weak_and_ambiguous = reqwest::Response::from(
            http::Response::builder()
                .header("etag", "W/\"abc\"")
                .header("last-modified", "Wed, 21 Oct 2026 07:28:00 GMT")
                .body(reqwest::Body::from(""))
                .unwrap(),
        );
        assert_eq!(
            ResourceValidator::from_response(&weak_and_ambiguous),
            None
        );
    }

    #[test]
    fn the_weaker_spellings_are_treated_as_weak_too() {
        // The prefix is case-sensitive in the specification, but a check that
        // guards a splice must not fail open on the spelling.
        for value in ["w/\"abc\"", "W/\"abc\"", "W/"] {
            assert!(
                !ResourceValidator::ETag { value: value.into() }.can_authorize_a_range(),
                "{value} must not authorize a range"
            );
            assert!(
                ResourceValidator::from_response(&reqwest::Response::from(
                    http::Response::builder()
                        .header("etag", value)
                        .body(reqwest::Body::from(""))
                        .unwrap(),
                ))
                .is_none(),
                "{value} must not be stored"
            );
        }
    }

    #[test]
    fn a_strong_etag_still_authorizes_a_range() {
        let strong = ResourceValidator::ETag {
            value: "\"abc\"".into(),
        };
        assert!(strong.can_authorize_a_range());
        // And a value that merely contains a slash is not weak.
        assert!(ResourceValidator::ETag {
            value: "\"a/b\"".into()
        }
        .can_authorize_a_range());
    }

    #[test]
    fn a_sidecar_naming_a_weak_validator_authorizes_nothing() {
        // The rule has to hold for a sidecar that is already on disk, written by
        // a build that had not learned this yet — the file outlives the code
        // that wrote it.
        let stale = PartialMetadata {
            validator: Some(ResourceValidator::ETag {
                value: "W/\"abc\"".into(),
            }),
            total_bytes: Some(1000),
            offset_bytes: 100,
        };
        assert!(
            !stale.authorizes(100),
            "a weak tag cannot stand as an identity, whenever it was written"
        );

        // A strong tag with the same shape still does.
        let sound = PartialMetadata {
            validator: Some(ResourceValidator::ETag {
                value: "\"abc\"".into(),
            }),
            ..stale
        };
        assert!(sound.authorizes(100));
    }

    #[test]
    fn an_empty_last_modified_is_not_a_validator() {
        assert!(!ResourceValidator::LastModified {
            value: "   ".into()
        }
        .can_authorize_a_range());
    }
}
