/**
 * The single seam between the UI and the Rust backend.
 *
 * Every command call goes through {@link call}, which:
 *
 * 1. invokes the Tauri command,
 * 2. converts whatever the backend rejected with into a {@link BackendError}.
 *
 * Nothing else in the frontend imports `invoke` directly, so error handling and
 * transport concerns stay in one place.
 */

import { invoke } from "@tauri-apps/api/core";

import type { AppErrorKind, SerializedError } from "@/types";

/** Codes the frontend synthesizes; the backend never sends these. */
export const IPC_ERROR_CODE = "ipc_error";

/**
 * A failed command, normalized for the UI.
 *
 * `message` is always safe to display. `details` carries developer information
 * and is only populated by debug builds of the backend, so the UI must never
 * depend on it.
 */
export class BackendError extends Error {
  readonly kind: AppErrorKind;
  readonly code: string;
  readonly details?: string;
  /** Whether the backend says trying the same thing again could work. */
  readonly retryable: boolean;

  constructor(error: SerializedError) {
    super(error.message);
    this.name = "BackendError";
    this.kind = error.kind;
    this.code = error.code;
    this.details = error.details;
    this.retryable = error.retryable ?? false;
  }

  /** True when the user can fix the problem by changing their input. */
  get isUserFixable(): boolean {
    return this.kind === "validation" || this.kind === "unsupported_content";
  }
}

function isSerializedError(value: unknown): value is SerializedError {
  if (typeof value !== "object" || value === null) return false;
  const candidate = value as Partial<SerializedError>;
  return (
    typeof candidate.code === "string" &&
    typeof candidate.message === "string" &&
    typeof candidate.kind === "string"
  );
}

/**
 * Normalize anything `invoke` can reject with.
 *
 * Commands return structured errors, but the Tauri transport itself rejects
 * with a plain string when a command is missing or panics. Those strings can
 * contain internal detail, so they are demoted to `details` and the user sees a
 * generic message.
 */
export function normalizeError(raw: unknown): BackendError {
  if (isSerializedError(raw)) return new BackendError(raw);
  if (raw instanceof BackendError) return raw;

  return new BackendError({
    kind: "unknown",
    code: IPC_ERROR_CODE,
    message: "The application backend did not respond. Please restart the application.",
    details: typeof raw === "string" ? raw : String(raw),
  });
}

/** Invoke a Tauri command, normalizing failures. */
export async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(command, args);
  } catch (raw) {
    throw normalizeError(raw);
  }
}
