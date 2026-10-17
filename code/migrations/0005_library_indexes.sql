-- Migration 0005 — indexes for the Library's read patterns.
--
-- The Library lists posts joined with their creator, their media summary and
-- their newest download. Two of those joins had no index behind them:
--
--   * `downloads` had indexes on `state` and `created_at`, but not on
--     `post_id`, so "the newest download for this post" was a scan per post.
--   * `download_history` is searched by `post_id` when the Library matches a
--     local path, and only `downloaded_at` was indexed.
--
-- Both are added here rather than in a new table because nothing about the
-- schema changed — only how it is read.

CREATE INDEX idx_downloads_post_created ON downloads (post_id, created_at DESC);

CREATE INDEX idx_history_post_id_local_path ON download_history (post_id, local_path);
