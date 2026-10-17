/**
 * Turn a backend error into a short label plus guidance.
 *
 * The backend already sends a user-safe message and a stable `kind`; this only
 * adds the framing the screens need. It deliberately does not try to reinterpret
 * the error — deciding what a failure *means* is the backend's job.
 */

import type { AppErrorKind } from "@/types";

import type { BackendError } from "@/services/ipc";

/** Short label for the error category, used in badges and empty states. */
const LABELS: Record<AppErrorKind, string> = {
  validation: "Invalid link",
  network: "Network problem",
  resolution: "Could not resolve",
  download: "Download failed",
  storage: "Storage problem",
  database: "Database problem",
  unsupported_content: "Unsupported",
  unavailable_content: "Unavailable",
  browser_unavailable: "Browser needed",
  authentication_required: "Sign-in required",
  muxer_unavailable: "FFmpeg needed",
  not_found: "Not found",
  unknown: "Error",
};

export function errorKindLabel(error: BackendError): string {
  return LABELS[error.kind] ?? "Error";
}

/**
 * Whether retrying the same input could plausibly succeed.
 *
 * Only network and download failures are worth retrying from the screen: a
 * validation or unsupported-content error will fail identically every time.
 */
export function isRetryable(error: BackendError): boolean {
  return error.kind === "network" || error.kind === "download";
}

/** A short, plain-language hint for how the user might recover. */
export function recoveryHint(error: BackendError): string | null {
  switch (error.kind) {
    case "validation":
      return "Check the link and paste it again.";
    case "unsupported_content":
      return "Only public Douyin links are supported.";
    case "unavailable_content":
      return "The post may be private, removed, or not available in your region.";
    case "browser_unavailable":
      return "Reading this page needs a browser. Install Chrome or Edge, or turn browser-assisted resolution on in Settings.";
    case "authentication_required":
      return "Use Login on this screen to sign in, enable \"Use login session when loading profiles\", and load the profile again.";
    case "muxer_unavailable":
      return "This video's sound is served as a separate track, and combining the two needs FFmpeg. Install it, or point the application at an ffmpeg executable with DOUYIN_DOWNLOADER_FFMPEG.";
    case "network":
      return "Check your connection and try again.";
    case "resolution":
      return "The source returned something this app could not read.";
    case "storage":
      return "Check the download folder and available disk space.";
    case "download":
      return "You can retry the download.";
    default:
      return null;
  }
}
