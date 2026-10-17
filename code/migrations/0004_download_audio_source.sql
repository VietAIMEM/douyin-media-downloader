-- Migration 0004 — record a second source track on a download task.
--
-- Some sources serve a video and its audio as separate renditions, so producing
-- one playable file means fetching two URLs and combining them. A task therefore
-- has to remember both, because a retry must fetch the same pair: re-resolving
-- mid-retry would either fail or, worse, quietly produce a silent video.
--
-- Signed media URLs are short-lived by nature. Storing one here is deliberate
-- and bounded: the row exists only for as long as the task does, and the value
-- is overwritten by the next resolution rather than accumulated.
--
-- Existing rows are unaffected: NULL means "this download has a single source",
-- which is what every earlier download was.

ALTER TABLE downloads ADD COLUMN audio_source_url TEXT;
