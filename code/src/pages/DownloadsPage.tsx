import { useCallback, useMemo, useRef, useState } from "react";

import {
  Badge,
  Button,
  Card,
  ConfirmButton,
  EmptyState,
  ErrorNotice,
  PageHeader,
  ProgressBar,
  Select,
  type BadgeTone,
} from "@/components/ui";

import { FileStateBadge, useScanWhenVisible } from "@/features/media/FileStateBadge";
import { errorKindLabel } from "@/lib/errors";
import { formatBytes, formatDateTime, humanize } from "@/lib/format";
import { api } from "@/services/api";
import { normalizeError, type BackendError } from "@/services/ipc";
import { useDownloadStore, useDownloadTasks } from "@/stores/useDownloadStore";
import { useFileStateStore } from "@/stores/useFileStateStore";
import type { DownloadState, DownloadTask } from "@/types";

const STATE_TONES: Record<DownloadState, BadgeTone> = {
  queued: "neutral",
  downloading: "brand",
  paused: "warning",
  completed: "success",
  failed: "danger",
  cancelled: "neutral",
};

const ACTIVE_STATES: DownloadState[] = ["queued", "downloading", "paused"];

/** The rows the screen can be narrowed to. `all` is the default. */
const FILTERS = [
  { value: "all", label: "All states" },
  { value: "downloading", label: "Downloading" },
  { value: "queued", label: "Queued" },
  { value: "paused", label: "Paused" },
  { value: "completed", label: "Completed" },
  { value: "failed", label: "Failed" },
  { value: "cancelled", label: "Cancelled" },
];

/** A live measurement, which is not persisted and so is not on the task. */
interface LiveRate {
  speedBytesPerSecond: number | null;
  etaSeconds: number | null;
}

export function DownloadsPage() {
  // The queue comes from the shared store, so this screen and the Single URL
  // and Profile screens are showing the same rows rather than each holding a
  // copy that can drift. The store is fed by the backend's event stream, which
  // the application root subscribes to once.
  const tasks = useDownloadTasks();
  const summary = useDownloadStore((state) => state.summary);
  const rates = useDownloadStore((state) => state.rates);
  const hydrated = useDownloadStore((state) => state.hydrated);
  const streamError = useDownloadStore((state) => state.error);
  const refresh = useCallback(() => useDownloadStore.getState().reconcile(), []);
  const rescan = useFileStateStore((state) => state.rescan);
  const scanning = useFileStateStore((state) => state.scanning);
  // Asked when this screen is the one on screen, and when the user asks. Never
  // on a render: every scan touches the disk once per recorded file.
  useScanWhenVisible("/downloads");
  /** A failed action, or a failed refresh: both are the user's business. */
  const [error, setError] = useState<BackendError | null>(null);
  const shownError = error ?? streamError;
  const [filter, setFilter] = useState<string>("all");
  /** Rows with a command in flight, so a second click cannot be sent. */
  const [busyRows, setBusyRows] = useState<string[]>([]);
  /**
   * The same set, readable synchronously.
   *
   * Two clicks land in the same tick, before React has re-rendered and handed
   * the handler the updated state — so state alone cannot refuse the second
   * one. The ref can.
   */
  const rowInFlight = useRef<Set<string>>(new Set());
  const [busyBulk, setBusyBulk] = useState(false);
  const bulkInFlight = useRef(false);

  const runRowAction = useCallback(
    async (taskId: string, action: "pause" | "resume" | "cancel" | "retry" | "remove") => {
      // One command per row at a time: a second click while the first is in
      // flight would ask for a transition the first one has already made.
      if (rowInFlight.current.has(taskId)) return;
      rowInFlight.current.add(taskId);
      setBusyRows((current) => [...current, taskId]);
      setError(null);
      try {
        if (action === "pause") await api.pauseDownload(taskId);
        else if (action === "resume") await api.resumeDownload(taskId);
        else if (action === "cancel") await api.cancelDownload(taskId);
        else if (action === "retry") await api.retryDownload(taskId);
        else await api.removeDownload(taskId);
      } catch (raw) {
        setError(normalizeError(raw));
      } finally {
        rowInFlight.current.delete(taskId);
        setBusyRows((current) => current.filter((id) => id !== taskId));
        // The row is whatever the backend now says it is, not what was asked.
        await refresh();
      }
    },
    [refresh],
  );

  const runBulkAction = useCallback(
    async (action: "retryFailed" | "clearCompleted") => {
      if (bulkInFlight.current) return;
      bulkInFlight.current = true;
      setBusyBulk(true);
      setError(null);
      try {
        if (action === "retryFailed") await api.retryFailedDownloads();
        else await api.clearCompletedDownloads();
      } catch (raw) {
        setError(normalizeError(raw));
      } finally {
        bulkInFlight.current = false;
        setBusyBulk(false);
        await refresh();
      }
    },
    [refresh],
  );

  const openPath = useCallback(async (taskId: string, kind: "file" | "folder") => {
    setError(null);
    try {
      await api.openDownloadPath(taskId, kind);
    } catch (raw) {
      setError(normalizeError(raw));
    }
  }, []);

  const visible = useMemo(
    () => (filter === "all" ? tasks : tasks.filter((task) => task.state === filter)),
    [tasks, filter],
  );

  const hasFailed = (summary?.failed ?? 0) > 0;
  const hasCompleted = (summary?.completed ?? 0) > 0;

  return (
    <>
      <PageHeader
        title="Downloads"
        description="What is downloading now, what is waiting, and what has finished or failed."
      >
        <Button size="sm" loading={!hydrated} onClick={() => void refresh()}>
          Refresh
        </Button>
        {/* A second button because it does a second thing: Refresh re-reads the
            queue, this asks the filesystem. They can disagree — a task is
            finished whether or not its file is still on the disk. */}
        <Button size="sm" onClick={() => void rescan()} loading={scanning}>
          Check files
        </Button>
      </PageHeader>

      <div className="space-y-5">
        <ErrorNotice
          error={shownError}
          onDismiss={() => {
            setError(null);
            useDownloadStore.getState().clearError();
          }}
          title={shownError ? errorKindLabel(shownError) : undefined}
        />

        <Card
          title="Queue"
          description={
            summary
              ? `${summary.total} download${summary.total === 1 ? "" : "s"} tracked`
              : "Reading the queue…"
          }
          action={
            <div className="flex flex-wrap items-center gap-2">
              <Button
                size="sm"
                onClick={() => void runBulkAction("retryFailed")}
                disabled={!hasFailed || busyBulk}
                title={hasFailed ? "Re-queue every failed download" : "No failed downloads"}
              >
                Retry failed ({summary?.failed ?? 0})
              </Button>
              <ConfirmButton
                label={`Clear completed (${summary?.completed ?? 0})`}
                question="Remove the completed entries?"
                detail="Downloads and history are kept."
                confirmLabel="Clear entries"
                variant="ghost"
                busy={busyBulk}
                disabled={!hasCompleted || busyBulk}
                onConfirm={() => void runBulkAction("clearCompleted")}
              />
            </div>
          }
        >
          {summary ? (
            <div className="space-y-4">
              <dl className="grid grid-cols-2 gap-4 sm:grid-cols-4 lg:grid-cols-7">
                <Stat label="Total" value={summary.total} />
                <Stat label="Queued" value={summary.queued} />
                <Stat label="Active" value={summary.downloading} />
                <Stat label="Paused" value={summary.paused} />
                <Stat label="Completed" value={summary.completed} />
                <Stat label="Failed" value={summary.failed} />
                <Stat label="Cancelled" value={summary.cancelled} />
              </dl>

              {/* Aggregate over work in flight. Finished files are their own
                  number, because "how much has been transferred of what is
                  running" and "how much has landed" are different questions.

                  The totals are stated once, by the bar's own text: printing
                  them here as well made the same two numbers appear twice
                  within a couple of lines of each other. */}
              <div className="space-y-2 border-t border-surface-border pt-4">
                <span className="text-xs font-medium text-content-primary">In flight</span>
                <ProgressBar
                  downloadedBytes={summary.activeDownloadedBytes}
                  totalBytes={summary.activeTotalBytes}
                  tone="active"
                  label="Overall progress of unfinished downloads"
                />
                <p className="text-[11px] leading-relaxed text-content-muted">
                  {summary.completedBytes > 0 &&
                    `${formatBytes(summary.completedBytes)} of finished files on disk. `}
                  {summary.activeWithoutSize > 0 &&
                    `${summary.activeWithoutSize} unfinished task${
                      summary.activeWithoutSize === 1 ? "" : "s"
                    } whose size the source never reported ${
                      summary.activeWithoutSize === 1 ? "is" : "are"
                    } left out of this total rather than counted as zero.`}
                </p>
              </div>
            </div>
          ) : (
            <p className="text-xs text-content-muted">Waiting for the backend…</p>
          )}
        </Card>

        {tasks.length === 0 ? (
          <EmptyState
            title="No downloads yet"
            description="Analyze a link on the Single URL screen and download the media you choose; the task will appear here with real progress."
          />
        ) : (
          <Card
            title="Tasks"
            description={`${visible.length} of ${tasks.length} shown`}
            action={
              <Select
                value={filter}
                options={FILTERS}
                onValueChange={setFilter}
                className="w-40"
              />
            }
            flush
          >
            <ul className="divide-y divide-surface-border">
              {visible.map((task) => (
                <TaskRow
                  key={task.id}
                  task={task}
                  rate={rates[task.id] ?? null}
                  busy={busyRows.includes(task.id)}
                  onAction={(action) => void runRowAction(task.id, action)}
                  onOpenFile={() => void openPath(task.id, "file")}
                  onOpenFolder={() => void openPath(task.id, "folder")}
                />
              ))}
            </ul>
          </Card>
        )}
      </div>
    </>
  );
}

interface TaskRowProps {
  task: DownloadTask;
  rate: LiveRate | null;
  busy: boolean;
  onAction: (action: "pause" | "resume" | "cancel" | "retry" | "remove") => void;
  onOpenFile: () => void;
  onOpenFolder: () => void;
}

function TaskRow({ task, rate, busy, onAction, onOpenFile, onOpenFolder }: TaskRowProps) {
  const active = ACTIVE_STATES.includes(task.state);
  const tone =
    task.state === "completed" ? "success" : task.state === "failed" ? "danger" : "active";

  // Only ever shown for a task that is actually transferring: a speed on a
  // paused or finished row would describe nothing.
  const showRate = task.state === "downloading" && rate?.speedBytesPerSecond != null;
  // A bar is drawn for everything except a queued task, and the bar states the
  // bytes itself — so the sizes are only this row's job when there is no bar.
  const hasProgressBar = task.state !== "queued";
  const measuring = task.state === "downloading" && rate?.speedBytesPerSecond == null;

  return (
    <li className="space-y-2 px-5 py-3">
      <div className="flex flex-wrap items-center gap-3">
        <Badge tone={STATE_TONES[task.state]}>
          {humanize(task.state)}
        </Badge>
        <FileStateBadge taskId={task.id} state={task.state} />
        <span
          className="min-w-0 flex-1 truncate text-xs font-medium text-content-primary"
          title={task.fileName ?? task.sourceUrl}
        >
          {task.fileName ?? task.sourceUrl}
        </span>
        {task.attempts > 0 && (
          <span className="shrink-0 text-xs text-content-muted">
            {task.attempts}/{task.maxAttempts} attempts
          </span>
        )}
        <span className="shrink-0 text-xs text-content-muted">{formatDateTime(task.updatedAt)}</span>
      </div>

      {hasProgressBar && (
        <ProgressBar
          downloadedBytes={task.progress.downloadedBytes}
          totalBytes={task.progress.totalBytes}
          tone={tone}
        />
      )}

      {/*
        Only what the bar above does not already state. The bar prints the bytes
        and the percentage; printing the same bytes again here in a second format
        ("125 KB / 8.6 MB" above it, "125 KB of 8.6 MB" below it) is one fact
        shown twice, and the row read as if it were two measurements.
      */}
      {(!hasProgressBar || showRate || measuring) && (
        <div className="flex flex-wrap items-center gap-3 text-[11px] text-content-muted">
          {!hasProgressBar && (
            <span>
              {formatBytes(task.progress.downloadedBytes)}
              {task.progress.totalBytes !== null && ` of ${formatBytes(task.progress.totalBytes)}`}
            </span>
          )}
          {showRate && (
            <>
              <span>{formatBytes(rate?.speedBytesPerSecond ?? null)}/s</span>
              <span>
                {rate?.etaSeconds != null ? `${formatDuration(rate.etaSeconds)} left` : "time left unknown"}
              </span>
            </>
          )}
          {measuring && <span>measuring…</span>}
        </div>
      )}

      {task.error && (
        <p className="break-all font-mono text-[11px] text-danger">{task.error}</p>
      )}

      <div className="flex flex-wrap gap-2">
        {active && (
          <Button size="sm" disabled={busy} onClick={() => onAction("cancel")}>
            Cancel
          </Button>
        )}
        {(task.state === "downloading" || task.state === "queued") && (
          <Button size="sm" disabled={busy} onClick={() => onAction("pause")}>
            Pause
          </Button>
        )}
        {task.state === "paused" && (
          <Button size="sm" variant="primary" disabled={busy} onClick={() => onAction("resume")}>
            Resume
          </Button>
        )}
        {(task.state === "failed" || task.state === "cancelled") && (
          <Button size="sm" variant="primary" disabled={busy} onClick={() => onAction("retry")}>
            Retry
          </Button>
        )}
        {task.state === "completed" && (
          <>
            <Button size="sm" disabled={busy} onClick={onOpenFile}>
              Open file
            </Button>
            <Button size="sm" disabled={busy} onClick={onOpenFolder}>
              Open folder
            </Button>
          </>
        )}
        {/* Removing a *running* task is a cancellation first: the bytes have to
            stop before the row that describes them can go. */}
        {task.state !== "downloading" && (
          <Button
            size="sm"
            variant="ghost"
            disabled={busy}
            onClick={() => onAction("remove")}
            title="Removes this entry from the queue. The downloaded file is not touched."
          >
            Remove
          </Button>
        )}
      </div>
    </li>
  );
}

/** `125` → `2m 5s`. Used only for a running task with a known total. */
function formatDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "—";
  const whole = Math.round(seconds);
  if (whole < 60) return `${whole}s`;
  const minutes = Math.floor(whole / 60);
  const rest = whole % 60;
  if (minutes < 60) return rest === 0 ? `${minutes}m` : `${minutes}m ${rest}s`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${minutes % 60}m`;
}

function Stat({ label, value }: { label: string; value: number }) {
  return (
    <div>
      <dt className="text-xs text-content-muted">{label}</dt>
      <dd className="mt-1 text-lg font-semibold text-content-primary">{value}</dd>
    </div>
  );
}
