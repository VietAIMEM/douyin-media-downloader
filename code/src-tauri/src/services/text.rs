//! Saving a post's words.
//!
//! A text post — and the caption of any post — is content in its own right. It
//! is deliberately *not* a download: there is no task, no transfer, no row in
//! the download history, and no entry in the queue. Writing a caption is a file
//! this application produces from data it already holds, and modelling it as a
//! download would put a "completed download" in the user's history for something
//! nothing was downloaded for.
//!
//! What goes in the file is what the source stated and nothing more: the text,
//! the post's address on the platform, its platform id, the creator when one is
//! known, and the publish time exactly as the page printed it. A field the
//! source did not provide is left out rather than filled with a placeholder, so
//! the file never claims a fact about the post that nobody established.

use std::path::PathBuf;

use serde::Serialize;

use crate::database::repositories::posts as posts_repo;
use crate::errors::{AppError, AppResult};
use crate::models::{Id, MediaType};
use crate::storage::filenames::{self, NamingContext};

use super::AppCore;

/// What writing a post's text produced.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedText {
    pub post_id: Id,
    /// Where the file was written.
    pub path: String,
    /// How many bytes it holds.
    pub bytes: u64,
    /// Whether an existing file was in the way and this one was named beside it.
    pub renamed: bool,
}

/// The text of a post, as the source gave it.
///
/// Falls back to the caption, which is where the same words live for a post that
/// also has pictures or a video. A post with neither is refused rather than
/// exported empty.
fn text_of(post: &crate::models::Post) -> Option<String> {
    post.metadata
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| post.caption.clone())
        .filter(|text| !text.trim().is_empty())
}

/// The file's contents.
///
/// Plain UTF-8, one field per line, with the body last so that a reader skimming
/// the top sees what this is before they see the words. Every line is one fact,
/// and a fact that is absent is a line that is absent.
fn render(post: &crate::models::Post, text: &str) -> String {
    let mut out = String::new();
    out.push_str("# Douyin post\n");
    out.push_str(&format!("platform_post_id: {}\n", post.platform_post_id));
    if let Some(creator) = post
        .creator_handle
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        out.push_str(&format!("creator: {creator}\n"));
    }
    if let Some(url) = post
        .canonical_url
        .as_deref()
        .or(Some(post.url.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        out.push_str(&format!("url: {url}\n"));
    }
    if let Some(published) = post
        .metadata
        .get("publishedAt")
        .and_then(serde_json::Value::as_str)
    {
        // Exactly as the page printed it, with no timezone invented for it.
        out.push_str(&format!("published (as stated by the source): {published}\n"));
    } else if let Some(published) = post.published_at {
        out.push_str(&format!("published: {}\n", published.to_rfc3339()));
    }
    out.push('\n');
    out.push_str(text.trim_end());
    if !text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Write a post's text to the download folder as a `.txt` file.
///
/// The location and the name come from the user's own settings — the same
/// folder template and filename template every download uses — so a saved
/// caption lands where its post's pictures would have, under a name the user
/// chose the shape of. `{type}` reads `Text` for it, which puts it in its own
/// folder beside the media rather than among it.
pub fn save_post_text(core: &AppCore, post_id: &str) -> AppResult<SavedText> {
    let post = core
        .database()
        .with_conn(|conn| posts_repo::get(conn, post_id))?
        .ok_or_else(|| {
            AppError::not_found("post", post_id)
                .with_user_message("That post is not held locally, so its text cannot be saved.")
        })?;

    let text = text_of(&post).ok_or_else(|| {
        AppError::unsupported("the post holds no text")
            .with_user_message("This post has no text to save.")
            .with_technical_detail(format!(
                "post_id={post_id} media_type={}",
                post.media_type
            ))
    })?;

    let settings = core.settings().current();
    let context = NamingContext {
        creator: None,
        handle: post.creator_handle.clone(),
        post_id: Some(post.platform_post_id.clone()),
        // Saved text is typed as text so `{type}` and the media-type folder
        // agree with what the file actually is, whatever the post also holds.
        media_type: Some(MediaType::Text),
        caption: post.caption.clone(),
        index: None,
        item_count: None,
        extension: Some("txt".to_string()),
        published_at: post.published_at,
    };

    let plan = core.storage().plan_destination(&context, &settings)?;
    let directory = PathBuf::from(&plan.directory);
    std::fs::create_dir_all(&directory).map_err(|err| {
        AppError::storage("the text folder could not be created")
            .with_user_message("The folder for saved text could not be created.")
            .with_technical_detail(directory.to_string_lossy().into_owned())
            .with_source(err)
    })?;

    // Never over an existing file. A caption saved twice, or a post whose text
    // was already written by hand, must not be silently replaced — the name
    // moves aside instead, exactly as a repeated download's does.
    let planned = PathBuf::from(&plan.full_path);
    let (destination, renamed) = if planned.exists() {
        (
            filenames::unique_path(&directory, &plan.file_name),
            true,
        )
    } else {
        (planned, false)
    };

    let contents = render(&post, &text);
    std::fs::write(&destination, contents.as_bytes()).map_err(|err| {
        AppError::storage("the post's text could not be written")
            .with_user_message("The text could not be saved to the download folder.")
            .with_technical_detail(destination.to_string_lossy().into_owned())
            .with_source(err)
    })?;

    let bytes = contents.len() as u64;
    let path = destination.to_string_lossy().into_owned();
    tracing::info!(
        target: "library",
        post_id = %post.platform_post_id,
        bytes,
        renamed,
        "post text saved"
    );

    Ok(SavedText {
        post_id: post.id,
        path,
        bytes,
        renamed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Post;

    fn text_post(text: &str) -> Post {
        let mut post = Post::new(
            crate::models::Platform::Douyin,
            "7629996475679029850",
            MediaType::Text,
            "https://www.douyin.com/note/7629996475679029850",
        );
        post.canonical_url = Some("https://www.douyin.com/note/7629996475679029850".into());
        post.caption = Some(text.to_string());
        post.metadata
            .insert("text".into(), serde_json::json!(text));
        post
    }

    #[test]
    fn the_file_holds_the_text_and_the_facts_that_were_stated() {
        let mut post = text_post("开机大吉！顺顺利利！\n第二行");
        post.creator_handle = Some("郭虎".into());
        post.metadata
            .insert("publishedAt".into(), serde_json::json!("2026-04-18T15:23:11"));

        let rendered = render(&post, "开机大吉！顺顺利利！\n第二行");

        assert!(rendered.starts_with("# Douyin post\n"));
        assert!(rendered.contains("platform_post_id: 7629996475679029850"));
        assert!(rendered.contains("creator: 郭虎"));
        assert!(rendered.contains("url: https://www.douyin.com/note/7629996475679029850"));
        // The wall-clock time the page printed, not converted into an instant.
        assert!(rendered.contains("published (as stated by the source): 2026-04-18T15:23:11"));
        // The body, whole, last.
        assert!(rendered.ends_with("开机大吉！顺顺利利！\n第二行\n"));
    }

    #[test]
    fn a_field_the_source_did_not_state_is_not_invented() {
        let post = text_post("just words");
        let rendered = render(&post, "just words");

        assert!(!rendered.contains("published"));
        assert!(!rendered.contains("creator:"));
        // The address is always known: the post was reached by one.
        assert!(rendered.contains("url: "));
    }

    #[test]
    fn the_text_is_the_body_and_not_the_caption_of_a_post_that_has_both() {
        // A post with pictures keeps its words in `text` when it has them; the
        // caption is the fallback, and both are the same words when there is
        // only one place they live.
        let mut post = text_post("caption words");
        post.metadata.insert("text".into(), serde_json::json!("the full text"));
        assert_eq!(text_of(&post).as_deref(), Some("the full text"));

        post.metadata.remove("text");
        assert_eq!(text_of(&post).as_deref(), Some("caption words"));
    }

    #[test]
    fn a_post_with_no_words_has_nothing_to_save() {
        let mut post = text_post("");
        post.caption = None;
        post.metadata.remove("text");
        assert_eq!(text_of(&post), None);
    }
}
