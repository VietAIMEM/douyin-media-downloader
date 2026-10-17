/**
 * Subscription to the backend's download lifecycle stream.
 *
 * The backend broadcasts queue, progress, completion, and failure events on one
 * channel, which `lib.rs` forwards to the webview under {@link DOWNLOAD_EVENT_NAME}.
 * Subscribing here keeps the UI event-driven: progress appears as it happens
 * rather than at a polling interval the frontend invented.
 *
 * Progress frames are coalesced by the transfer worker before they cross IPC, so
 * a listener receives a bounded number of updates even on a fast connection.
 */

import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type { DownloadEvent } from "@/types";

/** Must match `DOWNLOAD_EVENT_NAME` in `src-tauri/src/lib.rs`. */
export const DOWNLOAD_EVENT_NAME = "download://event";

/**
 * Listen for download events.
 *
 * Returns the unlisten function. The payload is forwarded verbatim; the caller
 * decides how to fold it into state, so this module stays free of UI concerns.
 */
export async function onDownloadEvent(
  handler: (event: DownloadEvent) => void,
): Promise<UnlistenFn> {
  return listen<DownloadEvent>(DOWNLOAD_EVENT_NAME, (message) => handler(message.payload));
}
