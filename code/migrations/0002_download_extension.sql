-- Migration 0002 — record the resolved media extension on a download task.
--
-- Why a column rather than deriving it at download time: the extension belongs
-- to the *resolved media variant*, not to the URL. Douyin's playback URLs carry
-- no file extension at all (`.../aweme/v1/play/?video_id=...`), so parsing it
-- back out of the source URL would be guesswork. Persisting what the resolver
-- determined keeps the naming policy (`{ext}`, and the suffix appended when a
-- template omits it) working across an application restart.
--
-- Existing rows are unaffected: NULL simply means "unknown", and the storage
-- layer falls back to the media-type default in that case.

ALTER TABLE downloads ADD COLUMN extension TEXT;
