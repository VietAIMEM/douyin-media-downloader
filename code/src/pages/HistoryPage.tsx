import { useCallback, useEffect, useState } from "react";

import {
  Badge,
  Button,
  Card,
  EmptyState,
  ErrorNotice,
  PageHeader,
} from "@/components/ui";
import { Icon as IconObject } from "@/components/ui/icons";
import { FileStateBadge, useScanWhenVisible } from "@/features/media/FileStateBadge";
import { useDownloadStore } from "@/stores/useDownloadStore";
import { useFileStateStore } from "@/stores/useFileStateStore";
import { errorKindLabel } from "@/lib/errors";
import { formatBytes, formatDateTime, humanize } from "@/lib/format";
import { api } from "@/services/api";
import { normalizeError, type BackendError } from "@/services/ipc";
import type { DownloadHistoryEntry } from "@/types";
import { mediaTypeLabel } from "@/types";

/** Newest first, and capped so the first paint stays small. */
const PAGE_SIZE = 200;

/**
 * History.
 *
 * A record of outcomes, not a queue: everything here has already happened. The
 * row leads with what the download produced — the file, or the reason there is
 * none — and keeps the source it came from secondary, because the user is
 * looking for their media and not for a URL.
 */
export function HistoryPage() {
  const [entries, setEntries] = useState<DownloadHistoryEntry[]>([]);
  const [count, setCount] = useState<number | null>(null);
  const [error, setError] = useState<BackendError | null>(null);
  const [loading, setLoading] = useState(true);
  const rescan = useFileStateStore((state) => state.rescan);
  const scanning = useFileStateStore((state) => state.scanning);
  // Asked when this screen is the one on screen, never on a render.
  useScanWhenVisible("/history");

  const load = useCallback(async () => {
    setError(null);
    try {
      const [nextEntries, nextCount] = await Promise.all([
        api.listHistory(PAGE_SIZE),
        api.getHistoryCount(),
      ]);
      setEntries(nextEntries);
      setCount(nextCount);
    } catch (raw) {
      setError(normalizeError(raw));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // A finished download writes a history row, so the list follows along without
  // the user pressing Refresh. The signal is the store's state counter, which
  // moves when a task changes state and not when its bytes do — a history entry
  // is about an outcome, and reloading on every progress frame would be a
  // request per frame to learn nothing.
  const stateRevision = useDownloadStore((state) => state.stateRevision);
  useEffect(() => {
    if (stateRevision === 0) return;
    void load();
  }, [stateRevision, load]);

  const openPath = useCallback(async (entry: DownloadHistoryEntry, kind: "file" | "folder") => {
    if (!entry.taskId) return;
    setError(null);
    try {
      await api.openDownloadPath(entry.taskId, kind);
    } catch (raw) {
      setError(normalizeError(raw));
    }
  }, []);

  const succeededCount = entries.filter((entry) => entry.status === "completed").length;

  return (
    <>
      <PageHeader
        title="History"
        description="A record of every download that finished — including the ones that failed or were cancelled."
      >
        <Button size="sm" loading={loading} onClick={() => void load()}>
          Refresh
        </Button>
        {/* History is a record of what happened; this asks what is on the disk
            now. Kept separate because they are separate facts — a download that
            finished is still finished after the file is deleted. */}
        <Button size="sm" onClick={() => void rescan()} loading={scanning}>
          Check files
        </Button>
      </PageHeader>

      <div className="space-y-5">
        <ErrorNotice
          error={error}
          onDismiss={() => setError(null)}
          title={error ? errorKindLabel(error) : undefined}
        />

        <Card
          title="Recorded downloads"
          description={
            error && entries.length === 0
              ? "Nothing is shown until the record can be loaded"
              : count === null
                ? "Reading history…"
                : count === 0
                  ? "No downloads recorded yet"
                  : `${succeededCount} succeeded · ${count - succeededCount} failed or cancelled · showing the ${entries.length} most recent`
          }
          flush
        >
          {entries.length === 0 && !error ? (
            <div className="p-5">
              <EmptyState
                title="Nothing has been downloaded yet"
                description="Completed downloads are recorded here with the file they produced, so you can find them again later."
              />
            </div>
          ) : entries.length === 0 && error ? (
            // A read that failed has said nothing about what is recorded.
            // "Nothing has been downloaded" here would be a claim about the
            // user's history made by a screen that could not read it. The
            // notice above carries the reason, so this states only the
            // consequence and the way out.
            <p className="px-5 py-6 text-center text-xs text-content-muted">
              Nothing is shown until the record can be loaded. Try Refresh.
            </p>
          ) : (
            <ul className="divide-y divide-surface-border">
              {entries.map((entry) => (
                <HistoryRow key={entry.id} entry={entry} onOpen={openPath} />
              ))}
            </ul>
          )}
        </Card>
      </div>
    </>
  );
}

interface HistoryRowProps {
  entry: DownloadHistoryEntry;
  onOpen: (entry: DownloadHistoryEntry, kind: "file" | "folder") => void;
}

function HistoryRow({ entry, onOpen }: HistoryRowProps) {
  const succeeded = entry.status === "completed";
  const name =
    entry.localPath?.split(/[\\/]/).filter(Boolean).pop() ??
    entry.postUrl ??
    entry.sourceUrl;
  // The URL is the row's headline when there is no file to name, so it is not
  // printed a second time beside it — one address, said once.
  const sourceUrl = entry.postUrl && entry.postUrl !== name ? entry.postUrl : null;
  const context = [entry.creatorName ? `@${entry.creatorName}` : null, sourceUrl]
    .filter(Boolean)
    .join(" · ");

  return (
    <li className="space-y-1.5 px-5 py-3">
      <div className="flex flex-wrap items-center gap-3">
        <span
          aria-hidden="true"
          className={`flex h-7 w-7 shrink-0 items-center justify-center rounded-lg ${
            succeeded ? "bg-success-subtle text-success" : "bg-danger-subtle text-danger"
          }`}
        >
          <IconObject.check className="h-4 w-4" />
        </span>
        <Badge tone={succeeded ? "success" : "danger"}>
          {humanize(entry.status)}
        </Badge>
        {entry.taskId && <FileStateBadge taskId={entry.taskId} state={entry.status} />}
        <span className="min-w-0 flex-1 truncate text-xs font-medium text-content-primary" title={name}>
          {name}
        </span>
        <span className="text-xs text-content-muted">{mediaTypeLabel(entry.mediaType)}</span>
        <span className="text-xs text-content-muted">{formatBytes(entry.fileSize)}</span>
        <span className="text-xs text-content-muted">{formatDateTime(entry.downloadedAt)}</span>
      </div>

      <div className="flex flex-wrap items-center gap-3 pl-10">
        {context && <span className="text-[11px] text-content-muted">{context}</span>}
        {/* Opening needs the originating task; older rows without one simply
            do not offer the action rather than failing when pressed. */}
        {entry.taskId && succeeded && (
          <span className="flex gap-2">
            <Button size="sm" onClick={() => onOpen(entry, "file")}>
              Open file
            </Button>
            <Button size="sm" onClick={() => onOpen(entry, "folder")}>
              Open folder
            </Button>
          </span>
        )}
      </div>

      {entry.error && (
        <p className="break-all pl-10 font-mono text-[11px] text-danger">{entry.error}</p>
      )}
    </li>
  );
}
