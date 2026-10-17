/**
 * Types for the backend-to-frontend wire format: settings, diagnostics,
 * errors, and the URL inspection payloads.
 */

import type {
  MediaType,
  Platform,
  Post,
  Creator,
  ContentStatus,
  DownloadState,
  DownloadSummary,
  DownloadTask,
  FileState,
  Id,
  MediaItem,
  Theme,
  Timestamp,
  ConflictPolicy,
  LogLevel,
} from "./domain";

/** Coarse error classification. Mirrors `AppErrorKind` (snake_case values). */
export type AppErrorKind =
  | "validation"
  | "network"
  | "resolution"
  | "download"
  | "storage"
  | "database"
  | "unsupported_content"
  | "unavailable_content"
  | "browser_unavailable"
  | "authentication_required"
  | "muxer_unavailable"
  | "not_found"
  | "unknown";

/**
 * Structured error returned by every command.
 *
 * `message` is always safe to show to a user. `details` is developer-only and
 * is populated in debug builds only.
 */
/** What the browser's own profile is holding. */
export type LoginStatus = "signedIn" | "signedOut" | "unknown";

/** The state of the saved sign-in, as the sign-in section needs it. */
export interface LoginReport {
  /** False when this build has no browser to sign in with at all. */
  available: boolean;
  /**
   * What the profile held when it was asked, or `null` when it could not be
   * asked. The third answer is not a failure dressed up as one: "the browser
   * could not be started" is different from "the profile has no session".
   */
  status: LoginStatus | null;
  /** The application's own profile directory, shown so it is not a mystery. */
  profileDirectory: string;
}

export interface SerializedError {
  kind: AppErrorKind;
  /** Stable machine-readable code, e.g. `validation_error`. */
  code: string;
  message: string;
  details?: string;
  /**
   * Whether trying the same thing again could reasonably work.
   *
   * The backend already decides this for its own retries; a screen that pages
   * through a listing needs the same answer, so that a transient refusal is
   * retried and a definitive one is not.
   */
  retryable?: boolean;
}

export interface AppSettings {
  downloadDirectory: string;
  maxConcurrentDownloads: number;
  filenameTemplate: string;
  folderTemplate: string;
  organizeByCreator: boolean;
  organizeByMediaType: boolean;
  /**
   * Whether resolution may open a page in an installed browser when the plain
   * HTTP path cannot obtain the content.
   */
  browserAssistedResolution: boolean;
  /**
   * Load profiles through the application's own signed-in browser profile.
   *
   * Says only that the user asked for it. Whether a sign-in exists is a
   * question for the browser, asked every time it matters.
   */
  profileLoginEnabled: boolean;
  conflictPolicy: ConflictPolicy;
  theme: Theme;
  logLevel: LogLevel;
  logToFile: boolean;
  retryMaxAttempts: number;
  requestTimeoutSeconds: number;
  language: string;
}

export interface ResolverInfo {
  name: string;
  platform: Platform;
  capability: string;
}

export interface AppInfo {
  name: string;
  version: string;
  identifier: string;
  os: string;
  architecture: string;
  dataDirectory: string;
  databasePath: string;
  logDirectory: string;
  schemaVersion: number;
  logLevel: string;
  demoMode: boolean;
  /**
   * Whether this build names somewhere to look for updates. A build without a
   * channel has nothing to check against, so the Settings screen says so
   * instead of offering a check that could only fail.
   */
  updateChannelConfigured: boolean;
  supportedPlatforms: string[];
  resolvers: ResolverInfo[];
  startedAt: string;
}

export interface AppliedMigration {
  version: number;
  description: string;
  appliedAt: string;
}

export interface TableCount {
  table: string;
  rows: number;
}

export interface DatabaseStatus {
  healthy: boolean;
  location: string;
  isPersistent: boolean;
  schemaVersion: number;
  latestSchemaVersion: number;
  appliedMigrations: AppliedMigration[];
  tableCounts: TableCount[];
  journalMode: string;
  foreignKeys: boolean;
}

export interface EnumOption {
  value: string;
  label: string;
}

export interface SettingsOptions {
  conflictPolicies: EnumOption[];
  themes: EnumOption[];
  logLevels: EnumOption[];
  filenamePlaceholders: string[];
  maxConcurrentLimit: number;
  maxRetryAttempts: number;
  timeoutSecondsMin: number;
  timeoutSecondsMax: number;
  defaultDownloadDirectory: string;
}

/** What a pasted link means to the application. */
export interface UrlClassification {
  original: string;
  canonical: string;
  platform: Platform;
  /** `profile`, `post`, or `short_link`. */
  kind: "profile" | "post" | "short_link";
  label: string;
  id: string | null;
  isShortLink: boolean;
  requiresExpansion: boolean;
  resolver: string | null;
}

export interface UrlPreview {
  classification: UrlClassification;
  resolvedAs: "post" | "profile";
  post: Post | null;
  creator: Creator | null;
  /** How far resolution got in this build, e.g. `url_only`. */
  resolutionStage: string;
}

export interface TestFixture {
  label: string;
  url: string;
  /** Which screen the link belongs on: `profile` or `post`. */
  intent: string;
  expectedKind: string;
}

/**
 * The result of analyzing a pasted link.
 *
 * `intent` is what the UI branches on: a profile link is a *successful*
 * analysis with `intent: "profile"`, not an error, because the link was
 * understood correctly — it simply is not a downloadable post.
 */
export interface AnalyzedUrl {
  classification: UrlClassification;
  intent: "post" | "profile";
  post: Post | null;
  creator: Creator | null;
  media: MediaItem[];
  /** `resolved` once a post and its media are known. */
  resolutionStage: "resolved" | "classified_only";
  /**
   * How a post was resolved: `http` or `browser`.
   *
   * `browser` means a browser process was started to read the page. Starting one
   * is a bigger action than a request, so the UI says so rather than leaving the
   * user to guess.
   */
  resolvedVia: "http" | "browser" | null;
  /** Plain-language explanation when the link is understood but not downloadable. */
  notice: string | null;
}

/**
 * A request to download one media variant.
 *
 * Carries ids only. The backend reads the media URL from its own database, so
 * the frontend cannot choose what gets fetched.
 */
export interface CreateDownloadInput {
  postId: Id;
  mediaItemId: Id;
  variantId?: Id | null;
}

/** Helper for rendering a media type in the UI. */
export const mediaTypeLabel = (mediaType: MediaType): string => {
  switch (mediaType) {
    case "video":
      return "Video";
    case "image":
      return "Image";
    case "carousel":
      return "Carousel";
    case "audio":
      return "Audio";
    case "text":
      return "Text";
  }
};

// --- profile workflow -------------------------------------------------------

/**
 * The creator behind a profile link, and how much of it was actually read.
 *
 * `resolutionStage` is `resolved` when the profile page supplied the creator's
 * own details and `url_only` when only the link's identity could be
 * established — in which case the display name is the platform id, not an
 * invented one, and `notice` explains it.
 */
export interface ProfileIdentity {
  creator: Creator;
  classification: UrlClassification;
  resolutionStage: "resolved" | "url_only";
  resolvedVia: "http" | "browser" | null;
  notice: string | null;
}

/**
 * One post in a profile listing.
 *
 * `status` folds the source's availability together with local download state;
 * `downloadState` and `activeTaskId` are the local facts behind it. A post is
 * `new` until something is known locally — a listing never claims a post has
 * been downloaded.
 */
export interface ProfilePostView {
  post: Post;
  status: ContentStatus;
  downloadState: DownloadState | null;
  activeTaskId: Id | null;
  /** Why the status reads the way it does, when the reason is not obvious. */
  note: string | null;
}

/**
 * One page of a creator's posts.
 *
 * `nextCursor` is opaque: the platform's paging scheme never reaches here.
 * `totalKnown` is the creator's own post count when the source published one,
 * and `loadedTotal` is how many of their posts are stored locally.
 */
export interface ProfilePostsPage {
  creator: Creator;
  posts: ProfilePostView[];
  nextCursor: string | null;
  hasMore: boolean;
  totalKnown: number | null;
  loadedTotal: number;
}

/** What happened to one selected post. */
export interface ProfileDownloadOutcome {
  postId: Id;
  platformPostId: string;
  title: string;
  outcome:
    | "taskCreated"
    | "alreadyDownloaded"
    | "alreadyDownloading"
    | "unsupported"
    | "failed";
  message: string | null;
  taskId: Id | null;
}

/**
 * The result of submitting a selection.
 *
 * Every media *item* lands in exactly one of `tasksCreated` / `skipped` /
 * `failed` / `unsupported`, so the summary accounts for every download the batch
 * set out to make. The counts therefore count items — a carousel queues fifteen
 * and the summary has to say fifteen — while `selected` and `resolved` count
 * posts, which is what the user actually chose. Each post's own entry nests its
 * items, so "did my carousel download?" and "which picture failed?" are both
 * answerable from one result.
 */
export interface ProfileDownloadResult {
  /** Posts the user asked for. */
  selected: number;
  /** Posts whose media was obtained. */
  resolved: number;
  /**
   * Media items queued.
   *
   * Items, not posts: a carousel queues one per picture, and a summary that
   * counted the carousel as one would say "1 of 1" for fifteen downloads.
   */
  tasksCreated: number;
  skipped: number;
  failed: number;
  /** Media items that resolved to nothing downloadable. */
  unsupported: number;
  posts: ProfilePostResult[];
}

/** What happened to one selected post, and to each of its media items. */
export interface ProfilePostResult {
  postId: Id;
  platformPostId: string;
  title: string;
  /** `completed`, `queued`, `partial`, `failed` or `unsupported`. */
  outcome: string;
  media: ProfileDownloadOutcome[];
}

/** A request to submit stored posts. Carries ids only, never URLs. */
export interface SubmitProfileDownloadsInput {
  postIds: Id[];
  /**
   * The slides the user chose, keyed by post.
   *
   * Only the posts they actually narrowed appear here. A post left out keeps the
   * screen's default — every picture it has — so a caller with no per-slide
   * choice to make does not have to speak for it.
   */
  selectedItems?: Record<Id, Id[]>;
}

// --- download queue ---------------------------------------------------------

/**
 * The whole queue in one answer.
 *
 * One call rather than two, because two can straddle a task finishing and
 * report a list and a summary that disagree with each other.
 */
export interface DownloadQueue {
  tasks: DownloadTask[];
  summary: DownloadSummary;
}

// --- what is on disk --------------------------------------------------------
//
// `FileState` itself lives in `./domain`, beside the other state vocabularies:
// the Library describes files with the same three words, and a union declared
// here would have had to be imported back the other way.

/** What the filesystem said about one finished download. */
export interface FileScanEntry {
  taskId: Id;
  state: FileState;
  /** Why nothing could be established, when nothing could. */
  reason?: string;
  sizeBytes?: number;
  modified?: Timestamp;
}

/** One pass over every finished download, and where it looked. */
export interface FileScan {
  /** The download folder as it is configured now. */
  root: string;
  scannedAt: Timestamp;
  entries: FileScanEntry[];
}

/** What kind of media a file is, judged by the source's own extension. */
export type MediaKind = "image" | "video" | "audio" | "other";

/** One folder inside the download folder. */
export interface MediaFolder {
  name: string;
  path: string;
  mediaCount: number;
  imageCount: number;
  videoCount: number;
  audioCount: number;
  totalBytes: number;
  previewPath?: string;
  /** What `previewPath` is: a picture if the folder holds one, otherwise a video. */
  previewKind?: MediaKind;
  state: "ok" | "empty" | "unreadable";
}

/** One media file on disk, with whatever the application knows about it. */
export interface MediaFile {
  name: string;
  path: string;
  kind: MediaKind;
  extension: string;
  sizeBytes: number;
  modified?: Timestamp;
  /** Whether this window can show it itself, or should offer to open it. */
  playable: boolean;
  /**
   * What the download recorded it as — `image`, `video`, `carousel`, `audio`,
   * `text`. Absent when nothing recorded this file, which is the ordinary case
   * for a file put there by hand.
   */
  mediaType?: string;
  taskId?: Id;
  postId?: Id;
  mediaItemId?: Id;
  caption?: string;
  creator?: string;
  sourceUrl?: string;
  downloadedAt?: Timestamp;
}

/** The top level: the folders inside the download folder. */
export interface FolderListing {
  root: string;
  folders: MediaFolder[];
  /** Media sitting directly in the root, which no folder accounts for. */
  looseMediaCount: number;
  problem?: string;
}

/** Inside one folder. */
export interface FolderContents {
  path: string;
  parent?: string;
  folders: MediaFolder[];
  files: MediaFile[];
  /** True when the folder holds more media than one listing returns. */
  truncated: boolean;
}
