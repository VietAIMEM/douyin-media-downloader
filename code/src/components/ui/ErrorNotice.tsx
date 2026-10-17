import type { BackendError } from "@/services/ipc";
import { Icon } from "./icons";
import { Badge, type BadgeTone } from "./Badge";
import { FOCUS_RING } from "@/lib/a11y";

interface ErrorNoticeProps {
  error: BackendError | null;
  onDismiss?: () => void;
  /** Short category label, e.g. "Invalid link". Defaults to the error code. */
  title?: string;
  /** One line on how the user might recover. */
  hint?: string;
}

const TONES: Record<string, BadgeTone> = {
  validation: "warning",
  unsupported_content: "warning",
  unavailable_content: "warning",
};

/**
 * Render a command failure.
 *
 * Shows the backend's user-safe `message` and its stable `code`. `details` is
 * only ever populated by debug builds of the backend, and is labelled as such
 * so it is never mistaken for something a user should act on.
 */
export function ErrorNotice({ error, onDismiss, title, hint }: ErrorNoticeProps) {
  if (!error) return null;

  return (
    <div
      role="alert"
      className="flex items-start gap-3 rounded-xl border border-danger/30 bg-danger/5 px-4 py-3"
    >
      <Icon.warning className="mt-0.5 h-4 w-4 shrink-0 text-danger" />
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-2">
          <Badge tone={TONES[error.kind] ?? "danger"} mono>
            {title ?? error.code}
          </Badge>
          <span className="text-sm text-content-primary">{error.message}</span>
        </div>
        {hint && <p className="mt-2 text-xs leading-relaxed text-content-secondary">{hint}</p>}
        {error.details && (
          <p className="mt-2 break-all font-mono text-[11px] leading-relaxed text-content-muted">
            <span className="uppercase tracking-wide">debug detail</span> · {error.details}
          </p>
        )}
      </div>
      {onDismiss && (
        <button
          type="button"
          onClick={onDismiss}
          aria-label="Dismiss"
          className={`shrink-0 rounded-md p-1 text-content-muted transition-colors hover:bg-surface-overlay hover:text-content-primary ${FOCUS_RING}`}
        >
          <Icon.close className="h-3.5 w-3.5" />
        </button>
      )}
    </div>
  );
}
