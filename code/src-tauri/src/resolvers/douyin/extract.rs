//! Douyin payload parsing and normalization.
//!
//! Douyin's share pages embed a JSON blob under `window._ROUTER_DATA`. When the
//! page server-renders a post, that blob contains a `videoInfoRes.item_list`
//! array whose first element is the aweme (Douyin's word for a post). This
//! module extracts that blob and normalizes the aweme into the application's
//! domain models.
//!
//! # What this module will not do
//!
//! * It never invents a media URL. Every URL in the output came from the
//!   payload; a field that is absent stays `None`.
//! * It never rewrites a media URL to strip a watermark. Douyin's playback
//!   endpoint and its watermarked variant are distinguished by a path marker;
//!   the marker is *classified*, not removed. A watermarked rendition is
//!   reported as watermarked so the UI can say so.
//!
//! Everything here is pure: it takes bytes and returns domain models, so it is
//! fully testable against a saved payload without touching the network.

use serde_json::Value;

use crate::errors::{AppError, AppResult};
use crate::models::{
    ContentStatus, Creator, MediaItem, MediaType, Platform, Post, VariantKind,
};

/// Marker prefix that precedes the embedded router payload.
const ROUTER_DATA_MARKER: &str = "_ROUTER_DATA";

/// Path fragment Douyin uses for the watermarked playback rendition.
///
/// A URL containing this is a watermarked source. It is deliberately *not*
/// rewritten into the clean path — that rewrite is watermark removal, which the
/// application does not do.
const WATERMARK_PATH_MARKER: &str = "playwm";

/// Metadata key under which a resolver stores the normalized creator it derived
/// while resolving a post.
///
/// The value is a serialized [`Creator`] (a domain model, not a platform
/// shape), so the application service can surface creator information without
/// learning anything about Douyin's payload format.
pub const RESOLVED_CREATOR_KEY: &str = "resolved_creator";

/// A post normalized from a Douyin aweme.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedAweme {
    pub post: Post,
    /// Media items belonging to the post, already normalized.
    pub media: Vec<MediaItem>,
    /// Creator information, when the payload carried it.
    pub creator: Option<Creator>,
}

/// Markers that identify an embedded state blob in a Douyin page.
///
/// Douyin has shipped more than one of these over time; which one is present is
/// the first thing worth knowing when a page yields no post.
const STATE_MARKERS: &[&str] = &[
    "_ROUTER_DATA",
    "RENDER_DATA",
    "NEXT_DATA",
    "__INITIAL_STATE__",
];

/// Keys that indicate a payload actually carries a post.
const AWEME_MARKERS: &[&str] = &["videoInfoRes", "item_list", "aweme_detail", "play_addr"];

/// Summarize what a fetched page actually contained.
///
/// Used only for diagnostics: it never becomes user-facing text, and it is
/// bounded so a large or hostile page cannot flood a log line. It exists so that
/// "the source returned no post" can be backed by evidence — which state blob
/// was present, whether any post-bearing key appeared, and how big the page was
/// — instead of being an assertion.
pub fn describe_payload(html: &str) -> String {
    let blobs: Vec<&str> = STATE_MARKERS
        .iter()
        .copied()
        .filter(|marker| html.contains(marker))
        .collect();
    let aweme_keys: Vec<&str> = AWEME_MARKERS
        .iter()
        .copied()
        .filter(|marker| html.contains(marker))
        .collect();

    let sections = parse_router_data(html)
        .ok()
        .and_then(|data| {
            data.get("loaderData")
                .and_then(Value::as_object)
                .map(|loader| {
                    loader
                        .keys()
                        .cloned()
                        .collect::<Vec<String>>()
                        .join(",")
                })
        })
        .unwrap_or_else(|| "-".to_string());

    let parsed = parse_state_blobs(html).len();

    format!(
        "bytes={} state_blobs=[{}] parsed_blobs={} aweme_keys=[{}] loader_sections=[{}]",
        html.len(),
        blobs.join(","),
        parsed,
        aweme_keys.join(","),
        sections
    )
}

/// Extract the `_ROUTER_DATA` JSON object from a share page.
///
/// Returns a `Resolution` error when the marker or a well-formed payload is
/// absent — the page did not carry post data, which is a different situation
/// from the post not existing.
pub fn parse_router_data(html: &str) -> AppResult<Value> {
    let marker_at = html.find(ROUTER_DATA_MARKER).ok_or_else(|| {
        AppError::resolution("the page carried no embedded data")
            .with_user_message(
                "Douyin returned a page this app could not read. Its anti-bot page \
                 replaces the post data with a script this app does not execute.",
            )
            .with_technical_detail("marker=_ROUTER_DATA missing")
    })?;

    let after_marker = &html[marker_at + ROUTER_DATA_MARKER.len()..];
    let json_start = after_marker.find('{').ok_or_else(|| {
        AppError::resolution("embedded data had no JSON object")
            .with_user_message("The source returned an unexpected response.")
    })?;
    let json_text = &after_marker[json_start..];

    let end = balanced_object_end(json_text).ok_or_else(|| {
        AppError::resolution("embedded JSON was truncated")
            .with_user_message("The source returned an incomplete response.")
    })?;

    serde_json::from_str(&json_text[..end]).map_err(|err| {
        AppError::resolution("embedded JSON could not be parsed")
            .with_user_message("The source returned a response this app could not read.")
            .with_technical_detail(err.to_string())
    })
}

/// Find the index just past the `}` that closes the object starting at 0.
///
/// A naive search for the first `}` breaks on braces inside strings — captions
/// routinely contain `{` and `}` — so string literals and their escapes are
/// tracked.
fn balanced_object_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (index, &byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Percent-decode a string, leaving malformed escapes untouched.
///
/// Douyin's `RENDER_DATA` embeds its JSON percent-encoded, so decoding is a
/// precondition for reading it. A hand-rolled decoder keeps this module free of
/// an extra dependency for one small job.
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &input[index + 1..index + 3];
            if let Ok(value) = u8::from_str_radix(hex, 16) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read the contents of a `<script id="...">` element.
fn script_body<'a>(html: &'a str, id: &str) -> Option<&'a str> {
    let marker = format!("id=\"{id}\"");
    let id_at = html.find(&marker)?;
    let open_at = html[id_at..].find('>')? + id_at + 1;
    let close_at = html[open_at..].find("</script>")? + open_at;
    Some(&html[open_at..close_at])
}

/// Parse every embedded state blob the page carries.
///
/// Douyin has shipped its post data under different names over time, so all of
/// the known ones are read rather than betting on one. Blobs that fail to parse
/// are skipped rather than failing the whole page — one broken blob does not
/// mean the page has no data.
pub fn parse_state_blobs(html: &str) -> Vec<Value> {
    let mut blobs = Vec::new();

    // `_ROUTER_DATA` is a plain JSON object assigned to a global.
    if let Ok(value) = parse_router_data(html) {
        blobs.push(value);
    }

    // `RENDER_DATA` is percent-encoded JSON inside a script tag.
    if let Some(body) = script_body(html, "RENDER_DATA") {
        if let Ok(value) = serde_json::from_str::<Value>(&percent_decode(body)) {
            blobs.push(value);
        }
    }

    // Next.js-style payload, if Douyin ever adopts it.
    if let Some(body) = script_body(html, "__NEXT_DATA__") {
        if let Ok(value) = serde_json::from_str::<Value>(body) {
            blobs.push(value);
        }
    }

    // `window.__INITIAL_STATE__ = {...}`.
    if let Some(at) = html.find("__INITIAL_STATE__") {
        if let Some(brace) = html[at..].find('{') {
            let slice = &html[at + brace..];
            if let Some(end) = balanced_object_end(slice) {
                if let Ok(value) = serde_json::from_str::<Value>(&slice[..end]) {
                    blobs.push(value);
                }
            }
        }
    }

    blobs
}

/// Depth limit for the structural search.
///
/// Real payloads nest a handful of levels; the cap keeps a pathological or
/// hostile document from turning the search into a deep recursion.
const MAX_SEARCH_DEPTH: usize = 24;

/// True when an object looks like a Douyin post.
///
/// Deliberately shape-based rather than path-based: it keys on the fields a post
/// must have, so a rename of the *containing* structure does not hide the post.
/// A media field is required too, so a bare reference to an id is not mistaken
/// for the post itself.
fn looks_like_aweme(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.contains_key("aweme_id")
        && (object.contains_key("video")
            || object.contains_key("images")
            || object.contains_key("music"))
}

/// Search a payload for the first object that looks like a post.
pub fn find_aweme(value: &Value) -> Option<&Value> {
    fn walk(node: &Value, depth: usize) -> Option<&Value> {
        if depth > MAX_SEARCH_DEPTH {
            return None;
        }
        if looks_like_aweme(node) {
            return Some(node);
        }
        match node {
            Value::Object(map) => map.values().find_map(|child| walk(child, depth + 1)),
            Value::Array(list) => list.iter().find_map(|child| walk(child, depth + 1)),
            _ => None,
        }
    }
    walk(value, 0)
}

/// Extract a post from whatever state blob a page happens to carry.
///
/// This is the entry point the resolver uses. It tries the structural locations
/// Douyin is known to use first (cheap and precise), then falls back to a
/// structural search of every blob, so a schema move is survivable without a
/// code change.
///
/// Errors keep their meaning:
/// * the page carries no readable state at all → `Resolution` (an anti-bot or
///   unreadable page, not a missing post)
/// * the page carries state but no post → `UnavailableContent`
pub fn find_aweme_in_page(html: &str) -> AppResult<Value> {
    let blobs = parse_state_blobs(html);

    if blobs.is_empty() {
        return Err(AppError::resolution("the page carried no embedded data")
            .with_user_message(
                "Douyin returned a page this app could not read. Its anti-bot page \
                 replaces the post data with a script this app does not execute.",
            )
            .with_technical_detail(format!(
                "no state blob found; {}",
                describe_payload(html)
            )));
    }

    // Known structural paths first.
    for blob in &blobs {
        if let Ok(aweme) = first_aweme(blob) {
            return Ok(aweme.clone());
        }
    }

    // Then a shape-based search of everything, which survives a rename of the
    // containing structure.
    for blob in &blobs {
        if let Some(aweme) = find_aweme(blob) {
            return Ok(aweme.clone());
        }
    }

    Err(AppError::unavailable("the source returned no post data for this link")
        .with_user_message(
            "Douyin did not return this post's data. The video may be private, \
             region-restricted, or removed — Douyin also requires a signed-in session \
             for some content, which this app does not create on your behalf.",
        )
        .with_technical_detail(format!(
            "state blobs present but no post found; {}",
            describe_payload(html)
        )))
}

/// Pull the first aweme object out of a `_ROUTER_DATA` payload.
///
/// Douyin has shipped more than one page shape, so several known locations are
/// tried before giving up. An `UnavailableContent` error means the page loaded
/// but the source declined to include the post — which is what a blocked or
/// region-restricted request looks like, and is not the same as a parse failure.
pub fn first_aweme(router_data: &Value) -> AppResult<&Value> {
    let loader = router_data.get("loaderData").unwrap_or(router_data);

    let loader = loader.as_object().ok_or_else(|| {
        AppError::resolution("embedded payload had an unexpected shape")
            .with_user_message("The source returned a response this app could not read.")
    })?;

    for value in loader.values() {
        // Shape A: `video_(id)/page` → `videoInfoRes.item_list[]`.
        if let Some(items) = value
            .get("videoInfoRes")
            .and_then(|res| res.get("item_list"))
            .and_then(Value::as_array)
        {
            if let Some(first) = items.first().filter(|item| item.is_object()) {
                return Ok(first);
            }
        }
        // Shape B: the aweme sits directly under `aweme_detail`.
        if let Some(detail) = value.get("aweme_detail").filter(|d| d.is_object()) {
            return Ok(detail);
        }
        // Shape C: `videoInfoRes` is the aweme itself.
        if let Some(res) = value.get("videoInfoRes").filter(|d| d.is_object()) {
            if res.get("aweme_id").is_some() {
                return Ok(res);
            }
        }
    }

    Err(AppError::unavailable("the source returned no post data for this link")
        .with_user_message(
            "Douyin did not return this post's data. The video may be private, \
             region-restricted, or removed — Douyin also requires a signed-in session \
             for some content, which this app does not create on your behalf.",
        )
        .with_technical_detail("videoInfoRes.item_list was empty or absent"))
}

/// Normalize one aweme into domain models.
pub fn aweme_to_domain(aweme: &Value) -> AppResult<ParsedAweme> {
    let platform_post_id = string_at(aweme, &["aweme_id"]).ok_or_else(|| {
        AppError::resolution("aweme payload had no identifier")
            .with_user_message("The source returned an unexpected response.")
    })?;

    let images = aweme.get("images").and_then(Value::as_array);
    let is_image_post = images.map(|list| !list.is_empty()).unwrap_or(false);

    let media_type = if is_image_post {
        if images.map(|l| l.len() > 1).unwrap_or(false) {
            MediaType::Carousel
        } else {
            MediaType::Image
        }
    } else {
        MediaType::Video
    };

    let mut post = Post::new(
        Platform::Douyin,
        platform_post_id.clone(),
        media_type,
        format!("https://www.douyin.com/video/{platform_post_id}"),
    );
    post.canonical_url = Some(format!("https://www.douyin.com/video/{platform_post_id}"));
    post.caption = string_at(aweme, &["desc"]).filter(|d| !d.trim().is_empty());
    post.thumbnail_url = thumbnail_url(aweme);
    post.published_at = integer_at(aweme, &["create_time"])
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0));
    post.creator_handle = creator_handle(aweme);
    post.status = ContentStatus::Ready;

    let creator = creator_from_aweme(aweme, &platform_post_id);
    if let Some(creator) = &creator {
        post.creator_handle = Some(
            creator
                .handle
                .clone()
                .unwrap_or_else(|| creator.display_name.clone()),
        );
        post.metadata.insert(
            RESOLVED_CREATOR_KEY.to_string(),
            serde_json::to_value(creator).map_err(|err| {
                AppError::unknown("could not serialize the resolved creator")
                    .with_technical_detail(err.to_string())
            })?,
        );
    }

    let media = if is_image_post {
        image_items(&mut post, images.expect("checked non-empty"))
    } else {
        video_items(&mut post, aweme)?
    };

    if media.is_empty() {
        return Err(AppError::unsupported(
            "the post contained no downloadable media",
        )
        .with_user_message("This post does not contain downloadable media.")
        .with_technical_detail(format!("aweme_id={platform_post_id}")));
    }

    // The post's soundtrack, as metadata — whether or not there is anything to
    // download for it.
    //
    // A post carries a music block far more often than it carries a *playable*
    // one: the block names the track and credits its author, and the URL list is
    // empty or absent. Dropping the whole thing because there is no URL threw
    // away a fact the source stated. Recording it without a URL keeps the post's
    // own description of its soundtrack while giving the download path nothing
    // to invent a task from — `downloadable` says which of the two this is, and
    // the UI shows the credit either way without offering a file that does not
    // exist.
    let track = audio_item(&post, aweme);
    if let Some(soundtrack) = soundtrack_metadata(aweme, track.is_some()) {
        post.metadata.insert("soundtrack".into(), soundtrack);
    }

    // Attach a separate audio item when the post exposes a music track, so the
    // "Audio" variant the brief describes is real rather than synthesized.
    if let Some(audio) = track {
        post.metadata
            .insert("media_count".into(), serde_json::json!(media.len() + 1));
        let mut all = media;
        all.push(audio);
        return Ok(ParsedAweme {
            post,
            media: all,
            creator,
        });
    }

    post.metadata
        .insert("media_count".into(), serde_json::json!(media.len()));
    Ok(ParsedAweme {
        post,
        media,
        creator,
    })
}

/// Build the video media item (with its variants) for a video aweme.
fn video_items(post: &mut Post, aweme: &Value) -> AppResult<Vec<MediaItem>> {
    let video = match aweme.get("video").filter(|v| v.is_object()) {
        Some(video) => video,
        // Some image-less payloads are "note" posts with neither images nor
        // video; that is content we cannot serve.
        None => return Ok(Vec::new()),
    };

    let mut variants: Vec<MediaVariantSource> = Vec::new();

    // The playback source Douyin offers for streaming. Whether it is clean or
    // watermarked is decided by the URL marker, never by rewriting it.
    if let Some(url) = first_url_in(video, &["play_addr", "url_list"]) {
        let watermarked = is_watermarked(&url);
        variants.push(MediaVariantSource {
            kind: if watermarked {
                VariantKind::Standard
            } else {
                VariantKind::Original
            },
            label: if watermarked {
                "Standard".to_string()
            } else {
                "Original / Clean".to_string()
            },
            is_original: !watermarked,
            source_url: url,
            width: integer_at(video.get("play_addr").unwrap_or(&Value::Null), &["width"])
                .and_then(|v| u32::try_from(v).ok()),
            height: integer_at(video.get("play_addr").unwrap_or(&Value::Null), &["height"])
                .and_then(|v| u32::try_from(v).ok()),
            file_size: integer_at(video.get("play_addr").unwrap_or(&Value::Null), &["data_size"])
                .and_then(|v| u64::try_from(v).ok()),
            bitrate_kbps: None,
        });
    }

    // The explicit download rendition. Usually the watermarked one.
    if let Some(url) = first_url_in(video, &["download_addr", "url_list"]) {
        if !variants.iter().any(|v| v.source_url == url) {
            let watermarked = is_watermarked(&url);
            variants.push(MediaVariantSource {
                kind: VariantKind::Standard,
                label: if watermarked {
                    "Download (watermarked)".to_string()
                } else {
                    "Download".to_string()
                },
                is_original: false,
                source_url: url,
                width: None,
                height: None,
                file_size: integer_at(
                    video.get("download_addr").unwrap_or(&Value::Null),
                    &["data_size"],
                )
                .and_then(|v| u64::try_from(v).ok()),
                bitrate_kbps: None,
            });
        }
    }

    // Per-quality renditions, when the payload lists them.
    if let Some(bit_rates) = video.get("bit_rate").and_then(Value::as_array) {
        for entry in bit_rates {
            let Some(url) = first_url_in(entry, &["play_addr", "url_list"]) else {
                continue;
            };
            if variants.iter().any(|v| v.source_url == url) {
                continue;
            }
            let watermarked = is_watermarked(&url);
            let gear = string_at(entry, &["gear_name"]).unwrap_or_else(|| "Standard".into());
            let height = integer_at(entry.get("play_addr").unwrap_or(&Value::Null), &["height"])
                .and_then(|v| u32::try_from(v).ok());
            variants.push(MediaVariantSource {
                kind: if watermarked {
                    VariantKind::Standard
                } else {
                    VariantKind::High
                },
                label: if watermarked {
                    format!("{gear} (watermarked)")
                } else {
                    gear
                },
                is_original: false,
                source_url: url.clone(),
                width: integer_at(entry.get("play_addr").unwrap_or(&Value::Null), &["width"])
                    .and_then(|v| u32::try_from(v).ok()),
                height,
                file_size: integer_at(entry.get("play_addr").unwrap_or(&Value::Null), &["data_size"])
                    .and_then(|v| u64::try_from(v).ok()),
                bitrate_kbps: integer_at(entry, &["bit_rate"]).and_then(|v| u32::try_from(v).ok()),
            });
        }
    }

    if variants.is_empty() {
        return Ok(Vec::new());
    }

    // Best first: a clean original, then the highest quality, then the rest.
    variants.sort_by_key(|source| match (source.is_original, source.kind) {
        (true, _) => 0,
        (false, VariantKind::High) => 1,
        (false, VariantKind::Standard) => 2,
        _ => 3,
    });

    let primary = &variants[0];
    let mut item = MediaItem::new(
        post.id.clone(),
        MediaType::Video,
        primary.source_url.clone(),
    );
    item.thumbnail_url = thumbnail_url(aweme);
    item.mime_type = Some("video/mp4".to_string());
    item.extension = Some("mp4".to_string());
    item.width = integer_at(video, &["width"])
        .and_then(|v| u32::try_from(v).ok())
        .or(primary.width);
    item.height = integer_at(video, &["height"])
        .and_then(|v| u32::try_from(v).ok())
        .or(primary.height);
    item.duration_seconds = duration_seconds(video);
    item.file_size = primary.file_size.or_else(|| {
        integer_at(video, &["play_addr", "data_size"]).and_then(|v| u64::try_from(v).ok())
    });
    item.variants = variants
        .into_iter()
        .map(|source| source.into_variant(&item.id))
        .collect();
    item.metadata.insert(
        "has_clean_source".into(),
        serde_json::json!(has_clean_source(&item.variants)),
    );

    Ok(vec![item])
}

/// Build one media item per image for an image/carousel post.
fn image_items(post: &mut Post, images: &[Value]) -> Vec<MediaItem> {
    let post_id = post.id.clone();
    images
        .iter()
        .enumerate()
        .filter_map(|(index, image)| {
            let url = first_url_in(image, &["url_list"])
                .or_else(|| string_at(image, &["url"]))?;
            let mut item = MediaItem::new(&post_id, MediaType::Image, url);
            item.thumbnail_url = first_url_in(image, &["url_list"]);
            item.width = integer_at(image, &["width"]).and_then(|v| u32::try_from(v).ok());
            item.height = integer_at(image, &["height"]).and_then(|v| u32::try_from(v).ok());
            let extension = extension_for(&item.source_url, "jpeg");
            item.extension = Some(extension.clone());
            item.mime_type = Some(mime_for_extension(&extension));
            item.metadata
                .insert("index".into(), serde_json::json!(index));
            Some(item)
        })
        .collect()
}

/// The post's soundtrack as the source described it, when it described one.
///
/// `downloadable` is decided by whether a playable resource was actually found,
/// not by whether the block exists. Everything else is copied from the source
/// and nothing is filled in: a track with no stated title has no title here,
/// rather than the post's caption standing in for one.
fn soundtrack_metadata(aweme: &Value, downloadable: bool) -> Option<Value> {
    let music = aweme.get("music")?;
    let title = string_at(music, &["title"]).filter(|t| !t.trim().is_empty());
    let author = string_at(music, &["author"]).filter(|a| !a.trim().is_empty());
    let duration = integer_at(music, &["duration"])
        .map(|ms| ms as f64 / 1000.0)
        .filter(|d| *d > 0.0);

    // A music block that says nothing at all is not a soundtrack; recording it
    // would put an empty section on every post that happens to carry the key.
    if title.is_none() && author.is_none() && duration.is_none() {
        return None;
    }

    Some(serde_json::json!({
        "title": title,
        "author": author,
        "durationSeconds": duration,
        // Whether this application can fetch the track, as opposed to merely
        // knowing it exists. The screen uses this to decide between offering a
        // download and showing a credit.
        "downloadable": downloadable,
    }))
}

/// Build an audio item from the post's music track, when present.
fn audio_item(post: &Post, aweme: &Value) -> Option<MediaItem> {
    let music = aweme.get("music")?;
    let url = first_url_in(music, &["play_url", "url_list"])
        .or_else(|| first_url_in(music, &["play_url_list"]))?;

    let mut item = MediaItem::new(post.id.clone(), MediaType::Audio, url);
    let extension = extension_for(&item.source_url, "mp3");
    item.extension = Some(extension.clone());
    item.mime_type = Some(mime_for_extension(&extension));
    item.thumbnail_url = first_url_in(aweme, &["music", "cover_large", "url_list"])
        .or_else(|| first_url_in(aweme, &["music", "cover_thumb", "url_list"]));
    item.duration_seconds = integer_at(music, &["duration"])
        .map(|ms| ms as f64 / 1000.0)
        .filter(|d| *d > 0.0);
    if let Some(title) = string_at(music, &["title"]).filter(|t| !t.trim().is_empty()) {
        item.metadata.insert("title".into(), serde_json::json!(title));
    }
    item.metadata
        .insert("kind".into(), serde_json::json!("post_audio_track"));
    Some(item)
}

/// Normalize the aweme's author block into a [`Creator`].
fn creator_from_aweme(aweme: &Value, post_id: &str) -> Option<Creator> {
    let author = aweme.get("author")?;
    let platform_id = string_at(author, &["sec_uid"])
        .or_else(|| string_at(author, &["uid"]))?;
    let display_name = string_at(author, &["nickname"]).unwrap_or_else(|| platform_id.clone());

    let mut creator = Creator::new(
        Platform::Douyin,
        platform_id.clone(),
        display_name,
        format!("https://www.douyin.com/user/{platform_id}"),
    );
    creator.handle = string_at(author, &["unique_id"]).filter(|h| !h.trim().is_empty());
    creator.avatar_url = first_url_in(author, &["avatar_thumb", "url_list"])
        .or_else(|| first_url_in(author, &["avatar_larger", "url_list"]));
    creator.verified = string_at(author, &["custom_verify"])
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    creator
        .metadata
        .insert("source_post_id".into(), serde_json::json!(post_id));
    Some(creator)
}

/// The author handle to show next to a post.
fn creator_handle(aweme: &Value) -> Option<String> {
    let author = aweme.get("author")?;
    string_at(author, &["unique_id"])
        .filter(|h| !h.trim().is_empty())
        .or_else(|| string_at(author, &["nickname"]))
}

/// Pick a thumbnail, preferring the still cover.
fn thumbnail_url(aweme: &Value) -> Option<String> {
    for path in [
        &["video", "cover", "url_list"][..],
        &["video", "origin_cover", "url_list"][..],
        &["video", "dynamic_cover", "url_list"][..],
    ] {
        if let Some(url) = first_url_in(aweme, path) {
            return Some(url);
        }
    }
    // Image posts use the first image as their cover.
    aweme
        .get("images")
        .and_then(Value::as_array)
        .and_then(|images| images.first())
        .and_then(|first| first_url_in(first, &["url_list"]))
}

/// True when a media URL points at Douyin's watermarked rendition.
///
/// The check is on the path marker only; the URL is returned unchanged
/// everywhere it is used.
pub fn is_watermarked(url: &str) -> bool {
    url.to_ascii_lowercase().contains(WATERMARK_PATH_MARKER)
}

/// Follow a path of object keys to a string.
fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    walk(value, path).and_then(Value::as_str).map(str::to_string)
}

/// Follow a path of object keys to an integer.
fn integer_at(value: &Value, path: &[&str]) -> Option<i64> {
    let found = walk(value, path)?;
    found
        .as_i64()
        .or_else(|| found.as_str().and_then(|s| s.parse().ok()))
        .or_else(|| found.as_f64().map(|f| f as i64))
}

/// Follow a path of object keys to the first URL of a `url_list`.
fn first_url_in(value: &Value, path: &[&str]) -> Option<String> {
    let found = walk(value, path)?;
    match found {
        Value::Array(list) => list.first().and_then(Value::as_str).map(str::to_string),
        Value::String(url) => Some(url.clone()),
        _ => None,
    }
}

/// Walk a path of object keys.
fn walk<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(key)?;
    }
    Some(current)
}

/// Playback duration in seconds, when the payload reports it.
fn duration_seconds(video: &Value) -> Option<f64> {
    let millis = integer_at(video, &["duration"])?;
    if millis <= 0 {
        return None;
    }
    Some(millis as f64 / 1000.0)
}

/// Guess an extension from a URL path, falling back to `default`.
fn extension_for(url: &str, default: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let candidate = without_query
        .rsplit('/')
        .next()
        .and_then(|last| last.rsplit_once('.').map(|(_, ext)| ext))
        .map(|ext| ext.to_ascii_lowercase())
        .filter(|ext| (2..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()));
    candidate.unwrap_or_else(|| default.to_string())
}

/// Content type for a known extension.
fn mime_for_extension(extension: &str) -> String {
    match extension {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// An intermediate representation of a variant before its media item exists.
struct MediaVariantSource {
    kind: VariantKind,
    label: String,
    is_original: bool,
    source_url: String,
    width: Option<u32>,
    height: Option<u32>,
    file_size: Option<u64>,
    bitrate_kbps: Option<u32>,
}

impl MediaVariantSource {
    fn into_variant(self, media_item_id: &str) -> crate::models::MediaVariant {
        let mut variant =
            crate::models::MediaVariant::new(media_item_id, self.kind, self.label, self.source_url);
        variant.is_original = self.is_original;
        variant.width = self.width;
        variant.height = self.height;
        variant.file_size = self.file_size;
        variant.bitrate_kbps = self.bitrate_kbps;
        variant.mime_type = Some("video/mp4".to_string());
        variant.extension = Some("mp4".to_string());
        variant
    }
}

/// Whether any variant of the item is a genuinely clean source.
///
/// `MediaVariant` has no metadata column, so the watermarked/clean distinction
/// is expressed by `is_original` on the variant and summarised here for the UI.
/// A watermarked source keeps `is_original == false`; it is never rewritten to
/// look clean.
pub fn has_clean_source(variants: &[crate::models::MediaVariant]) -> bool {
    variants.iter().any(|variant| variant.is_original)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A share page shaped like the one Douyin serves to a mobile agent.
    ///
    /// This is *test data*, not a captured result: it exists so the parser can
    /// be exercised deterministically without the network. It is never served
    /// to a user and is never presented as a live resolution.
    pub(crate) fn sample_share_page() -> String {
        format!(
            "<html><head><title>Douyin</title></head><body>\n\
             <script>window._ROUTER_DATA = {{\"loaderData\":{{\"video_(id)/page\":\
             {{\"videoInfoRes\":{{\"item_list\":[{}]}}}}}},\"errors\":null}};</script>\n\
             </body></html>",
            aweme_json()
        )
    }

    /// A carousel page: three images instead of a video.
    pub(crate) fn sample_carousel_page() -> String {
        let aweme = r#"{
            "aweme_id": "7289999999999999999",
            "desc": "Three photos",
            "create_time": 1760000001,
            "author": { "sec_uid": "MS4wLjABAAAAexamplecreator", "nickname": "Alice", "unique_id": "alice" },
            "images": [
                { "url_list": ["https://p3-cdn.douyin.com/1.jpeg"], "width": 1080, "height": 1440 },
                { "url_list": ["https://p3-cdn.douyin.com/2.jpeg"], "width": 1080, "height": 1440 },
                { "url_list": ["https://p3-cdn.douyin.com/3.jpeg"], "width": 1080, "height": 1440 }
            ]
        }"#;
        format!(
            "<html><script>window._ROUTER_DATA = {{\"loaderData\":{{\"video_(id)/page\":\
             {{\"videoInfoRes\":{{\"item_list\":[{aweme}]}}}}}}}};</script></html>"
        )
    }

    /// A page that loaded but carries no post — the shape a blocked or
    /// region-restricted request produces.
    pub(crate) fn sample_page_without_items() -> String {
        "<html><script>window._ROUTER_DATA = {\"loaderData\":{\"video_(id)/page\":\
         {\"videoInfoRes\":{\"item_list\":[]}}}};</script></html>"
            .to_string()
    }

    /// The page shape Douyin currently serves to a mobile agent for a post.
    ///
    /// Reproduced from a live response (trimmed to the structurally significant
    /// fields): `_ROUTER_DATA` is present, but it contains only the page's own
    /// configuration — `itemId`, render flags, A/B parameters, and a nested
    /// `commonContext` — and no `videoInfoRes` at all. The post's media is
    /// fetched by the page's JavaScript afterwards, from an endpoint that
    /// requires a signed, signed-in session.
    ///
    /// This fixture is what pins the application's behaviour against that
    /// reality: the marker is there, the data is not, and the outcome must be a
    /// typed "unavailable" rather than a fabricated post.
    pub(crate) fn sample_live_config_only_page() -> String {
        "<!doctype html><html><head><title>Douyin</title></head><body>\n\
         <script nonce=\"argus-csp-token\">window._ROUTER_DATA = {\
         \"loaderData\":{\
         \"video_layout\":null,\
         \"video_(id)/page\":{\
         \"ua\":\"Mozilla/5.0 (iPhone; CPU iPhone OS 16_6 like Mac OS X)\",\
         \"isSpider\":false,\
         \"webId\":\"7694252254764746291\",\
         \"query\":{},\
         \"renderInSSR\":1,\
         \"lastPath\":\"7684462711358426420\",\
         \"appName\":\"safari\",\
         \"host\":\"www.iesdouyin.com\",\
         \"isNotSupportWebp\":false,\
         \"commonContext\":{\"ua\":\"Mozilla/5.0\",\"isSpider\":false,\"query\":{},\
         \"renderInSSR\":1,\"lastPath\":\"7684462711358426420\",\"appName\":\"safari\",\
         \"host\":\"www.iesdouyin.com\",\"isNotSupportWebp\":false},\
         \"isAiDouyinOpt\":\"\",\
         \"itemId\":\"7684462711358426420\",\
         \"isVideoOptimize\":true,\
         \"openDirectGroup\":\"4\",\
         \"isVideoStyleOptimize\":true,\
         \"isButtonOptimize\":true,\
         \"isAutoOpenApp\":true,\
         \"darkModeAdaptation\":true,\
         \"serverToken\":\"\",\
         \"abParams\":{\"reflow_video_page_optimize\":false,\"show_rec_group_list\":false}\
         }},\
         \"errors\":null};</script>\n</body></html>"
            .to_string()
    }

    /// The anti-bot shell Douyin serves to a desktop agent: a JavaScript VM with
    /// no embedded state at all.
    pub(crate) fn sample_antibot_shell_page() -> String {
        "<html><head><meta charset=\"UTF-8\" /></head><body></body> <script> var glb;\
         (glb=\"undefined\"==typeof window?global:window)._$jsvmprt=function(b,e,f){\
         return 0}();</script></html>"
            .to_string()
    }

    /// A page whose post lives in `RENDER_DATA` rather than `_ROUTER_DATA`.
    ///
    /// This is not a captured response — it is the PC shell's `RENDER_DATA`
    /// shape (a percent-encoded JSON document under one `app` key) with an
    /// aweme placed where a different schema version might put it. It exists to
    /// prove the extractor is not wired to one blob name or one path.
    pub(crate) fn sample_render_data_page() -> String {
        let document = format!(
            "{{\"app\":{{\"pathname\":\"/video/7281234567890123456\",\
             \"isSpider\":false,\"pageData\":{{\"nested\":{{\"deep\":{{\"aweme\":[\
             {}]}}}}}}}}}}",
            aweme_json()
        );
        let encoded: String = document
            .bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (byte as char).to_string()
                }
                other => format!("%{other:02X}"),
            })
            .collect();
        format!(
            "<!doctype html><html><body><script id=\"RENDER_DATA\" \
             type=\"application/json\">{encoded}</script></body></html>"
        )
    }

    /// The single aweme used by the fixtures above.
    fn aweme_json() -> &'static str {
        r#"{
            "aweme_id": "7281234567890123456",
            "desc": "Sunset timelapse {golden hour}",
            "create_time": 1760000000,
            "author": {
                "uid": "99001122",
                "sec_uid": "MS4wLjABAAAAexamplecreator",
                "nickname": "Alice",
                "unique_id": "alice",
                "avatar_thumb": { "url_list": ["https://p3-cdn.douyin.com/avatar.jpeg"] }
            },
            "video": {
                "duration": 15000,
                "width": 1080,
                "height": 1920,
                "play_addr": {
                    "uri": "v0200fclean",
                    "url_list": ["https://v3-cdn.douyin.com/aweme/v1/play/?video_id=v0200fclean"],
                    "width": 1080,
                    "height": 1920,
                    "data_size": 1234567
                },
                "download_addr": {
                    "url_list": ["https://v3-cdn.douyin.com/aweme/v1/playwm/?video_id=v0200fclean"],
                    "data_size": 2345678
                },
                "cover": { "url_list": ["https://p3-cdn.douyin.com/cover.jpeg"] },
                "bit_rate": [
                    {
                        "gear_name": "720p",
                        "bit_rate": 1200,
                        "play_addr": {
                            "url_list": ["https://v3-cdn.douyin.com/aweme/v1/play/?video_id=v0200f720"],
                            "width": 720,
                            "height": 1280,
                            "data_size": 800000
                        }
                    }
                ]
            },
            "music": {
                "title": "Original sound",
                "duration": 15000,
                "play_url": { "url_list": ["https://sf3-cdn.douyin.com/music/track.mp3"] }
            },
            "images": null
        }"#
    }

    #[test]
    fn parses_the_embedded_payload() {
        let data = parse_router_data(&sample_share_page()).unwrap();
        let aweme = first_aweme(&data).unwrap();
        assert_eq!(string_at(aweme, &["aweme_id"]).as_deref(), Some("7281234567890123456"));
    }

    #[test]
    fn brace_matching_survives_braces_inside_strings() {
        // The caption contains `{` and `}`; a naive first-`}` search would
        // truncate the payload and fail to parse.
        let data = parse_router_data(&sample_share_page()).unwrap();
        let aweme = first_aweme(&data).unwrap();
        assert_eq!(
            string_at(aweme, &["desc"]).as_deref(),
            Some("Sunset timelapse {golden hour}")
        );
    }

    #[test]
    fn a_page_without_the_marker_is_a_resolution_error() {
        let err = parse_router_data("<html><body>nothing here</body></html>").unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Resolution);
    }

    #[test]
    fn truncated_payload_is_a_resolution_error() {
        let err = parse_router_data("<script>window._ROUTER_DATA = {\"a\":1").unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Resolution);
    }

    #[test]
    fn an_empty_item_list_is_unavailable_not_a_parse_failure() {
        let data = parse_router_data(&sample_page_without_items()).unwrap();
        let err = first_aweme(&data).unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::UnavailableContent);
    }

    // --- the live page shape -------------------------------------------------

    #[test]
    fn the_live_config_only_page_is_parsed_but_yields_no_post() {
        // The marker is present and the JSON is well formed — the page simply
        // does not contain the post. That must be `UnavailableContent`, and it
        // must not produce a post with invented fields.
        let html = sample_live_config_only_page();
        let data = parse_router_data(&html).expect("the marker and JSON are both present");

        assert_eq!(string_at(&data, &["loaderData", "video_(id)/page", "itemId"]).as_deref(),
                   Some("7684462711358426420"));

        let err = first_aweme(&data).unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::UnavailableContent);
        assert!(!err.user_message().trim().is_empty());
    }

    #[test]
    fn describe_payload_reports_the_blob_and_the_missing_keys() {
        let summary = describe_payload(&sample_live_config_only_page());
        assert!(summary.contains("state_blobs=[_ROUTER_DATA]"), "got {summary}");
        // No post-bearing key is present, which is the whole point.
        assert!(summary.contains("aweme_keys=[]"), "got {summary}");
        // And the loader sections are named, so a schema change is visible.
        assert!(summary.contains("video_(id)/page"), "got {summary}");
        assert!(summary.contains("bytes="), "got {summary}");
    }

    #[test]
    fn describe_payload_reports_a_page_with_no_embedded_state() {
        let summary = describe_payload(&sample_antibot_shell_page());
        assert!(summary.contains("state_blobs=[]"), "got {summary}");
        assert!(summary.contains("aweme_keys=[]"), "got {summary}");
    }

    #[test]
    fn describe_payload_is_bounded_and_single_line() {
        // A hostile or enormous page must not be able to flood a log line.
        let huge = sample_share_page().repeat(2000);
        let summary = describe_payload(&huge);
        assert!(!summary.contains('\n'), "diagnostics must stay on one line");
        assert!(summary.len() < 400, "diagnostics must stay bounded, got {}", summary.len());
    }

    #[test]
    fn a_data_bearing_page_reports_its_post_keys_present() {
        let summary = describe_payload(&sample_share_page());
        assert!(summary.contains("videoInfoRes"), "got {summary}");
        assert!(summary.contains("play_addr"), "got {summary}");
    }

    #[test]
    fn a_page_without_the_marker_is_a_resolution_error_with_an_honest_message() {
        let err = parse_router_data(&sample_antibot_shell_page()).unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Resolution);
        // The message must not claim the post is gone; it is the *page* that
        // was unreadable.
        assert!(!err.user_message().is_empty());
    }

    // --- blob-agnostic extraction -------------------------------------------

    #[test]
    fn percent_decode_round_trips_and_tolerates_malformed_escapes() {
        assert_eq!(percent_decode("%7B%22a%22%3A1%7D"), "{\"a\":1}");
        assert_eq!(percent_decode("plain"), "plain");
        // A stray percent must not panic or eat the character.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%ZZ"), "%ZZ");
    }

    #[test]
    fn state_blobs_are_read_from_render_data_too() {
        let blobs = parse_state_blobs(&sample_render_data_page());
        assert_eq!(blobs.len(), 1, "RENDER_DATA should have been decoded and parsed");
        assert!(blobs[0].get("app").is_some());
    }

    #[test]
    fn extract_state_blobs_returns_nothing_for_an_antibot_shell() {
        assert!(parse_state_blobs(&sample_antibot_shell_page()).is_empty());
    }

    #[test]
    fn the_structural_search_finds_a_post_under_any_path() {
        // The fixture buries the post under app.pageData.nested.deep.aweme[0],
        // which none of the known paths describe. A shape-based search still
        // finds it, which is what makes a schema move survivable.
        let page = sample_render_data_page();
        let aweme = find_aweme_in_page(&page).expect("the post must be found wherever it lives");
        assert_eq!(string_at(&aweme, &["aweme_id"]).as_deref(), Some("7281234567890123456"));

        // And it normalizes from there just like the `_ROUTER_DATA` path.
        let parsed = aweme_to_domain(&aweme).unwrap();
        assert_eq!(parsed.post.caption.as_deref(), Some("Sunset timelapse {golden hour}"));
        assert_eq!(parsed.media.len(), 2);
    }

    #[test]
    fn find_aweme_requires_media_so_an_id_reference_is_not_mistaken_for_a_post() {
        // A payload that mentions an id but carries no media must not be
        // accepted as the post.
        let bare = serde_json::json!({"aweme_id": "1", "reference": {"aweme_id": "2"}});
        assert!(find_aweme(&bare).is_none());

        let real = serde_json::json!({"wrap": [{"aweme_id": "1", "images": [{"url_list": ["u"]}]}]});
        assert!(find_aweme(&real).is_some());
    }

    #[test]
    fn find_aweme_in_page_reports_an_unreadable_page_distinctly() {
        let err = find_aweme_in_page(&sample_antibot_shell_page()).unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Resolution);
    }

    #[test]
    fn find_aweme_in_page_reports_a_readable_but_postless_page_distinctly() {
        let err = find_aweme_in_page(&sample_live_config_only_page()).unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::UnavailableContent);
    }

    #[test]
    fn find_aweme_in_page_reads_the_router_data_shape() {
        let aweme = find_aweme_in_page(&sample_share_page()).expect("the post must be found");
        assert_eq!(string_at(&aweme, &["aweme_id"]).as_deref(), Some("7281234567890123456"));
    }

    #[test]
    fn find_aweme_in_page_reads_an_empty_item_list_as_unavailable() {
        let err = find_aweme_in_page(&sample_page_without_items()).unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::UnavailableContent);
    }

    #[test]
    fn video_aweme_normalizes_into_a_post_with_media_and_variants() {
        let data = parse_router_data(&sample_share_page()).unwrap();
        let parsed = aweme_to_domain(first_aweme(&data).unwrap()).unwrap();

        assert_eq!(parsed.post.platform_post_id, "7281234567890123456");
        assert_eq!(parsed.post.media_type, MediaType::Video);
        assert_eq!(parsed.post.caption.as_deref(), Some("Sunset timelapse {golden hour}"));
        assert_eq!(parsed.post.creator_handle.as_deref(), Some("alice"));
        assert_eq!(parsed.post.thumbnail_url.as_deref(), Some("https://p3-cdn.douyin.com/cover.jpeg"));
        assert!(parsed.post.published_at.is_some());

        // The creator is normalized alongside the post.
        let creator = parsed.creator.expect("creator present");
        assert_eq!(creator.platform_id, "MS4wLjABAAAAexamplecreator");
        assert_eq!(creator.display_name, "Alice");
        assert_eq!(creator.handle.as_deref(), Some("alice"));
        // And stored on the post as a domain object, not a platform shape.
        assert!(parsed.post.metadata.contains_key(RESOLVED_CREATOR_KEY));

        // Video item + audio item.
        assert_eq!(parsed.media.len(), 2);
        let video = parsed.media.iter().find(|m| m.media_type == MediaType::Video).unwrap();
        assert_eq!(video.extension.as_deref(), Some("mp4"));
        assert_eq!(video.mime_type.as_deref(), Some("video/mp4"));
        assert_eq!(video.width, Some(1080));
        assert_eq!(video.height, Some(1920));
        assert_eq!(video.duration_seconds, Some(15.0));
        // clean play + watermarked download + 720p rendition
        assert_eq!(video.variants.len(), 3);

        let audio = parsed.media.iter().find(|m| m.media_type == MediaType::Audio).unwrap();
        assert_eq!(audio.source_url, "https://sf3-cdn.douyin.com/music/track.mp3");
        assert_eq!(audio.extension.as_deref(), Some("mp3"));
    }

    #[test]
    fn an_image_post_with_playable_music_offers_the_pictures_and_the_track() {
        // The shape the brief calls image-plus-music: pictures stay pictures,
        // the track is its own item of its own type, and the post is still a
        // picture post rather than an audio one.
        let aweme = serde_json::json!({
            "aweme_id": "7629996475679029857",
            "desc": "两张照片",
            "author": { "uid": "1", "sec_uid": "sec", "nickname": "郭虎" },
            "images": [
                { "url_list": ["https://p3-cdn.douyinpic.com/a.jpeg"], "width": 1080, "height": 1440 },
                { "url_list": ["https://p3-cdn.douyinpic.com/b.jpeg"], "width": 1080, "height": 1440 }
            ],
            "music": {
                "title": "Original sound",
                "author": "郭虎",
                "duration": 15000,
                "play_url": { "url_list": ["https://sf3-cdn.douyin.com/music/track.mp3"] }
            }
        });

        let parsed = aweme_to_domain(&aweme).expect("parsed");

        assert_eq!(parsed.post.media_type, MediaType::Carousel);
        let images = parsed.media.iter().filter(|m| m.media_type == MediaType::Image).count();
        let audio = parsed.media.iter().filter(|m| m.media_type == MediaType::Audio).count();
        assert_eq!((images, audio), (2, 1), "two pictures and one track, separately typed");
        // No black video was synthesized to hold the sound.
        assert!(parsed.media.iter().all(|m| m.media_type != MediaType::Video));

        let track = &parsed.post.metadata["soundtrack"];
        assert_eq!(track["title"], serde_json::json!("Original sound"));
        assert_eq!(track["author"], serde_json::json!("郭虎"));
        assert_eq!(track["downloadable"], serde_json::json!(true));
    }

    #[test]
    fn music_with_no_playable_url_is_credited_rather_than_downloaded() {
        // The common case, and the one that used to lose a fact: the block names
        // the track and credits its author, and carries no URL at all. The
        // credit is kept; no download is invented for it.
        let aweme = serde_json::json!({
            "aweme_id": "7629996475679029858",
            "desc": "照片",
            "author": { "uid": "1", "sec_uid": "sec", "nickname": "郭虎" },
            "images": [
                { "url_list": ["https://p3-cdn.douyinpic.com/a.jpeg"], "width": 1080, "height": 1440 }
            ],
            "music": { "title": "背景音乐", "author": "某歌手", "duration": 30000 }
        });

        let parsed = aweme_to_domain(&aweme).expect("parsed");

        assert!(
            parsed.media.iter().all(|m| m.media_type == MediaType::Image),
            "nothing downloadable may be invented from a credit"
        );
        let track = &parsed.post.metadata["soundtrack"];
        assert_eq!(track["title"], serde_json::json!("背景音乐"));
        assert_eq!(track["author"], serde_json::json!("某歌手"));
        assert_eq!(track["durationSeconds"], serde_json::json!(30.0));
        assert_eq!(
            track["downloadable"],
            serde_json::json!(false),
            "the screen must not offer a file the source never provided"
        );
    }

    #[test]
    fn a_music_block_that_says_nothing_is_not_a_soundtrack() {
        let aweme = serde_json::json!({
            "aweme_id": "7629996475679029859",
            "desc": "照片",
            "author": { "uid": "1", "sec_uid": "sec", "nickname": "郭虎" },
            "images": [
                { "url_list": ["https://p3-cdn.douyinpic.com/a.jpeg"], "width": 1080, "height": 1440 }
            ],
            "music": { "id_str": "123" }
        });

        let parsed = aweme_to_domain(&aweme).expect("parsed");
        assert!(
            !parsed.post.metadata.contains_key("soundtrack"),
            "an empty music block is not a soundtrack"
        );
    }

    #[test]
    fn the_clean_source_is_preferred_and_the_watermark_is_labelled_not_removed() {
        let data = parse_router_data(&sample_share_page()).unwrap();
        let parsed = aweme_to_domain(first_aweme(&data).unwrap()).unwrap();
        let video = parsed.media.iter().find(|m| m.media_type == MediaType::Video).unwrap();

        let preferred = video.preferred_variant();
        assert!(preferred.is_original, "a clean source must be preferred");
        assert!(!is_watermarked(&preferred.source_url));

        // The watermarked rendition is present, labelled, and keeps its URL.
        let marked = video
            .variants
            .iter()
            .find(|v| is_watermarked(&v.source_url))
            .expect("watermarked variant retained");
        assert!(!marked.is_original);

        // Nothing was rewritten: no variant URL silently lost its marker.
        for variant in &video.variants {
            if variant.source_url.contains("playwm") {
                assert!(!variant.is_original);
            }
        }
    }

    #[test]
    fn image_post_normalizes_into_one_item_per_image() {
        let data = parse_router_data(&sample_carousel_page()).unwrap();
        let parsed = aweme_to_domain(first_aweme(&data).unwrap()).unwrap();

        assert_eq!(parsed.post.media_type, MediaType::Carousel);
        assert_eq!(parsed.media.len(), 3);
        assert!(parsed.media.iter().all(|m| m.media_type == MediaType::Image));
        assert!(parsed.media.iter().all(|m| m.extension.as_deref() == Some("jpeg")));
        assert_eq!(parsed.media[1].metadata["index"], serde_json::json!(1));
        // The first image doubles as the post thumbnail.
        assert_eq!(parsed.post.thumbnail_url.as_deref(), Some("https://p3-cdn.douyin.com/1.jpeg"));
    }

    #[test]
    fn an_aweme_without_an_id_is_a_resolution_error() {
        let err = aweme_to_domain(&serde_json::json!({ "desc": "no id" })).unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Resolution);
    }

    #[test]
    fn an_aweme_with_no_downloadable_media_is_unsupported() {
        let err = aweme_to_domain(&serde_json::json!({
            "aweme_id": "1",
            "desc": "text only",
            "author": { "sec_uid": "x" }
        }))
        .unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::UnsupportedContent);
    }

    #[test]
    fn watermark_detection_reads_the_path_marker_only() {
        assert!(is_watermarked("https://cdn/aweme/v1/playwm/?video_id=x"));
        assert!(!is_watermarked("https://cdn/aweme/v1/play/?video_id=x"));
        assert!(!is_watermarked("https://cdn/watermarked-name-is-fine.mp4"));
    }

    #[test]
    fn extensions_come_from_the_url_and_fall_back_predictably() {
        assert_eq!(extension_for("https://cdn/a/b/c.JPEG?x=1", "jpeg"), "jpeg");
        assert_eq!(extension_for("https://cdn/audio/track", "mp3"), "mp3");
        assert_eq!(mime_for_extension("mp4"), "video/mp4");
        assert_eq!(mime_for_extension("unknown"), "application/octet-stream");
    }
}
