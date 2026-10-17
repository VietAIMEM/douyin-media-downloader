//! Database integration tests: schema creation, migrations, and the round trip
//! of each aggregate through its repository.
//!
//! These drive the real `Database` against real SQLite files, because the point
//! of the persistence layer is precisely the thing an in-memory stand-in would
//! not exercise: that a second process opening the same file sees what the
//! first one wrote.

use douyin_downloader_lib::database::{
    migrations,
    repositories::{
        creators as creators_repo, downloads as downloads_repo, history as history_repo,
        media as media_repo, posts as posts_repo, settings as settings_repo,
    },
    Database, TRACKED_TABLES,
};
use douyin_downloader_lib::errors::AppErrorKind;
use douyin_downloader_lib::models::{
    ContentStatus, Creator, DownloadHistoryEntry, DownloadRequest, DownloadState, DownloadTask,
    MediaItem, MediaType, MediaVariant, Platform, Post, Timestamp, VariantKind,
};

fn temp_database() -> (tempfile::TempDir, Database) {
    let temp = tempfile::tempdir().expect("temp dir");
    let database = Database::open(temp.path().join("test.db")).expect("open database");
    (temp, database)
}

// --- schema and migrations --------------------------------------------------

#[test]
fn a_fresh_database_applies_every_migration_and_creates_the_tracked_tables() {
    let (_temp, database) = temp_database();

    let status = database.status().expect("status");
    assert!(status.healthy);
    assert!(status.is_current(), "schema must be at the newest migration");
    assert_eq!(status.schema_version, database.latest_schema_version());
    assert!(!status.applied_migrations.is_empty());

    // Every table the project specifies must exist and be queryable.
    let reported: Vec<&str> = status.table_counts.iter().map(|t| t.table.as_str()).collect();
    for table in ["creators", "posts", "media_items", "downloads", "download_history", "settings"] {
        assert!(reported.contains(&table), "{table} is not tracked");
        assert!(
            TRACKED_TABLES.contains(&table),
            "{table} must be part of the tracked set"
        );
    }

    // Querying each one is the real proof that the table was created.
    for table in TRACKED_TABLES {
        let count = database
            .with_conn(|conn| {
                Ok(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })?)
            })
            .unwrap_or_else(|err| panic!("{table} is not queryable: {err}"));
        assert_eq!(count, 0, "{table} starts empty");
    }
}

#[test]
fn opening_an_existing_database_does_not_re_apply_migrations() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("test.db");

    let first = Database::open(&path).unwrap();
    let before = first.status().unwrap();
    drop(first);

    // Every startup runs the migration step again; it must be a no-op rather
    // than duplicating rows in `schema_migrations`.
    let second = Database::open(&path).unwrap();
    let after = second.status().unwrap();

    assert_eq!(before.schema_version, after.schema_version);
    assert_eq!(
        before.applied_migrations.len(),
        after.applied_migrations.len(),
        "re-running must not duplicate migration records"
    );
    assert!(after.is_current());
}

#[test]
fn the_applied_migrations_are_recorded_with_a_version_and_description() {
    let (_temp, database) = temp_database();

    let applied = database
        .with_conn(|conn| migrations::applied_versions(conn))
        .expect("applied versions");

    assert!(!applied.is_empty());
    let (version, description, applied_at) = &applied[0];
    assert!(*version >= 1);
    assert!(!description.trim().is_empty());
    assert!(
        !applied_at.trim().is_empty(),
        "each migration records when it was applied"
    );

    // Versions must be unique and ascending; a duplicate would mean two
    // migrations share a number.
    let versions: Vec<i64> = applied.iter().map(|(v, _, _)| *v).collect();
    let mut sorted = versions.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(versions, sorted);
}

#[test]
fn an_in_memory_database_is_reported_as_non_persistent() {
    let database = Database::open_in_memory().expect("in-memory database");
    let status = database.status().unwrap();

    assert!(status.healthy);
    assert!(!status.is_persistent);
    assert_eq!(status.location, ":memory:");
}

// --- settings ---------------------------------------------------------------

#[test]
fn settings_values_are_written_read_updated_and_survive_reopening() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("test.db");

    {
        let database = Database::open(&path).unwrap();
        database
            .with_conn(|conn| settings_repo::set(conn, "download_directory", "C:/Downloads/Douyin"))
            .unwrap();
        database
            .with_conn(|conn| settings_repo::set(conn, "max_concurrent_downloads", "3"))
            .unwrap();

        // Update an existing key.
        database
            .with_conn(|conn| settings_repo::set(conn, "max_concurrent_downloads", "5"))
            .unwrap();

        let value = database
            .with_conn(|conn| settings_repo::get(conn, "max_concurrent_downloads"))
            .unwrap();
        assert_eq!(value.as_deref(), Some("5"));

        let all = database.with_conn(|conn| settings_repo::all(conn)).unwrap();
        assert_eq!(all.len(), 2, "updating a key must not insert a second row");

        let missing = database
            .with_conn(|conn| settings_repo::get(conn, "not_a_setting"))
            .unwrap();
        assert!(missing.is_none(), "an unknown key is absent, not an error");
    }

    // Reopen: this is what "persists across restarts" means.
    let reopened = Database::open(&path).unwrap();
    let value = reopened
        .with_conn(|conn| settings_repo::get(conn, "download_directory"))
        .unwrap();
    assert_eq!(value.as_deref(), Some("C:/Downloads/Douyin"));
    assert_eq!(reopened.schema_version().unwrap(), reopened.latest_schema_version());
}

#[test]
fn settings_can_be_deleted() {
    let (_temp, database) = temp_database();

    database
        .with_conn(|conn| settings_repo::set(conn, "theme", "dark"))
        .unwrap();
    assert!(database
        .with_conn(|conn| settings_repo::delete(conn, "theme"))
        .unwrap());
    assert!(database
        .with_conn(|conn| settings_repo::get(conn, "theme"))
        .unwrap()
        .is_none());
    assert!(!database
        .with_conn(|conn| settings_repo::delete(conn, "theme"))
        .unwrap());
}

// --- creators ---------------------------------------------------------------

#[test]
fn creators_are_upserted_by_platform_identity() {
    let (_temp, database) = temp_database();

    let mut creator = Creator::new(
        Platform::Douyin,
        "MS4wLjABAAAAexample",
        "Example Creator",
        "https://www.douyin.com/user/MS4wLjABAAAAexample",
    );
    creator.handle = Some("@example".into());
    creator.post_count = Some(42);

    let first_id = database
        .with_conn(|conn| creators_repo::upsert(conn, &creator))
        .unwrap();

    // Same platform identity, new display name: must update, not duplicate.
    creator.display_name = "Renamed Creator".into();
    let second_id = database
        .with_conn(|conn| creators_repo::upsert(conn, &creator))
        .unwrap();
    assert_eq!(first_id, second_id);

    database
        .with_conn(|conn| {
            assert_eq!(creators_repo::count(conn)?, 1);
            let stored = creators_repo::get(conn, &first_id)?.expect("creator exists");
            assert_eq!(stored.display_name, "Renamed Creator");
            assert_eq!(stored.handle.as_deref(), Some("@example"));
            assert_eq!(stored.post_count, Some(42));

            let by_platform = creators_repo::get_by_platform_id(
                conn,
                Platform::Douyin,
                "MS4wLjABAAAAexample",
            )?
            .expect("lookup by platform id");
            assert_eq!(by_platform.id, first_id);
            Ok(())
        })
        .unwrap();
}

// --- posts, media, and variants --------------------------------------------

#[test]
fn posts_round_trip_with_their_media_items_and_variants() {
    let (_temp, database) = temp_database();

    let mut post = Post::new(
        Platform::Douyin,
        "7300000000000000000",
        MediaType::Video,
        "https://www.douyin.com/video/7300000000000000000",
    );
    post.canonical_url = Some("https://www.douyin.com/video/7300000000000000000".into());
    post.caption = Some("A fixture caption".into());
    post.status = ContentStatus::Ready;

    let mut media = MediaItem::new(
        post.id.clone(),
        MediaType::Video,
        "https://cdn.example/standard.mp4",
    );
    media.extension = Some("mp4".into());
    media.width = Some(720);
    media.height = Some(1280);

    let mut original = MediaVariant::new(
        media.id.clone(),
        VariantKind::Original,
        "Original",
        "https://cdn.example/original.mp4",
    );
    original.is_original = true;
    let standard = MediaVariant::new(
        media.id.clone(),
        VariantKind::Standard,
        "720p",
        "https://cdn.example/standard.mp4",
    );
    media.variants = vec![original, standard];
    post.media_items = vec![media];

    let post_id = database
        .with_conn(|conn| {
            let id = posts_repo::upsert(conn, &post)?;
            posts_repo::save_media_items(conn, &id, &post.media_items)?;
            Ok(id)
        })
        .unwrap();

    database
        .with_conn(|conn| {
            let stored = posts_repo::get(conn, &post_id)?.expect("post exists");
            assert_eq!(stored.caption.as_deref(), Some("A fixture caption"));
            assert_eq!(stored.media_type, MediaType::Video);
            assert_eq!(stored.status, ContentStatus::Ready);
            assert_eq!(stored.media_items.len(), 1);

            let item = &stored.media_items[0];
            assert_eq!(item.extension.as_deref(), Some("mp4"));
            assert_eq!(item.width, Some(720));
            assert_eq!(item.variants.len(), 2);

            // The original/clean source is represented as a variant, which is
            // how later phases express "prefer the original when offered".
            assert_eq!(item.preferred_variant().kind, VariantKind::Original);
            assert!(item.preferred_variant().is_original);
            assert_eq!(media_repo::count_items(conn)?, 1);
            assert_eq!(media_repo::count_variants(conn)?, 2);
            Ok(())
        })
        .unwrap();

    // Saving media again replaces rather than duplicates.
    database
        .with_conn(|conn| posts_repo::save_media_items(conn, &post_id, &post.media_items))
        .unwrap();
    database
        .with_conn(|conn| {
            assert_eq!(media_repo::count_items(conn)?, 1);
            assert_eq!(media_repo::count_variants(conn)?, 2);
            Ok(())
        })
        .unwrap();

    // Post identity is `(platform, platform_post_id)`, not the local id.
    let by_platform = database
        .with_conn(|conn| {
            posts_repo::get_by_platform_id(conn, Platform::Douyin, "7300000000000000000")
        })
        .unwrap()
        .expect("lookup by platform id");
    assert_eq!(by_platform.id, post_id);
}

#[test]
fn a_post_status_can_be_updated_independently_of_its_mutable_fields() {
    let (_temp, database) = temp_database();

    let post = Post::new(
        Platform::Douyin,
        "7300000000000000001",
        MediaType::Image,
        "https://www.douyin.com/note/7300000000000000001",
    );
    let post_id = database
        .with_conn(|conn| posts_repo::upsert(conn, &post))
        .unwrap();

    database
        .with_conn(|conn| posts_repo::set_status(conn, &post_id, ContentStatus::Unavailable))
        .unwrap();
    database
        .with_conn(|conn| {
            let stored = posts_repo::get(conn, &post_id)?.unwrap();
            assert_eq!(stored.status, ContentStatus::Unavailable);
            Ok(())
        })
        .unwrap();
}

// --- downloads --------------------------------------------------------------

#[test]
fn download_tasks_are_persisted_and_listed_by_state() {
    let (_temp, database) = temp_database();

    let request = DownloadRequest::new("https://cdn.example/video.mp4", MediaType::Video)
        .with_post("post-1");
    let mut task = DownloadTask::new(&request);

    assert_eq!(task.state, DownloadState::Queued);
    database
        .with_conn(|conn| downloads_repo::insert(conn, &task))
        .unwrap();
    assert_eq!(database.with_conn(|conn| downloads_repo::count(conn)).unwrap(), 1);
    assert_eq!(
        database
            .with_conn(|conn| downloads_repo::list_by_state(conn, DownloadState::Queued))
            .unwrap()
            .len(),
        1
    );

    // Move it to a terminal state and persist that.
    task.state = DownloadState::Completed;
    task.attempts = 1;
    task.destination_path = Some("C:/Downloads/Douyin/Alice/Videos/clip.mp4".into());
    task.completed_at = Some(douyin_downloader_lib::models::common::now());

    database
        .with_conn(|conn| downloads_repo::update(conn, &task))
        .unwrap();
    assert_eq!(
        database
            .with_conn(|conn| downloads_repo::list_by_state(conn, DownloadState::Queued))
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        database.with_conn(|conn| downloads_repo::count_completed(conn)).unwrap(),
        1
    );

    database
        .with_conn(|conn| {
            let stored = downloads_repo::get(conn, &task.id)?.expect("task exists");
            assert_eq!(stored.state, DownloadState::Completed);
            assert_eq!(stored.attempts, 1);
            assert_eq!(
                stored.destination_path.as_deref(),
                Some("C:/Downloads/Douyin/Alice/Videos/clip.mp4")
            );
            Ok(())
        })
        .unwrap();

    // Clearing terminal rows leaves queued work alone.
    let queued = DownloadTask::new(&request);
    database
        .with_conn(|conn| downloads_repo::insert(conn, &queued))
        .unwrap();
    let removed = database
        .with_conn(|conn| downloads_repo::clear_terminal(conn))
        .unwrap();
    assert_eq!(removed, 1);
    assert_eq!(database.with_conn(|conn| downloads_repo::count(conn)).unwrap(), 1);
}

#[test]
fn interrupted_work_is_rediscoverable_after_a_restart() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("test.db");
    let request = DownloadRequest::new("https://cdn.example/video.mp4", MediaType::Video);
    let mut task = DownloadTask::new(&request);
    task.state = DownloadState::Downloading;
    let task_id = task.id.clone();

    {
        let database = Database::open(&path).unwrap();
        database
            .with_conn(|conn| downloads_repo::insert(conn, &task))
            .unwrap();
    }

    // A fresh connection to the same file must still see the in-flight task,
    // which is what lets the manager requeue it on startup.
    let reopened = Database::open(&path).unwrap();
    let pending = reopened
        .with_conn(|conn| downloads_repo::list_pending(conn))
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, task_id);
    assert_eq!(pending[0].state, DownloadState::Downloading);
}

// --- history ----------------------------------------------------------------

#[test]
fn history_records_outcomes_and_is_listed_newest_first() {
    let (_temp, database) = temp_database();

    // Explicit timestamps keep the ordering assertion deterministic; two
    // entries inserted in the same instant would be free to come back in
    // either order.
    let older: Timestamp = "2024-01-01T00:00:00Z".parse().unwrap();
    let newer: Timestamp = "2024-06-01T00:00:00Z".parse().unwrap();

    let mut ok = DownloadHistoryEntry::completed(
        "https://cdn.example/clip.mp4",
        MediaType::Video,
        "C:/Downloads/Douyin/clip.mp4",
    );
    ok.downloaded_at = Some(older);

    let mut failed = DownloadHistoryEntry::failed(
        "https://cdn.example/broken.mp4",
        MediaType::Video,
        "connection reset",
    );
    failed.downloaded_at = Some(newer);

    database
        .with_conn(|conn| {
            history_repo::insert(conn, &ok)?;
            history_repo::insert(conn, &failed)?;
            Ok(())
        })
        .unwrap();

    database
        .with_conn(|conn| {
            assert_eq!(history_repo::count(conn)?, 2);

            let entries = history_repo::list(conn, 10, 0)?;
            assert_eq!(entries.len(), 2);
            assert_eq!(
                entries[0].downloaded_at, Some(newer),
                "the most recent download is listed first"
            );
            assert_eq!(entries[1].downloaded_at, Some(older));

            let failures = history_repo::list_by_status(conn, DownloadState::Failed)?;
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].error.as_deref(), Some("connection reset"));
            assert!(failures[0].local_path.is_none());

            let completed = history_repo::list_by_status(conn, DownloadState::Completed)?;
            assert_eq!(completed.len(), 1);
            assert!(completed[0].was_successful());

            // Duplicate detection is a query, not a scan of the whole table.
            assert!(history_repo::has_successful_download(conn, "https://cdn.example/clip.mp4")?);
            assert!(!history_repo::has_successful_download(conn, "https://cdn.example/other.mp4")?);
            Ok(())
        })
        .unwrap();
}

// --- error conversion -------------------------------------------------------

#[test]
fn a_failing_statement_becomes_a_database_error() {
    let (_temp, database) = temp_database();

    let err = database
        .with_conn(|conn| {
            conn.execute("SELECT * FROM a_table_that_does_not_exist", [])?;
            Ok(())
        })
        .expect_err("the statement must fail");

    assert_eq!(
        err.kind(),
        AppErrorKind::Database,
        "raw rusqlite failures must not fall through as `unknown`"
    );
    assert!(
        !err.user_message().to_lowercase().contains("no such table"),
        "the user message must not leak SQL details: {}",
        err.user_message()
    );
}

#[test]
fn a_constraint_violation_is_reported_as_a_database_error() {
    let (_temp, database) = temp_database();

    // `id` is the primary key, so inserting the same row twice must fail.
    database
        .with_conn(|conn| settings_repo::set(conn, "theme", "dark"))
        .unwrap();

    let err = database
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO settings (key, value, updated_at) VALUES ('theme', 'light', 'now')",
                [],
            )?;
            Ok(())
        })
        .expect_err("duplicate primary key must fail");

    assert_eq!(err.kind(), AppErrorKind::Database);
}
// --- profile persistence ----------------------------------------------------
//
// A profile listing is a *refresh*: the same creator and posts are seen again,
// usually with less detail than the first time. These cover what that must not
// cost the user.

/// A creator and one post, as a profile listing would deliver them.
fn listing_fixture() -> (Creator, Post) {
    let creator = Creator::new(
        Platform::Douyin,
        "MS4wLjABAAAAexample",
        "郭虎",
        "https://www.douyin.com/user/MS4wLjABAAAAexample",
    );
    let mut post = Post::new(
        Platform::Douyin,
        "7091303675863321870",
        MediaType::Video,
        "https://www.douyin.com/video/7091303675863321870",
    );
    post.creator_id = Some(creator.id.clone());
    post.caption = Some("第一条".into());
    post.thumbnail_url = Some("https://p3.douyinpic.com/a.jpeg".into());
    (creator, post)
}

#[test]
fn a_listed_creator_and_post_are_persisted_and_reload_identically() {
    let (_temp, database) = temp_database();
    let (creator, post) = listing_fixture();

    let (creator_id, post_id) = database
        .with_conn(|conn| {
            let creator_id = creators_repo::upsert(conn, &creator)?;
            let mut post = post.clone();
            post.creator_id = Some(creator_id.clone());
            let post_id = posts_repo::upsert_from_listing(conn, &post)?;
            Ok((creator_id, post_id))
        })
        .unwrap();

    // The post is associated with its creator and survives a reload.
    let stored = database
        .with_conn(|conn| posts_repo::get(conn, &post_id))
        .unwrap()
        .expect("post stored");
    assert_eq!(stored.creator_id.as_deref(), Some(creator_id.as_str()));
    assert_eq!(stored.platform_post_id, "7091303675863321870");
    assert_eq!(stored.caption.as_deref(), Some("第一条"));

    let listed = database
        .with_conn(|conn| posts_repo::list_by_creator(conn, &creator_id, 50, 0))
        .unwrap();
    assert_eq!(listed.len(), 1);

    let creator_stored = database
        .with_conn(|conn| {
            creators_repo::get_by_platform_id(conn, Platform::Douyin, "MS4wLjABAAAAexample")
        })
        .unwrap()
        .expect("creator stored");
    assert_eq!(creator_stored.display_name, "郭虎");
}

#[test]
fn re_visiting_a_profile_reuses_the_creator_and_the_post_rather_than_duplicating_them() {
    let (_temp, database) = temp_database();
    let (creator, post) = listing_fixture();

    let first = database
        .with_conn(|conn| {
            let creator_id = creators_repo::upsert(conn, &creator)?;
            let mut post = post.clone();
            post.creator_id = Some(creator_id);
            posts_repo::upsert_from_listing(conn, &post)
        })
        .unwrap();

    // Second visit: same identity, refreshed metadata.
    let second = database
        .with_conn(|conn| {
            let creator_id = creators_repo::upsert(conn, &creator)?;
            let mut post = post.clone();
            post.creator_id = Some(creator_id);
            post.caption = Some("更新的标题".into());
            posts_repo::upsert_from_listing(conn, &post)
        })
        .unwrap();

    assert_eq!(first, second, "a refresh must not mint a second identity");
    assert_eq!(
        database.with_conn(|conn| creators_repo::count(conn)).unwrap(),
        1
    );
    assert_eq!(database.with_conn(|conn| posts_repo::count(conn)).unwrap(), 1);
    assert_eq!(
        database
            .with_conn(|conn| posts_repo::get(conn, &first))
            .unwrap()
            .unwrap()
            .caption
            .as_deref(),
        Some("更新的标题")
    );
}

#[test]
fn a_listing_refresh_does_not_erase_a_stored_download_status() {
    let (_temp, database) = temp_database();
    let (creator, post) = listing_fixture();

    let post_id = database
        .with_conn(|conn| {
            let creator_id = creators_repo::upsert(conn, &creator)?;
            let mut post = post.clone();
            post.creator_id = Some(creator_id);
            posts_repo::upsert_from_listing(conn, &post)
        })
        .unwrap();

    // The download happened.
    database
        .with_conn(|conn| posts_repo::set_status(conn, &post_id, ContentStatus::Downloaded))
        .unwrap();

    // The profile is browsed again, and the listing reports the post as merely
    // new — because a listing knows nothing about local state.
    database
        .with_conn(|conn| {
            let mut refreshed = post.clone();
            refreshed.status = ContentStatus::New;
            refreshed.caption = None;
            posts_repo::upsert_from_listing(conn, &refreshed)
        })
        .unwrap();

    let stored = database
        .with_conn(|conn| posts_repo::get(conn, &post_id))
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.status,
        ContentStatus::Downloaded,
        "a refresh must not turn a downloaded post back into a new one"
    );
    // And a caption the listing omitted does not blank the stored one.
    assert_eq!(stored.caption.as_deref(), Some("第一条"));
}

#[test]
fn a_listing_can_state_a_status_that_says_more_than_new() {
    let (_temp, database) = temp_database();
    let (creator, post) = listing_fixture();

    let post_id = database
        .with_conn(|conn| {
            let creator_id = creators_repo::upsert(conn, &creator)?;
            let mut post = post.clone();
            post.creator_id = Some(creator_id);
            posts_repo::upsert_from_listing(conn, &post)
        })
        .unwrap();

    database
        .with_conn(|conn| {
            let mut gone = post.clone();
            // The source itself now reports the post as unavailable.
            gone.status = ContentStatus::Unavailable;
            posts_repo::upsert_from_listing(conn, &gone)
        })
        .unwrap();

    assert_eq!(
        database
            .with_conn(|conn| posts_repo::get(conn, &post_id))
            .unwrap()
            .unwrap()
            .status,
        ContentStatus::Unavailable
    );
}

#[test]
fn a_listing_upsert_keeps_the_local_row_and_its_resolved_media() {
    let (_temp, database) = temp_database();
    let (creator, post) = listing_fixture();

    // First the post is resolved properly, with media.
    let mut resolved = post.clone();
    resolved.status = ContentStatus::Ready;
    let mut media = MediaItem::new(
        resolved.id.clone(),
        MediaType::Video,
        "https://cdn.example/v.mp4",
    );
    media.extension = Some("mp4".into());
    resolved.media_items = vec![media];

    let (post_id, media_id) = database
        .with_conn(|conn| {
            let creator_id = creators_repo::upsert(conn, &creator)?;
            resolved.creator_id = Some(creator_id);
            let post_id = posts_repo::upsert(conn, &resolved)?;
            let mut items = resolved.media_items.clone();
            for item in &mut items {
                item.post_id = post_id.clone();
            }
            media_repo::replace_items_for_post(conn, &post_id, &items)?;
            Ok((post_id, items[0].id.clone()))
        })
        .unwrap();

    // Then the profile is browsed again and the listing restates the post.
    let listed_id = database
        .with_conn(|conn| posts_repo::upsert_from_listing(conn, &post))
        .unwrap();

    assert_eq!(listed_id, post_id, "the resolved row must be reused");
    let stored = database
        .with_conn(|conn| posts_repo::get(conn, &post_id))
        .unwrap()
        .unwrap();
    assert_eq!(stored.media_items.len(), 1, "resolved media must survive");
    assert_eq!(stored.media_items[0].id, media_id);
    assert_eq!(stored.status, ContentStatus::Ready, "the resolved status stands");
}

#[test]
fn the_latest_download_for_a_post_is_what_a_listing_reads() {
    let (_temp, database) = temp_database();
    let (_, post) = listing_fixture();

    assert!(database
        .with_conn(|conn| downloads_repo::latest_for_post(conn, &post.id))
        .unwrap()
        .is_none());

    let mut first = DownloadTask::new(&DownloadRequest::new(
        "https://cdn.example/a.mp4",
        MediaType::Video,
    ));
    first.post_id = Some(post.id.clone());
    first.media_item_id = Some("media-a".into());
    first.state = DownloadState::Failed;

    let mut second = DownloadTask::new(&DownloadRequest::new(
        "https://cdn.example/b.mp4",
        MediaType::Video,
    ));
    second.post_id = Some(post.id.clone());
    second.media_item_id = Some("media-b".into());
    second.state = DownloadState::Completed;
    second.completed_at = Some("2026-01-02T00:00:00Z".parse::<Timestamp>().unwrap());

    database
        .with_conn(|conn| {
            downloads_repo::insert(conn, &first)?;
            downloads_repo::insert(conn, &second)
        })
        .unwrap();
    // Make the completed one the more recent of the two.
    database
        .with_conn(|conn| {
            conn.execute(
                "UPDATE downloads SET created_at = '2099-01-01T00:00:00Z' WHERE id = ?1",
                [second.id.clone()],
            )?;
            Ok(())
        })
        .unwrap();

    let latest = database
        .with_conn(|conn| downloads_repo::latest_for_post(conn, &post.id))
        .unwrap()
        .expect("a task");
    assert_eq!(latest.id, second.id);
    assert_eq!(latest.state, DownloadState::Completed);

    // And the duplicate-detection queries see only what they should.
    assert!(database
        .with_conn(|conn| downloads_repo::active_for_media(conn, "media-b"))
        .unwrap()
        .is_none());
    assert_eq!(
        database
            .with_conn(|conn| downloads_repo::completed_for_media(conn, "media-b"))
            .unwrap()
            .expect("completed task")
            .id,
        second.id
    );
    assert!(database
        .with_conn(|conn| downloads_repo::active_for_media(conn, "media-a"))
        .unwrap()
        .is_none());
}

#[test]
fn an_in_flight_task_is_visible_to_duplicate_detection() {
    let (_temp, database) = temp_database();
    let mut queued = DownloadTask::new(&DownloadRequest::new(
        "https://cdn.example/a.mp4",
        MediaType::Video,
    ));
    queued.media_item_id = Some("media-a".into());

    database
        .with_conn(|conn| downloads_repo::insert(conn, &queued))
        .unwrap();

    let active = database
        .with_conn(|conn| downloads_repo::active_for_media(conn, "media-a"))
        .unwrap()
        .expect("queued task counts as in flight");
    assert_eq!(active.id, queued.id);
    assert!(database
        .with_conn(|conn| downloads_repo::completed_for_media(conn, "media-a"))
        .unwrap()
        .is_none());
}

#[test]
fn a_post_count_is_reported_per_creator() {
    let (_temp, database) = temp_database();
    let (creator, post) = listing_fixture();

    let creator_id = database
        .with_conn(|conn| {
            let creator_id = creators_repo::upsert(conn, &creator)?;
            let mut post = post.clone();
            post.creator_id = Some(creator_id.clone());
            posts_repo::upsert_from_listing(conn, &post)?;
            let mut other = Post::new(
                Platform::Douyin,
                "7473536022312144155",
                MediaType::Carousel,
                "https://www.douyin.com/note/7473536022312144155",
            );
            other.creator_id = Some(creator_id.clone());
            posts_repo::upsert_from_listing(conn, &other)?;
            Ok(creator_id)
        })
        .unwrap();

    assert_eq!(
        database
            .with_conn(|conn| posts_repo::count_by_creator(conn, &creator_id))
            .unwrap(),
        2
    );
}
