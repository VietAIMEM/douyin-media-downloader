/**
 * Typed wrappers around every Tauri command.
 *
 * Command names appear exactly once in the frontend (here), so a renamed Rust
 * command breaks in one obvious place instead of across components.
 */

import { call } from "./ipc";

import type {
  AnalyzedUrl,
  AppInfo,
  AppSettings,
  CreateDownloadInput,
  DatabaseStatus,
  DownloadHistoryEntry,
  DownloadQueue,
  DownloadSummary,
  DownloadTask,
  ExportFormat,
  ExportResult,
  ExportScope,
  LibraryCreator,
  LibraryPage,
  LibraryQuery,
  ProfileDownloadResult,
  ProfileIdentity,
  LoginReport,
  ProfilePostsPage,
  FolderContents,
  FolderListing,
  FileScan,
  SavedText,
  SettingsOptions,
  SubmitProfileDownloadsInput,
  TestFixture,
  UrlClassification,
  UrlPreview,
} from "@/types";

export const api = {
  // --- application ---------------------------------------------------------
  getAppInfo: () => call<AppInfo>("get_app_info"),
  getDatabaseStatus: () => call<DatabaseStatus>("get_database_status"),

  // --- settings ------------------------------------------------------------
  getSettings: () => call<AppSettings>("get_settings"),
  saveSettings: (settings: AppSettings) => call<AppSettings>("save_settings", { settings }),
  resetSettings: () => call<AppSettings>("reset_settings"),
  getSettingsOptions: () => call<SettingsOptions>("get_settings_options"),

  // --- URL inspection ------------------------------------------------------
  classifyUrl: (url: string) => call<UrlClassification>("classify_url", { url }),
  expandUrl: (url: string) => call<UrlClassification>("expand_url", { url }),
  resolveUrlMetadata: (url: string) => call<UrlPreview>("resolve_url_metadata", { url }),
  getTestFixtures: () => call<TestFixture[]>("get_test_fixtures"),

  // --- single URL workflow -------------------------------------------------
  analyzeUrl: (url: string) => call<AnalyzedUrl>("analyze_url", { url }),
  createDownloadTask: (input: CreateDownloadInput) =>
    call<DownloadTask>("create_download_task", { input }),
  cancelDownload: (taskId: string) => call<DownloadTask>("cancel_download", { taskId }),
  retryDownload: (taskId: string) => call<DownloadTask>("retry_download", { taskId }),
  openDownloadPath: (taskId: string, kind: "file" | "folder") =>
    call<void>("open_download_path", { taskId, kind }),

  // --- profile workflow ----------------------------------------------------
  resolveProfile: (url: string) => call<ProfileIdentity>("resolve_profile", { url }),
  listProfilePosts: (url: string, cursor: string | null, limit?: number) =>
    call<ProfilePostsPage>("list_profile_posts", { url, cursor, limit }),
  submitProfileDownloads: (input: SubmitProfileDownloadsInput) =>
    call<ProfileDownloadResult>("submit_profile_downloads", { input }),

  // --- the saved Douyin sign-in -------------------------------------------
  beginLogin: () => call<LoginReport>("begin_login"),
  getLoginStatus: () => call<LoginReport>("get_login_status"),
  clearLoginSession: () => call<LoginReport>("clear_login_session"),

  // --- downloads and history ----------------------------------------------
  listDownloads: () => call<DownloadTask[]>("list_downloads"),
  getDownloadQueue: () => call<DownloadQueue>("get_download_queue"),
  getDownloadSummary: () => call<DownloadSummary>("get_download_summary"),
  getDownload: (taskId: string) => call<DownloadTask | null>("get_download", { taskId }),
  pauseDownload: (taskId: string) => call<DownloadTask>("pause_download", { taskId }),
  resumeDownload: (taskId: string) => call<DownloadTask>("resume_download", { taskId }),
  removeDownload: (taskId: string) => call<boolean>("remove_download", { taskId }),
  clearCompletedDownloads: () => call<number>("clear_completed_downloads"),
  retryFailedDownloads: () => call<DownloadTask[]>("retry_failed_downloads"),
  listHistory: (limit?: number) => call<DownloadHistoryEntry[]>("list_history", { limit }),
  getHistoryCount: () => call<number>("get_history_count"),

  // --- library -------------------------------------------------------------
  queryLibrary: (query: LibraryQuery) => call<LibraryPage>("query_library", { query }),
  listLibraryCreators: () => call<LibraryCreator[]>("list_library_creators"),
  exportLibrary: (query: LibraryQuery, format: ExportFormat, scope: ExportScope = "matching") =>
    call<ExportResult>("export_library", { query, format, scope }),
  revealExport: (path: string) => call<void>("reveal_export", { path }),

  /**
   * Save a post's text as a UTF-8 `.txt` file.
   *
   * Not a download: there is no task, no queue entry and no history row. A text
   * post has nothing to transfer, and its words are written from data already
   * held.
   */
  savePostText: (postId: string) => call<SavedText>("save_post_text", { postId }),

  // --- what is on disk ------------------------------------------------------
  /**
   * Ask the filesystem about every finished download.
   *
   * Read-only: it opens nothing for writing and removes nothing. Called when a
   * screen is opened and when the user asks for a rescan, never on a render.
   */
  scanDownloadedFiles: () => call<FileScan>("scan_downloaded_files"),

  /** The folders inside the download folder. */
  browseMediaFolders: () => call<FolderListing>("browse_media_folders"),

  /** What is inside one folder: the folders below it, and the media in it. */
  browseMediaFolder: (path: string) => call<FolderContents>("browse_media_folder", { path }),

  /**
   * Open one file with whatever the desktop uses for it.
   *
   * The backend checks the path against the download folder before opening
   * anything, so this cannot be used to launch a file from elsewhere.
   */
  openMediaFile: (path: string) => call<void>("open_media_file", { path }),

  /**
   * A small preview picture for one file in the download folder.
   *
   * Answers `null` when there is no preview to be had — no FFmpeg on this
   * machine, a format that will not decode, audio, anything that is not a
   * picture. That is an ordinary answer, not a failure, and the card draws a
   * placeholder for it.
   */
  mediaThumbnail: (path: string) => call<string | null>("media_thumbnail", { path }),
};

export type Api = typeof api;
