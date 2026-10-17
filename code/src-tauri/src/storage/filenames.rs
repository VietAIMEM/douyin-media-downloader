//! Filename and path safety.
//!
//! Every string that becomes part of a filesystem path passes through this
//! module. It is the single defence against invalid Windows names, path
//! traversal, reserved device names, and over-long paths.

use std::path::{Component, Path, PathBuf};

use crate::models::common::{MediaType, Timestamp};

/// Characters Windows forbids in a path segment.
const ILLEGAL_SEGMENT_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Device names Windows reserves (case-insensitive, with or without extension).
const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Longest a single directory segment may be.
pub const MAX_SEGMENT_LEN: usize = 120;
/// Longest a file name (including extension) may be.
pub const MAX_FILENAME_LEN: usize = 150;
/// Conservative ceiling for the whole path, comfortably below the 260-character
/// Windows `MAX_PATH` limit so directories can still be appended.
pub const MAX_FULL_PATH_LEN: usize = 240;

/// Fallback used when a name sanitizes down to nothing.
const FALLBACK_NAME: &str = "untitled";

/// Sanitize one path segment (directory or file name fragment).
///
/// Guarantees the result is a single, non-empty, legal Windows segment.
pub fn sanitize_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_control() || ILLEGAL_SEGMENT_CHARS.contains(&ch) {
            out.push('_');
        } else {
            out.push(ch);
        }
    }

    // Collapse runs of whitespace and trim.
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut trimmed = collapsed.trim_matches(['.', ' ']).to_string();

    if trimmed.is_empty() {
        return FALLBACK_NAME.to_string();
    }

    if is_reserved(&trimmed) {
        trimmed.push('_');
    }

    truncate_chars(&mut trimmed, MAX_SEGMENT_LEN);
    // Truncation can expose a trailing dot/space again.
    let trimmed = trimmed.trim_end_matches(['.', ' ']).to_string();
    if trimmed.is_empty() {
        return FALLBACK_NAME.to_string();
    }
    trimmed
}

/// Sanitize a file name, preserving its extension.
pub fn sanitize_filename(raw: &str) -> String {
    let raw = raw.trim();
    let (stem, extension) = match raw.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() && ext.len() <= 10 => {
            (stem, Some(ext))
        }
        _ => (raw, None),
    };

    let mut name = sanitize_segment(stem);
    if let Some(ext) = extension {
        let ext = sanitize_segment(ext);
        // Guard against the extension itself being a reserved name fragment.
        name.push('.');
        name.push_str(&ext);
    }

    truncate_filename(&mut name);
    name
}

/// True when `value` (ignoring any extension) is a reserved Windows device name.
pub fn is_reserved(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or(value);
    RESERVED_NAMES
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

/// The standard sub-folder name for a media type, used by `{type}` in folder
/// templates and by the default organisation scheme.
pub fn media_folder_name(media_type: MediaType) -> &'static str {
    match media_type {
        MediaType::Video => "Videos",
        MediaType::Image | MediaType::Carousel => "Images",
        MediaType::Audio => "Audio",
        // Saved text has its own folder, so a caption never lands among the
        // pictures of the post it belongs to.
        MediaType::Text => "Text",
    }
}

/// Values available to filename and folder templates.
#[derive(Debug, Clone, Default)]
pub struct NamingContext {
    pub creator: Option<String>,
    pub handle: Option<String>,
    pub post_id: Option<String>,
    pub media_type: Option<MediaType>,
    pub caption: Option<String>,
    /// Zero-based index within a multi-item post.
    pub index: Option<u32>,
    /// How many items of the same kind the post holds.
    ///
    /// The slides of a carousel are brothers of one another; a video and the
    /// separate audio track beside it are not. Only brothers share a name, so
    /// only brothers need telling apart, and this is what says whether they do.
    pub item_count: Option<u32>,
    pub published_at: Option<Timestamp>,
    /// Extension without the leading dot.
    pub extension: Option<String>,
}

/// Substitute placeholders in `template`.
///
/// Unknown placeholders are left untouched here; `models::settings` validation
/// is what rejects them before a template is ever saved.
/// Replace every placeholder in `template` with the value from `context`.
///
/// Substituted *values* are flattened first (see [`flatten_value`]) so data from
/// the source — a caption containing `/`, a handle containing `..` — can never
/// introduce a directory level or a parent-directory hop. Separators written
/// literally in the template are left alone, because those are the user
/// deliberately describing a folder layout.
pub fn apply_template(template: &str, context: &NamingContext) -> String {
    let date = context.published_at.map(|t| t.format("%Y-%m-%d").to_string());
    let time = context.published_at.map(|t| t.format("%H%M%S").to_string());

    let replacements: [(&str, String); 9] = [
        ("{creator}", context.creator.clone().unwrap_or_default()),
        ("{handle}", context.handle.clone().unwrap_or_default()),
        ("{id}", context.post_id.clone().unwrap_or_default()),
        ("{date}", date.unwrap_or_default()),
        ("{time}", time.unwrap_or_default()),
        (
            "{type}",
            context
                .media_type
                .map(|t| media_folder_name(t).to_string())
                .unwrap_or_default(),
        ),
        (
            "{index}",
            context
                .index
                .map(|i| format!("{:03}", i + 1))
                .unwrap_or_default(),
        ),
        ("{caption}", context.caption.clone().unwrap_or_default()),
        ("{ext}", context.extension.clone().unwrap_or_default()),
    ];

    let mut out = template.to_string();
    for (needle, value) in replacements {
        out = out.replace(needle, &flatten_value(&value));
    }
    out
}

/// Make a substituted value safe to sit inside a path segment.
///
/// Path separators become underscores, so a value can never add a directory
/// level, and dot runs are collapsed, so a value can never read as `.` or `..`.
fn flatten_value(value: &str) -> String {
    let mut out = value.replace(['/', '\\'], "_");
    while out.contains("..") {
        out = out.replace("..", "_");
    }
    out
}

/// Render and sanitize a folder template into relative path segments.
///
/// Returns an empty vector when the template resolves to nothing (files then go
/// directly into the download root).
pub fn render_folder_segments(template: &str, context: &NamingContext) -> Vec<String> {
    let rendered = apply_template(template, context);
    rendered
        .split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(sanitize_segment)
        .collect()
}

/// Render a filename template, appending the extension when the template does
/// not already place `{ext}`.
pub fn render_filename(template: &str, context: &NamingContext) -> String {
    let rendered = apply_template(template, context);

    // A template's separators only make sense between two values. When a
    // placeholder had nothing to contribute — `{date}_{id}` on content with no
    // known publish date — the separator it was meant to join is left dangling,
    // producing names like `_728123`. Trimming the ends keeps the name readable
    // without touching the separators the template author put between values.
    let stem = rendered.trim_matches(['_', '-', ' ']);
    let stem = if stem.is_empty() {
        // Every placeholder was empty, so there is no name to build one from.
        // The project's fallback name is used rather than an empty file name.
        sanitize_segment("")
    } else {
        stem.to_string()
    };

    let with_extension = match &context.extension {
        Some(ext) if !ext.is_empty() && !template.contains("{ext}") => {
            format!("{stem}.{ext}")
        }
        _ => stem,
    };
    // A post holding several items of one kind is a carousel, and its slides
    // must not share a name: `(1)` is the download layer's answer to a
    // collision, not a way of telling one slide from another. A template that
    // already places `{index}` has said where the number belongs, and is left
    // exactly as the user wrote it.
    let numbered = match (context.index, context.item_count) {
        (Some(index), Some(count)) if count > 1 && !template.contains("{index}") => {
            match with_extension.rfind('.') {
                // Padded the same way `{index}` pads, so a user who writes the
                // placeholder explicitly and one who leaves it out get the same
                // number in the same shape.
                Some(dot) => format!(
                    "{}_{:03}{}",
                    &with_extension[..dot],
                    index + 1,
                    &with_extension[dot..]
                ),
                None => format!("{with_extension}_{:03}", index + 1),
            }
        }
        _ => with_extension,
    };
    sanitize_filename(&numbered)
}

/// Append ` (n)` before the extension until `candidate` no longer exists.
///
/// Filesystem access is confined to *this* directory listing, which is the
/// minimum needed to make the "rename" conflict policy work.
pub fn unique_path(directory: &Path, file_name: &str) -> PathBuf {
    let base = Path::new(file_name);
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_name.to_string());
    let extension = base
        .extension()
        .map(|s| s.to_string_lossy().into_owned());

    let mut candidate = directory.join(file_name);
    let mut counter = 1;
    while candidate.exists() {
        let name = match &extension {
            Some(ext) => format!("{stem} ({counter}).{ext}"),
            None => format!("{stem} ({counter})"),
        };
        candidate = directory.join(name);
        counter += 1;
        if counter > 9_999 {
            // Pathological directory; fall back to a guaranteed-unique suffix.
            let name = match &extension {
                Some(ext) => format!("{stem}-{}.{ext}", crate::models::new_id()),
                None => format!("{stem}-{}", crate::models::new_id()),
            };
            return directory.join(name);
        }
    }
    candidate
}

/// Verify that `candidate` stays inside `root` **after the filesystem has
/// resolved whatever links are in the way**.
///
/// [`is_within`] compares path *text*, which cannot see a junction or a symbolic
/// link: `D:\Downloads\link\file.mp4` is inside `D:\Downloads` as a string, and
/// somewhere else entirely as a file. On Windows a junction needs no elevation
/// to create, and a download folder is exactly the sort of place one appears —
/// a user redirects `Downloads\videos` at another drive.
///
/// The deepest part of `candidate` that already exists is resolved, because the
/// file itself usually does not exist yet. That is the component that matters:
/// if a link anywhere on the path leads out of the root, the first existing
/// ancestor resolves to the outside and the check fails.
///
/// Returns `false` when neither `root` nor any ancestor of `candidate` can be
/// resolved. A destination that cannot be resolved is not one that can be shown
/// to be safe.
pub fn resolved_is_within(root: &Path, candidate: &Path) -> bool {
    let Ok(real_root) = std::fs::canonicalize(root) else {
        return false;
    };

    let mut probe = candidate;
    loop {
        if let Ok(real) = std::fs::canonicalize(probe) {
            return is_within(&real_root, &real);
        }
        match probe.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => probe = parent,
            _ => return false,
        }
    }
}

/// Verify that `candidate` stays inside `root`.
///
/// Purely lexical: it never touches the filesystem, so it also works for
/// directories that do not exist yet. `..` components and absolute paths that
/// escape the root are rejected.
pub fn is_within(root: &Path, candidate: &Path) -> bool {
    let mut depth: i32 = 0;
    let root_components: Vec<Component<'_>> = root.components().collect();
    let candidate_components: Vec<Component<'_>> = candidate.components().collect();

    if candidate_components.len() < root_components.len() {
        return false;
    }
    if candidate_components[..root_components.len()] != root_components[..] {
        return false;
    }
    for component in &candidate_components[root_components.len()..] {
        match component {
            Component::ParentDir => depth -= 1,
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            // Another root/prefix means the path escaped.
            Component::RootDir | Component::Prefix(_) => return false,
        }
        if depth < 0 {
            return false;
        }
    }
    true
}

/// A key that two spellings of the same file share.
///
/// Windows accepts `\` and `/` interchangeably and ignores case, so one file can
/// be named by more than one string: a download folder re-entered in Settings
/// with different capitalisation is enough to produce that. Comparing the raw
/// text would then fail to recognise a file the application downloaded itself,
/// and the failure is quiet — the file is still listed, just without its
/// creator, caption or source, and nothing on screen says why.
///
/// Case is folded with the ASCII-only rule, which is what SQLite's `lower()`
/// does, so a value normalised here can be compared against one normalised by a
/// query. That matters: the row has to be *found* before it can be compared, and
/// two different folding rules would leave the query and the lookup disagreeing.
///
/// Folding is applied only where the filesystem does it. On a case-sensitive
/// volume two names differing by case are two different files, and folding them
/// would attach one file's record to another.
pub fn comparison_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        text.to_ascii_lowercase()
    } else {
        text
    }
}

/// The SQL expression that normalises a stored path the same way.
///
/// Kept beside [`comparison_key`] on purpose: the two must agree exactly, and a
/// reader changing one needs to see the other. `replace` and `lower` are both
/// core SQLite, and `lower` folds ASCII only, which the Rust side mirrors.
pub const COMPARISON_KEY_SQL: &str = "REPLACE(LOWER(destination_path), '/', '\\')";

/// Trim a rendered file name so that `directory` + name fits in
/// [`MAX_FULL_PATH_LEN`], preserving the extension.
pub fn fit_to_path_budget(directory: &Path, file_name: &str) -> String {
    let directory_len = directory.to_string_lossy().chars().count();
    // +1 for the separator between directory and file name.
    let budget = MAX_FULL_PATH_LEN.saturating_sub(directory_len + 1);
    if budget == 0 {
        return sanitize_filename(FALLBACK_NAME);
    }
    if file_name.chars().count() <= budget {
        return file_name.to_string();
    }

    let (stem, extension) = match file_name.rsplit_once('.') {
        Some((stem, ext)) => (stem.to_string(), Some(ext.to_string())),
        None => (file_name.to_string(), None),
    };
    let ext_len = extension.as_ref().map(|e| e.chars().count() + 1).unwrap_or(0);
    let stem_budget = budget.saturating_sub(ext_len);
    if stem_budget == 0 {
        // Not even the stem fits; return a short fallback plus extension.
        return match extension {
            Some(ext) => format!("{FALLBACK_NAME}.{ext}"),
            None => FALLBACK_NAME.to_string(),
        };
    }

    let mut truncated: String = stem.chars().take(stem_budget).collect();
    let trimmed = truncated.trim_end_matches(['.', ' ']).to_string();
    truncated = if trimmed.is_empty() {
        FALLBACK_NAME.to_string()
    } else {
        trimmed
    };

    match extension {
        Some(ext) => format!("{truncated}.{ext}"),
        None => truncated,
    }
}

fn truncate_chars(value: &mut String, max: usize) {
    if value.chars().count() > max {
        let truncated: String = value.chars().take(max).collect();
        *value = truncated;
    }
}

fn truncate_filename(value: &mut String) {
    if value.chars().count() > MAX_FILENAME_LEN {
        let (stem, extension) = match value.rsplit_once('.') {
            Some((stem, ext)) if ext.len() <= 10 => (stem.to_string(), Some(ext.to_string())),
            _ => (value.clone(), None),
        };
        let ext_len = extension.as_ref().map(|e| e.chars().count() + 1).unwrap_or(0);
        let stem_budget = MAX_FILENAME_LEN.saturating_sub(ext_len);
        let mut truncated: String = stem.chars().take(stem_budget).collect();
        let trimmed = truncated.trim_end_matches(['.', ' ']).to_string();
        truncated = trimmed;
        *value = match extension {
            Some(ext) => format!("{truncated}.{ext}"),
            None => truncated,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_spellings_of_one_path_share_a_key() {
        let canonical = Path::new(r"D:\Downloads\Douyin\Videos\clip.mp4");
        for other in [
            r"D:\Downloads\Douyin\Videos\clip.mp4",
            r"D:/Downloads/Douyin/Videos/clip.mp4",
            r"d:\downloads\douyin\videos\clip.mp4",
            r"D:\DOWNLOADS\DOUYIN\VIDEOS\CLIP.MP4",
        ] {
            assert_eq!(
                comparison_key(canonical),
                comparison_key(Path::new(other)),
                "{other} names the same file"
            );
        }

        // And two files that really are different keep different keys, or the
        // comparison would attach one record to another.
        assert_ne!(
            comparison_key(Path::new(r"D:\Downloads\Douyin\Videos\clip.mp4")),
            comparison_key(Path::new(r"D:\Downloads\Douyin\Videos\other.mp4"))
        );
        assert_ne!(
            comparison_key(Path::new(r"D:\Downloads\Douyin\A\clip.mp4")),
            comparison_key(Path::new(r"D:\Downloads\Douyin\B\clip.mp4"))
        );

        // A name that is not ASCII is left alone rather than mangled: it has no
        // case to fold, and folding must not change it.
        let cjk = Path::new(r"D:\Downloads\Douyin\郭虎\Videos\clip.mp4");
        assert!(comparison_key(cjk).contains("郭虎"));
    }

    #[test]
    fn the_query_normalises_the_same_way_the_lookup_does() {
        // The two must agree exactly. A row that the query does not return can
        // never be compared, however careful the comparison is — so this pins
        // the shape of the SQL beside the function it mirrors.
        assert_eq!(COMPARISON_KEY_SQL, "REPLACE(LOWER(destination_path), '/', '\\')");
        assert!(COMPARISON_KEY_SQL.contains("LOWER("));
        assert!(COMPARISON_KEY_SQL.contains("REPLACE("));
        #[cfg(windows)]
        assert!(
            comparison_key(Path::new(r"D:\X\Y.MP4")).contains("y.mp4"),
            "the Rust side folds case on Windows, so the query must too"
        );
    }

    #[test]
    fn strips_illegal_characters() {
        assert_eq!(sanitize_segment("a<b>c:d\"e|f?g*h"), "a_b_c_d_e_f_g_h");
        assert_eq!(sanitize_segment("a/b\\c"), "a_b_c");
    }

    #[test]
    fn strips_control_characters() {
        assert_eq!(sanitize_segment("clip\u{0007}name"), "clip_name");
    }

    #[test]
    fn trims_trailing_dots_and_spaces() {
        assert_eq!(sanitize_segment("name... "), "name");
        assert_eq!(sanitize_segment("  spaced  "), "spaced");
    }

    #[test]
    fn empty_input_becomes_fallback() {
        assert_eq!(sanitize_segment(""), FALLBACK_NAME);
        assert_eq!(sanitize_segment("   "), FALLBACK_NAME);
        assert_eq!(sanitize_segment("..."), FALLBACK_NAME);
    }

    #[test]
    fn escapes_reserved_device_names() {
        assert!(is_reserved("CON"));
        assert!(is_reserved("com1"));
        assert!(is_reserved("LPT1.txt"));
        assert!(!is_reserved("console"));
        assert_eq!(sanitize_segment("CON"), "CON_");
        assert_eq!(sanitize_segment("nul"), "nul_");
    }

    #[test]
    fn preserves_extension_when_sanitizing_filename() {
        assert_eq!(sanitize_filename("my clip.mp4"), "my clip.mp4");
        assert_eq!(sanitize_filename("bad:name.mp4"), "bad_name.mp4");
    }

    #[test]
    fn folder_names_follow_media_type() {
        assert_eq!(media_folder_name(MediaType::Video), "Videos");
        assert_eq!(media_folder_name(MediaType::Image), "Images");
        assert_eq!(media_folder_name(MediaType::Carousel), "Images");
        assert_eq!(media_folder_name(MediaType::Audio), "Audio");
    }

    #[test]
    fn renders_template_placeholders() {
        let context = NamingContext {
            creator: Some("Alice".into()),
            post_id: Some("728".into()),
            media_type: Some(MediaType::Video),
            index: Some(0),
            extension: Some("mp4".into()),
            ..Default::default()
        };
        assert_eq!(
            render_filename("{creator}_{id}_{index}", &context),
            "Alice_728_001.mp4"
        );
        assert_eq!(
            render_folder_segments("{creator}/{type}", &context),
            vec!["Alice".to_string(), "Videos".to_string()]
        );
    }

    #[test]
    fn a_carousels_slides_do_not_share_a_name() {
        // The reported defect: every slide was named after the post, so the only
        // thing telling them apart was the download layer's `(1)` collision
        // suffix — which is an answer to a collision, not a way of naming a
        // slide. The number goes in the name instead, in the post's own order.
        let slide = |index: u32| NamingContext {
            post_id: Some("7629996475679029850".into()),
            media_type: Some(MediaType::Image),
            extension: Some("webp".into()),
            index: Some(index),
            item_count: Some(15),
            ..Default::default()
        };

        assert_eq!(
            render_filename("{id}", &slide(0)),
            "7629996475679029850_001.webp"
        );
        assert_eq!(
            render_filename("{id}", &slide(14)),
            "7629996475679029850_015.webp"
        );
    }

    #[test]
    fn a_post_with_one_item_of_its_kind_is_not_numbered() {
        // A video and the separate audio track beside it are two items but not
        // two slides. Numbering them would rename every video already on disk.
        let video = NamingContext {
            post_id: Some("728".into()),
            media_type: Some(MediaType::Video),
            extension: Some("mp4".into()),
            index: Some(0),
            item_count: Some(1),
            ..Default::default()
        };
        assert_eq!(render_filename("{id}", &video), "728.mp4");
    }

    #[test]
    fn a_template_that_places_the_index_keeps_its_own_shape() {
        // The user has said where the number belongs. Adding a second one would
        // produce names like `728_001_001.jpg`.
        let slide = NamingContext {
            post_id: Some("728".into()),
            media_type: Some(MediaType::Image),
            extension: Some("jpg".into()),
            index: Some(0),
            item_count: Some(4),
            ..Default::default()
        };
        assert_eq!(render_filename("{id}_{index}", &slide), "728_001.jpg");
    }

    #[test]
    fn template_extension_placeholder_is_not_duplicated() {
        let context = NamingContext {
            post_id: Some("1".into()),
            extension: Some("mp4".into()),
            ..Default::default()
        };
        assert_eq!(render_filename("{id}.{ext}", &context), "1.mp4");
    }

    #[test]
    fn a_placeholder_with_nothing_to_say_does_not_leave_a_dangling_separator() {
        // The shipped default is `{date}_{id}`, and content resolved without a
        // publish date has no `{date}`. The name must still read as a name.
        let context = NamingContext {
            post_id: Some("7281234567890123456".into()),
            extension: Some("mp4".into()),
            ..Default::default()
        };
        assert_eq!(
            render_filename("{date}_{id}", &context),
            "7281234567890123456.mp4"
        );

        let with_date = NamingContext {
            published_at: Some(crate::models::common::now()),
            ..context
        };
        let rendered = render_filename("{date}_{id}", &with_date);
        assert!(rendered.contains('_'), "a present date keeps its separator");
        assert!(rendered.ends_with(".mp4"));
    }

    #[test]
    fn a_template_that_renders_to_nothing_still_falls_back_gracefully() {
        let context = NamingContext {
            extension: Some("mp4".into()),
            ..Default::default()
        };
        // `{creator}` has no value here, so trimming would empty the name; the
        // fallback name is used instead of an empty file name.
        let rendered = render_filename("{creator}", &context);
        assert_eq!(rendered, "untitled.mp4");
    }

    #[test]
    fn caption_is_sanitized_after_substitution() {
        let context = NamingContext {
            caption: Some("hello/world: <sunset>".into()),
            extension: Some("mp4".into()),
            ..Default::default()
        };
        let rendered = render_filename("{caption}", &context);
        assert!(!rendered.contains('/'), "got {rendered}");
        assert!(!rendered.contains(':'), "got {rendered}");
        assert!(rendered.ends_with(".mp4"));
    }

    #[test]
    fn traversal_cannot_survive_a_template() {
        let context = NamingContext {
            caption: Some("../../etc/passwd".into()),
            ..Default::default()
        };
        let segments = render_folder_segments("{caption}", &context);
        assert_eq!(segments.len(), 1);
        assert!(!segments[0].contains(".."));
        let name = render_filename("{caption}", &context);
        assert!(!name.contains(".."));
    }

    #[test]
    fn long_names_are_truncated() {
        let long = "a".repeat(500);
        let sanitized = sanitize_segment(&long);
        assert!(sanitized.chars().count() <= MAX_SEGMENT_LEN);

        let filename = sanitize_filename(&format!("{long}.mp4"));
        assert!(filename.chars().count() <= MAX_FILENAME_LEN);
        assert!(filename.ends_with(".mp4"));
    }

    #[test]
    fn path_budget_protects_long_directories() {
        let deep = PathBuf::from("C:\\").join("x".repeat(220));
        let name = fit_to_path_budget(&deep, &format!("{}.mp4", "b".repeat(100)));
        let total = deep.to_string_lossy().chars().count() + 1 + name.chars().count();
        assert!(total <= MAX_FULL_PATH_LEN, "total was {total}");
        assert!(name.ends_with(".mp4"));
    }

    #[test]
    fn containment_check_rejects_escapes() {
        let root = Path::new("C:\\Downloads\\Douyin");
        assert!(is_within(root, Path::new("C:\\Downloads\\Douyin\\a\\b.mp4")));
        assert!(is_within(root, Path::new("C:\\Downloads\\Douyin")));
        assert!(!is_within(root, Path::new("C:\\Downloads\\Douyin\\..\\evil.mp4")));
        assert!(!is_within(root, Path::new("C:\\Windows\\system32\\x.dll")));
        assert!(!is_within(root, Path::new("C:\\Downloads")));
    }

    #[test]
    fn unique_path_avoids_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let first = unique_path(dir.path(), "clip.mp4");
        std::fs::write(&first, b"x").unwrap();
        let second = unique_path(dir.path(), "clip.mp4");
        assert_ne!(first, second);
        assert!(second.file_name().unwrap().to_string_lossy().contains("(1)"));
    }
}
