/**
 * Presentation helpers.
 *
 * Kept free of domain types so they can be used for any value the backend
 * sends; formatting is a view concern and must not leak into stores.
 */

const UNITS = ["B", "KB", "MB", "GB", "TB"] as const;

/** Human-readable byte count, or an em dash when the size is unknown. */
export function formatBytes(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined || !Number.isFinite(bytes) || bytes < 0) {
    return "—";
  }
  if (bytes < 1024) return `${bytes} B`;

  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(value >= 100 ? 0 : 1)} ${UNITS[unit]}`;
}

/**
 * Format an ISO timestamp in the user's locale.
 *
 * Values that fail to parse are shown verbatim rather than replaced with
 * `Invalid Date`, so a malformed backend timestamp is visible in the UI instead
 * of hidden by the formatter.
 */
export function formatDateTime(iso: string | null | undefined): string {
  if (!iso) return "—";
  const parsed = new Date(iso);
  if (Number.isNaN(parsed.getTime())) return iso;
  return parsed.toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** Turn a snake_case backend token into a readable label. */
export function humanize(token: string): string {
  const spaced = token.replace(/_/g, " ").trim();
  return spaced.charAt(0).toUpperCase() + spaced.slice(1);
}
