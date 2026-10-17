import { formatBytes } from "@/lib/format";

interface ProgressBarProps {
  downloadedBytes: number;
  /** `null` when the source did not report a content length. */
  totalBytes: number | null;
  /** Drives the bar's colour and whether it animates. */
  tone?: "active" | "success" | "danger" | "muted";
  /**
   * What this bar is measuring, for a screen reader.
   *
   * The visible row already says which download it belongs to, but a bar read
   * out of context is just "40 percent" — the label is what makes the number
   * mean something.
   */
  label?: string;
}

const TONES = {
  active: "bg-brand",
  success: "bg-success",
  danger: "bg-danger",
  muted: "bg-content-muted",
} as const;

/**
 * Byte-accurate progress bar.
 *
 * When the total is unknown the bar cannot show a percentage, so it says so
 * rather than showing an invented one: an indeterminate stripe plus the byte
 * count is honest, a fake 50% is not. That distinction is carried into the
 * accessibility tree too — an unknown total omits `aria-valuenow` rather than
 * reporting a zero, because a screen reader announcing "0 percent" for a
 * download that is progressing normally is worse than saying nothing.
 */
export function ProgressBar({
  downloadedBytes,
  totalBytes,
  tone = "active",
  label,
}: ProgressBarProps) {
  const known = totalBytes !== null && totalBytes > 0;
  const percent = known
    ? Math.min(100, Math.round((downloadedBytes / (totalBytes as number)) * 100))
    : null;

  const readable = known
    ? `${formatBytes(downloadedBytes)} of ${formatBytes(totalBytes)}`
    : `${formatBytes(downloadedBytes)}, total size unknown`;

  return (
    <div className="space-y-1">
      <div
        role="progressbar"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={known ? 100 : undefined}
        aria-valuenow={percent ?? undefined}
        aria-valuetext={readable}
        className="h-1.5 w-full overflow-hidden rounded-full bg-surface-overlay"
      >
        {percent === null ? (
          <div
            className={`h-full w-1/3 rounded-full ${TONES[tone]} motion-safe:animate-pulse`}
          />
        ) : (
          <div
            className={`h-full rounded-full transition-[width] duration-200 ${TONES[tone]}`}
            style={{ width: `${percent}%` }}
          />
        )}
      </div>
      {/*
        The same numbers, as text. A progress bar alone communicates nothing to
        someone who cannot see it, and colour alone communicates nothing to
        someone who cannot distinguish it — so the state is always written out.
      */}
      <div className="flex items-center justify-between text-[11px] text-content-muted">
        <span>
          {formatBytes(downloadedBytes)}
          {known ? ` / ${formatBytes(totalBytes)}` : ""}
        </span>
        <span>{percent === null ? "size unknown" : `${percent}%`}</span>
      </div>
    </div>
  );
}
