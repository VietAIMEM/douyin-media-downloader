import { Link } from "react-router-dom";

import {
  Badge,
  Card,
  EmptyState,
  PageHeader,
  ProgressBar,
} from "@/components/ui";
import { Icon as IconObject } from "@/components/ui/icons";
import { formatDateTime, humanize } from "@/lib/format";
import { useDownloadStore, useDownloadTasks } from "@/stores/useDownloadStore";
import type { DownloadState, DownloadTask } from "@/types";

const STATE_TONES: Record<DownloadState, "neutral" | "brand" | "warning" | "success" | "danger"> = {
  queued: "neutral",
  downloading: "brand",
  paused: "warning",
  completed: "success",
  failed: "danger",
  cancelled: "neutral",
};

/**
 * Home.
 *
 * The previous page answered "what did the developers build" — an implemented
 * checklist, a phase list, a diagnostics read-out. This one answers the three
 * questions a user arrives with: how do I start, what is happening, and where
 * is my media. Each section exists only when it has something to say: an empty
 * application shows its two entry points and nothing else.
 *
 * Diagnostics were not deleted — they moved to Settings → Application, which is
 * where a person looks when something is wrong.
 */
export function HomePage() {
  const tasks = useDownloadTasks();
  const summary = useDownloadStore((state) => state.summary);

  // The four tasks the user most recently touched, oldest at the bottom — the
  // queue order the backend maintains is exactly that.
  const recent = tasks.slice(-4).reverse();
  const activeCount = summary
    ? summary.queued + summary.downloading + summary.paused
    : tasks.filter(
        (task) => task.state === "queued" || task.state === "downloading" || task.state === "paused",
      ).length;
  const completedCount = summary?.completed ?? tasks.filter((task) => task.state === "completed").length;
  const hasAny = tasks.length > 0;

  return (
    <>
      <PageHeader
        title="Home"
        description="Download public Douyin media: paste a link, or browse a creator's posts, then pick what to keep."
      />

      <div className="space-y-5">
        {/* The two ways in, as the page's first content. They are links styled
            like panels rather than buttons, so a click is navigation and the
            keyboard treats them as links. */}
        <div className="grid gap-4 sm:grid-cols-2">
          <Link
            to="/single"
            className="animate-rise block rounded-xl border border-surface-border bg-surface-raised p-5 transition-colors hover:border-brand/60"
          >
            <span className="flex items-start gap-3.5">
              <span className="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg bg-brand text-white">
                <IconObject.download className="h-5 w-5" />
              </span>
              <span className="min-w-0">
                <span className="block text-sm font-semibold text-content-primary">
                  Download from a link
                </span>
                <span className="mt-1 block text-xs leading-relaxed text-content-secondary">
                  Paste one Douyin post link, preview what it holds, and download what you want.
                </span>
              </span>
            </span>
          </Link>

          <Link
            to="/profile"
            className="animate-rise block rounded-xl border border-surface-border bg-surface-raised p-5 transition-colors hover:border-brand/60"
          >
            <span className="flex items-start gap-3.5">
              <span className="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg bg-accent text-white">
                <IconObject.users className="h-5 w-5" />
              </span>
              <span className="min-w-0">
                <span className="block text-sm font-semibold text-content-primary">
                  Download from a profile
                </span>
                <span className="mt-1 block text-xs leading-relaxed text-content-secondary">
                  Browse a creator's public posts, select, and queue the ones you want.
                </span>
              </span>
            </span>
          </Link>
        </div>

        {/* What the queue is doing right now, when it is doing anything. The
            same rows the Downloads screen reads, so the two cannot disagree. */}
        {hasAny && (
          <Card
            title="Download status"
            description={
              activeCount > 0
                ? `${activeCount} in progress or waiting · ${completedCount} completed`
                : `${completedCount} completed · nothing running`
            }
            action={
              <Link to="/downloads" className="text-xs font-medium text-brand hover:underline">
                Open Downloads
              </Link>
            }
          >
            <div className="space-y-3">
              {recent.map((task) => (
                <RecentRow key={task.id} task={task} />
              ))}
            </div>
          </Card>
        )}

        {/* Where media lives, once there is any. Three answers to "where is my
            media", each naming what it actually is — the disk, the database,
            the record. */}
        {completedCount > 0 && (
          <Card title="Find your media">
            <div className="grid gap-3 sm:grid-cols-3">
              <Link
                to="/library"
                className="rounded-lg border border-surface-border bg-surface-base p-3.5 transition-colors hover:border-brand/60"
              >
                <span className="block text-xs font-medium text-content-primary">Library</span>
                <span className="mt-1 block text-[11px] leading-relaxed text-content-muted">
                  Search everything this application knows about.
                </span>
              </Link>
              <Link
                to="/dashboard"
                className="rounded-lg border border-surface-border bg-surface-base p-3.5 transition-colors hover:border-brand/60"
              >
                <span className="block text-xs font-medium text-content-primary">
                  Download Folder
                </span>
                <span className="mt-1 block text-[11px] leading-relaxed text-content-muted">
                  Look at the files on disk, folder by folder.
                </span>
              </Link>
              <Link
                to="/history"
                className="rounded-lg border border-surface-border bg-surface-base p-3.5 transition-colors hover:border-brand/60"
              >
                <span className="block text-xs font-medium text-content-primary">History</span>
                <span className="mt-1 block text-[11px] leading-relaxed text-content-muted">
                  Every download that finished, succeeded or not.
                </span>
              </Link>
            </div>
          </Card>
        )}

        {/* A first run: the two entry points, and no invented activity. The
            empty state offers the primary action rather than narrating phases. */}
        {!hasAny && (
          <EmptyState
            title="Nothing downloaded yet"
            description="Paste a Douyin link on the Single URL screen, or browse a creator's posts in Profile Downloader. What you download will show up here and in the Library."
          />
        )}
      </div>
    </>
  );
}

/** One recent task, compact: name, state, and how far it has gone. */
function RecentRow({ task }: { task: DownloadTask }) {
  const active =
    task.state === "queued" || task.state === "downloading" || task.state === "paused";

  return (
    <div className="flex items-center gap-4">
      <div className="min-w-0 flex-1">
        <p className="truncate text-xs font-medium text-content-primary">
          {task.fileName ?? task.sourceUrl}
        </p>
        <p className="mt-0.5 text-[11px] text-content-muted">{formatDateTime(task.updatedAt)}</p>
      </div>
      {active && (
        <div className="w-28 shrink-0">
          <ProgressBar
            downloadedBytes={task.progress.downloadedBytes}
            totalBytes={task.progress.totalBytes}
            tone={task.state === "paused" ? "muted" : "active"}
            label={`Progress of ${task.fileName ?? task.sourceUrl}`}
          />
        </div>
      )}
      <Badge tone={STATE_TONES[task.state]}>{humanize(task.state)}</Badge>
      <Link
        to="/downloads"
        className="shrink-0 rounded p-1 text-content-muted hover:text-content-primary"
        aria-label={`Open in Downloads`}
      >
        <IconObject.external className="h-3.5 w-3.5" />
      </Link>
    </div>
  );
}
