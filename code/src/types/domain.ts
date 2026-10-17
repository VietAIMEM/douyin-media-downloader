/**
 * Types mirroring the Rust domain models.
 *
 * These are hand-written rather than generated, so they are checked against the
 * backend by the round-trip tests rather than by a code generator. Field names
 * match the `camelCase` wire format produced by `#[serde(rename_all)]`.
 *
 * Enum values are the *stable lowercase strings* the Rust `string_enum!` macro
 * emits — they are part of the database format, so they must not be renamed
 * here without a backend migration.
 */

export type Id = string;
export type Timestamp = string;

export type Platform = "douyin" | "unknown";
export type MediaType = "video" | "image" | "carousel" | "audio" | "text";

/** Availability of the *source* content. */
export type ContentStatus =
  | "new"
  | "ready"
  | "downloading"
  | "downloaded"
  | "failed"
  | "unavailable"
  | "private"
  | "unsupported";

/** State of a *local download operation*. */
export type DownloadState =
  | "queued"
  | "downloading"
  | "paused"
  | "completed"
  | "failed"
  | "cancelled";

/**
 * What is known about a file on disk.
 *
 * Three answers, and the third is the point: `unavailable` means the question
 * could not be answered — a drive that is not attached, a directory this process
 * may not read, a path recorded under a download folder the user has since
 * changed. Reporting any of those as `missing` would be asserting that somebody
 * checked, when nobody could.
 *
 * Lives here with the other state vocabularies rather than beside the wire
 * format, because both the Library and the file scan describe files with it.
 */
export type FileState = "present" | "missing" | "unavailable";

export type VariantKind =
  | "original"
  | "standard"
  | "high"
  | "low"
  | "audio_only"
  | "other";

export type ConflictPolicy = "rename" | "overwrite" | "skip";
export type Theme = "system" | "light" | "dark";
export type LogLevel = "error" | "warn" | "info" | "debug" | "trace";

/** Free-form metadata carried through from the resolver layer. */
export type Metadata = Record<string, unknown>;

export interface MediaVariant {
  id: Id;
  mediaItemId: Id;
  kind: VariantKind;
  label: string;
  sourceUrl: string;
  mimeType: string | null;
  extension: string | null;
  width: number | null;
  height: number | null;
  fileSize: number | null;
  bitrateKbps: number | null;
  isOriginal: boolean;
}

export interface MediaItem {
  id: Id;
  postId: Id;
  mediaType: MediaType;
  sourceUrl: string;
  thumbnailUrl: string | null;
  mimeType: string | null;
  extension: string | null;
  width: number | null;
  height: number | null;
  durationSeconds: number | null;
  fileSize: number | null;
  variants: MediaVariant[];
  metadata: Metadata;
}

/**
 * What tracks a media item was found to have.
 *
 * Douyin serves a video and its audio as two separate renditions, so "a video"
 * and "a video with sound" are genuinely different findings.
 *
 * `video_only` and `unknown` are deliberately distinct: the first means the
 * source was watched and offers no audio, the second means it was not looked
 * for. Only the first is a statement about the content.
 */
export type TrackLayout = "audio_video" | "video_only" | "audio_only" | "unknown";

/** The track layout the backend recorded for an item, if it recorded one. */
export function mediaTrackLayout(item: MediaItem): TrackLayout | null {
  const value = (item.metadata as Record<string, unknown> | undefined)?.track_layout;
  if (value === "audio_video" || value === "video_only" || value === "audio_only" || value === "unknown") {
    return value;
  }
  return null;
}

export interface Creator {
  id: Id;
  platform: Platform;
  platformId: string;
  handle: string | null;
  displayName: string;
  avatarUrl: string | null;
  description: string | null;
  verified: boolean;
  postCount: number | null;
  profileUrl: string;
  metadata: Metadata;
  createdAt: Timestamp | null;
  updatedAt: Timestamp | null;
}

export interface Post {
  id: Id;
  platform: Platform;
  platformPostId: string;
  creatorId: Id | null;
  creatorHandle: string | null;
  url: string;
  canonicalUrl: string | null;
  caption: string | null;
  mediaType: MediaType;
  thumbnailUrl: string | null;
  publishedAt: Timestamp | null;
  status: ContentStatus;
  mediaItems: MediaItem[];
  metadata: Metadata;
  createdAt: Timestamp | null;
  updatedAt: Timestamp | null;
}

export interface DownloadProgress {
  downloadedBytes: number;
  totalBytes: number | null;
}

export interface DownloadTask {
  id: Id;
  postId: Id | null;
  mediaItemId: Id | null;
  variantId: Id | null;
  sourceUrl: string;
  destinationPath: string | null;
  fileName: string | null;
  /** Extension of the resolved media, without the dot. */
  extension: string | null;
  mediaType: MediaType;
  state: DownloadState;
  attempts: number;
  maxAttempts: number;
  progress: DownloadProgress;
  error: string | null;
  createdAt: Timestamp | null;
  updatedAt: Timestamp | null;
  startedAt: Timestamp | null;
  completedAt: Timestamp | null;
}

/**
 * Lifecycle notification broadcast by the backend.
 *
 * Mirrors the Rust `DownloadEvent`. One stream carries queue, progress,
 * completion, and failure, so the UI has a single subscription.
 */
export type DownloadEvent =
  | { type: "queued"; taskId: Id }
  | { type: "stateChanged"; taskId: Id; state: DownloadState }
  | {
      type: "progress";
      taskId: Id;
      downloadedBytes: number;
      totalBytes: number | null;
      /**
       * Transfer speed over a recent window, when one could be measured.
       * `null` until there is enough history for the number to mean anything.
       */
      speedBytesPerSecond: number | null;
      /**
       * Seconds remaining at the current speed. `null` whenever the total is
       * unknown or the speed is not yet meaningful — an estimate built on
       * nothing is worse than no estimate, because it is believed.
       */
      etaSeconds: number | null;
    }
  | { type: "completed"; taskId: Id; destinationPath: string | null }
  | { type: "failed"; taskId: Id; message: string }
  | { type: "removed"; taskId: Id };

export interface DownloadSummary {
  total: number;
  queued: number;
  downloading: number;
  paused: number;
  completed: number;
  failed: number;
  cancelled: number;
  /** Bytes transferred so far across tasks that have not finished. */
  activeDownloadedBytes: number;
  /**
   * Those tasks' totals, summed over the ones whose size the source reported.
   * `null` when not one of them did — which is not the same as zero work.
   */
  activeTotalBytes: number | null;
  /** Unfinished tasks whose total the source never reported. */
  activeWithoutSize: number;
  /** Total size of the files completed downloads produced. */
  completedBytes: number;
}

export interface DownloadHistoryEntry {
  id: Id;
  taskId: string | null;
  postId: Id | null;
  postUrl: string | null;
  creatorId: Id | null;
  creatorName: string | null;
  mediaType: MediaType;
  sourceUrl: string;
  localPath: string | null;
  fileSize: number | null;
  status: DownloadState;
  error: string | null;
  downloadedAt: Timestamp | null;
}

// --- library ---------------------------------------------------------------

/**
 * Which slice of the local library is being shown.
 *
 * `downloaded` means every resource of the post is present locally — not merely
 * that a download finished — so a carousel with one of three images on disk is
 * not in it.
 */
export type LibraryView =
  | "all"
  | "videos"
  | "images"
  | "audio"
  | "downloaded"
  | "failed"
  | "new";

export type LibrarySort =
  | "newest"
  | "oldest"
  | "title_asc"
  | "creator_asc"
  | "downloaded_first";

/** Everything a library listing can be narrowed by. */
export interface LibraryFilter {
  search?: string | null;
  mediaTypes?: MediaType[];
  creatorId?: string | null;
  statuses?: ContentStatus[];
  /** ISO 8601, or a bare `YYYY-MM-DD` meaning midnight UTC. */
  publishedAfter?: string | null;
  publishedBefore?: string | null;
  minDurationSeconds?: number | null;
  minWidth?: number | null;
}

export interface LibraryQuery {
  view?: LibraryView;
  search?: string | null;
  filter?: LibraryFilter;
  sort?: LibrarySort;
  offset?: number;
  limit?: number | null;
}

/** One post, reconciled with what is known about it locally. */
export interface LibraryEntry {
  post: Post;
  creatorName: string | null;
  creatorHandle: string | null;
  /** The reconciled status: source availability folded with local state. */
  status: ContentStatus;
  downloadState: DownloadState | null;
  /**
   * What was established about the file the newest download produced:
   * `present`, `missing`, or `unavailable`.
   *
   * Three answers rather than a boolean, because the third is the point — a file
   * this application could not look at is not a file that is gone. `null` when
   * no completed download recorded a path at all, which is not one of the three.
   *
   * The same three words the Downloads screen matches on, so one condition is
   * described identically wherever it appears.
   */
  fileState: FileState | null;
  /** Why it could not be checked, when it could not. */
  fileStateReason: string | null;
  localPath: string | null;
  downloadedAt: Timestamp | null;
  /** Why the last attempt failed, or why the status is what it is. */
  error: string | null;
  /** Resources the post holds; `0` for one only ever discovered. */
  mediaItemCount: number;
  /** Those with a completed download whose file exists. */
  downloadedItemCount: number;
  durationSeconds: number | null;
  width: number | null;
  height: number | null;
}

/** How many entries each view holds, over the whole match. */
export interface LibraryCounts {
  all: number;
  videos: number;
  images: number;
  audio: number;
  downloaded: number;
  failed: number;
  new: number;
}

export interface LibraryPage {
  entries: LibraryEntry[];
  total: number;
  offset: number;
  limit: number;
  counts: LibraryCounts;
  /** Posts the query looked at before derived filters were applied. */
  scanned: number;
  /**
   * True when the read stopped at the scan limit, so `total` and the tab counts
   * are floors rather than the whole answer. The screen says so rather than
   * letting "of N" read as complete when it is not.
   */
  truncated: boolean;
}

export interface LibraryCreator {
  id: string;
  name: string;
}

export type ExportFormat = "json" | "csv";

/** Which records an export covers. The match, never just the page on screen. */
export type ExportScope = "matching";

export interface ExportResult {
  path: string;
  format: ExportFormat;
  records: number;
  bytes: number;
  scope: ExportScope;
}

/**
 * A post's text written to disk.
 *
 * Deliberately not a `DownloadTask`: nothing was downloaded, so nothing about
 * this belongs in the queue or the history.
 */
export interface SavedText {
  postId: Id;
  /** Absolute path the text was written to. */
  path: string;
  bytes: number;
  /** True when a file was already there and this one was named beside it. */
  renamed: boolean;
}
