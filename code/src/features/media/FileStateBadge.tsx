import { useEffect } from "react";
import { useLocation } from "react-router-dom";

import {
  fileStateHint,
  fileStateLabel,
  useFileStateStore,
} from "@/stores/useFileStateStore";
import type { DownloadState, Id } from "@/types";
import { Badge } from "@/components/ui";

/**
 * The only kind of task that has a file to have a view on.
 *
 * A download still running has not written its file yet. A download that failed
 * never wrote one. Neither has a file that could have gone missing, and saying
 * so about either would be this application inventing a loss the user never had
 * — so the badge is offered for completed downloads and nothing else. The scan
 * behind it takes the same line, which is why a `failed` row shows its failure
 * and nothing about a file.
 */
const COMPLETED: DownloadState[] = ["completed"];

/**
 * Read the disk whenever the screen this is used on becomes the one on screen.
 *
 * Screens are kept mounted rather than unmounted, so "when the screen opens" is
 * not a mount. Watching the route is what makes a second visit re-check — the
 * user may well have moved a file in between, which is the entire reason this
 * exists.
 */
export function useScanWhenVisible(prefix: string) {
  const { pathname } = useLocation();
  const rescan = useFileStateStore((store) => store.rescan);
  const active = pathname === prefix || pathname.startsWith(`${prefix}/`);

  useEffect(() => {
    if (active) void rescan();
  }, [active, rescan]);
}

/**
 * Whether the file a finished download produced is still there.
 *
 * Silent when everything is fine — a download whose file is present needs no
 * badge saying so, and a column of "OK" badges is noise that hides the one row
 * worth looking at. It appears only when there is something to say.
 *
 * Nothing is shown for a task that has not finished. A download still running
 * has not written its file yet, and a badge reading "missing" beside a working
 * download would be the worst kind of wrong.
 */
export function FileStateBadge({ taskId, state }: { taskId: Id; state: DownloadState }) {
  const entry = useFileStateStore((store) => store.index.get(taskId) ?? null);

  if (!COMPLETED.includes(state)) return null;

  const label = fileStateLabel(entry?.state ?? null);
  if (!label) return null;

  const hint = fileStateHint(entry?.state ?? null, entry?.reason) ?? undefined;

  return (
    <span title={hint}>
      <Badge tone={entry?.state === "missing" ? "warning" : "neutral"}>{label}</Badge>
    </span>
  );
}
