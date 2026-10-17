import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  Badge,
  Button,
  Card,
  EmptyState,
  ErrorNotice,
  PageHeader,
  Select,
  type BadgeTone,
} from "@/components/ui";
import { FOCUS_RING } from "@/lib/a11y";
import { Icon as IconObject } from "@/components/ui/icons";
import { recoveryHint } from "@/lib/errors";
import { formatDateTime, humanize } from "@/lib/format";
import { api } from "@/services/api";
import { normalizeError, type BackendError } from "@/services/ipc";
import { fileStateHint, fileStateLabel } from "@/stores/useFileStateStore";
import type {
  ContentStatus,
  ExportResult,
  LibraryCreator,
  LibraryEntry,
  LibraryPage,
  LibraryQuery,
  LibrarySort,
  LibraryView,
  ProfileDownloadResult,
} from "@/types";

const PAGE_SIZE = 50;

const VIEWS: { value: LibraryView; label: string; count: keyof LibraryPage["counts"] }[] = [
  { value: "all", label: "All", count: "all" },
  { value: "videos", label: "Videos", count: "videos" },
  { value: "images", label: "Images", count: "images" },
  { value: "audio", label: "Audio", count: "audio" },
  { value: "downloaded", label: "Downloaded", count: "downloaded" },
  { value: "failed", label: "Failed", count: "failed" },
  { value: "new", label: "New", count: "new" },
];

const SORTS: { value: LibrarySort; label: string }[] = [
  { value: "newest", label: "Newest first" },
  { value: "oldest", label: "Oldest first" },
  { value: "title_asc", label: "Title A–Z" },
  { value: "creator_asc", label: "Creator A–Z" },
  { value: "downloaded_first", label: "Downloaded first" },
];

const STATUSES: ContentStatus[] = [
  "new",
  "ready",
  "downloading",
  "downloaded",
  "failed",
  "unavailable",
  "private",
  "unsupported",
];

const STATUS_TONES: Record<ContentStatus, BadgeTone> = {
  new: "accent",
  ready: "warning",
  downloading: "brand",
  downloaded: "success",
  failed: "danger",
  unavailable: "neutral",
  private: "neutral",
  unsupported: "neutral",
};

/** Statuses that describe something a user can still download. */
const DOWNLOADABLE: ContentStatus[] = ["new", "ready"];

const inputClass =
  "w-full rounded-lg border border-surface-border bg-surface-base px-3 py-2 text-sm " +
  "text-content-primary placeholder:text-content-muted focus:border-brand focus:outline-none";

/** `3:05`, or the raw seconds when the value is odd enough to be worth seeing. */
function formatDuration(seconds: number | null): string {
  if (seconds === null || !Number.isFinite(seconds)) return "—";
  if (seconds < 3600) {
    const whole = Math.round(seconds);
    return `${Math.floor(whole / 60)}:${String(whole % 60).padStart(2, "0")}`;
  }
  return `${(seconds / 3600).toFixed(1)} h`;
}

/** The last path segment, which is what a user recognizes. */
function fileName(path: string | null): string | null {
  if (!path) return null;
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || null;
}

/**
 * A title that is never blank.
 *
 * Mirrors the backend's `Post::display_title`, so a row sorted by title reads
 * under the same name the backend sorted it by.
 */
function title(post: LibraryEntry["post"]): string {
  const caption = post.caption?.trim();
  return caption ? caption : `${post.platform} post ${post.platformPostId}`;
}

export function LibraryPage() {
  const [view, setView] = useState<LibraryView>("all");
  const [sort, setSort] = useState<LibrarySort>("newest");
  const [searchInput, setSearchInput] = useState("");
  const [search, setSearch] = useState("");
  const [creatorId, setCreatorId] = useState("");
  const [status, setStatus] = useState("");
  const [publishedAfter, setPublishedAfter] = useState("");
  const [publishedBefore, setPublishedBefore] = useState("");
  const [minDuration, setMinDuration] = useState("");
  const [minWidth, setMinWidth] = useState("");
  const [offset, setOffset] = useState(0);
  const [showFilters, setShowFilters] = useState(false);
  /** Cards or rows. Cards default: this is a media library, not a ledger. */
  const [layout, setLayout] = useState<"grid" | "list">("grid");
  /** Which format the one export control writes. */
  const [exportFormat, setExportFormat] = useState<"json" | "csv">("json");

  const [page, setPage] = useState<LibraryPage | null>(null);
  const [creators, setCreators] = useState<LibraryCreator[]>([]);
  const [error, setError] = useState<BackendError | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const [selected, setSelected] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [exported, setExported] = useState<ExportResult | null>(null);

  /**
   * Identifies the newest query, so a slow answer to an older one cannot
   * overwrite it. Typing in the search box starts a query per debounce, and
   * they do not necessarily come back in order.
   */
  const requestId = useRef(0);

  const query: LibraryQuery = useMemo(
    () => ({
      view,
      search: search.trim() ? search : null,
      sort,
      offset,
      limit: PAGE_SIZE,
      filter: {
        creatorId: creatorId || null,
        statuses: status ? [status as ContentStatus] : [],
        publishedAfter: publishedAfter || null,
        publishedBefore: publishedBefore || null,
        minDurationSeconds: minDuration ? Number(minDuration) : null,
        minWidth: minWidth ? Number(minWidth) : null,
      },
    }),
    [
      view,
      search,
      sort,
      offset,
      creatorId,
      status,
      publishedAfter,
      publishedBefore,
      minDuration,
      minWidth,
    ],
  );

  const load = useCallback(async (next: LibraryQuery) => {
    const id = ++requestId.current;
    setLoading(true);
    setError(null);
    try {
      const result = await api.queryLibrary(next);
      // Discarded rather than applied: an answer to a query the user has since
      // changed would show rows that do not match the controls on screen.
      if (id !== requestId.current) return;
      setPage(result);
    } catch (raw) {
      if (id !== requestId.current) return;
      setError(normalizeError(raw));
    } finally {
      if (id === requestId.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load(query);
  }, [load, query]);

  useEffect(() => {
    api
      .listLibraryCreators()
      .then(setCreators)
      .catch(() => {
        // The menu is a convenience; failing to fill it must not blank the page.
      });
  }, []);

  // Debounced so a query is not issued for every keystroke.
  useEffect(() => {
    const timer = window.setTimeout(() => {
      setSearch(searchInput);
      setOffset(0);
    }, 300);
    return () => window.clearTimeout(timer);
  }, [searchInput]);

  const filtersActive =
    Boolean(creatorId || status || publishedAfter || publishedBefore || minDuration || minWidth) ||
    Boolean(searchInput.trim());

  const clearFilters = () => {
    setSearchInput("");
    setSearch("");
    setCreatorId("");
    setStatus("");
    setPublishedAfter("");
    setPublishedBefore("");
    setMinDuration("");
    setMinWidth("");
    setOffset(0);
  };

  const entries: LibraryEntry[] = page?.entries ?? [];
  const selectableIds = useMemo(
    () => entries.filter((entry) => DOWNLOADABLE.includes(entry.status)).map((e) => e.post.id),
    [entries],
  );

  const toggle = (id: string) => {
    setSelected((current) =>
      current.includes(id) ? current.filter((value) => value !== id) : [...current, id],
    );
  };

  const downloadSelected = async () => {
    if (busy || selected.length === 0) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      // The same command the Profile Downloader uses. It resolves each stored
      // post, skips anything already downloaded or downloading, and creates the
      // task through the one central download manager — so the Library does not
      // become a second way of starting a download.
      const result: ProfileDownloadResult = await api.submitProfileDownloads({
        postIds: selected,
      });
      setNotice(
        [
          `${result.tasksCreated} queued`,
          result.skipped ? `${result.skipped} already downloaded` : null,
          result.unsupported ? `${result.unsupported} unsupported` : null,
          result.failed ? `${result.failed} failed` : null,
        ]
          .filter(Boolean)
          .join(" · "),
      );
      setSelected([]);
      await load(query);
    } catch (raw) {
      setError(normalizeError(raw));
    } finally {
      setBusy(false);
    }
  };

  const runExport = async (format: "json" | "csv") => {
    setExporting(true);
    setError(null);
    try {
      setExported(await api.exportLibrary(query, format));
    } catch (raw) {
      setError(normalizeError(raw));
    } finally {
      setExporting(false);
    }
  };

  return (
    <>
      <PageHeader
        title="Library"
        description="Everything this application has discovered or downloaded, with the state of each item on this machine. A post counts as downloaded only while the file it produced is still on disk."
      />

      {/*
        A group of mutually exclusive views. `role="group"` with a name is what
        tells a screen reader these buttons belong together and what they belong
        to; the pressed state on each is what says which one is showing.
      */}
      <div
        role="group"
        aria-label="Library view"
        className="mb-4 flex flex-wrap items-center gap-2"
      >
        {VIEWS.map((item) => {
          const active = item.value === view;
          const count = page?.counts[item.count] ?? 0;
          return (
            <button
              key={item.value}
              type="button"
              onClick={() => {
                setView(item.value);
                setOffset(0);
              }}
              aria-pressed={active}
              className={`rounded-lg border px-3 py-1.5 text-sm transition ${FOCUS_RING} ${
                active
                  ? "border-brand bg-brand-subtle text-brand"
                  : "border-surface-border bg-surface-base text-content-secondary hover:text-content-primary"
              }`}
            >
              {item.label}
              <span className="ml-2 text-xs text-content-muted">{count}</span>
            </button>
          );
        })}
      </div>

      <Card className="mb-4">
        <div className="flex flex-wrap items-center gap-3">
          <input
            type="search"
            value={searchInput}
            onChange={(event) => setSearchInput(event.target.value)}
            placeholder="Search creator, caption, post id, URL or filename"
            aria-label="Search the library"
            className={`${inputClass} min-w-64 flex-1`}
          />
          <div className="w-44">
            <Select
              value={sort}
              onValueChange={(value) => {
                setSort(value as LibrarySort);
                setOffset(0);
              }}
              options={SORTS}
              aria-label="Sort by"
            />
          </div>
          {/* The same records, drawn as cards or as rows. Cards lead because the
              library's content is pictures; rows stay for scanning details.
              The icons are a grid and a list — a folder and a checkbox were
              used here at first, and neither said what the control does. */}
          <div
            role="group"
            aria-label="Layout"
            className="flex overflow-hidden rounded-lg border border-surface-border"
          >
            <button
              type="button"
              onClick={() => setLayout("grid")}
              aria-pressed={layout === "grid"}
              title="Show as picture cards"
              aria-label="Show as picture cards"
              className={`px-2.5 py-2 ${FOCUS_RING} ${
                layout === "grid"
                  ? "bg-brand-subtle text-brand"
                  : "bg-surface-base text-content-secondary hover:text-content-primary"
              }`}
            >
              <IconObject.layoutGrid className="h-4 w-4" />
            </button>
            <button
              type="button"
              onClick={() => setLayout("list")}
              aria-pressed={layout === "list"}
              title="Show as a detailed list"
              aria-label="Show as a detailed list"
              className={`px-2.5 py-2 ${FOCUS_RING} ${
                layout === "list"
                  ? "bg-brand-subtle text-brand"
                  : "bg-surface-base text-content-secondary hover:text-content-primary"
              }`}
            >
              <IconObject.layoutList className="h-4 w-4" />
            </button>
          </div>
          <Button variant="ghost" onClick={() => setShowFilters((open) => !open)}>
            {showFilters ? "Hide filters" : "Filters"}
          </Button>
          {filtersActive && (
            <Button variant="ghost" onClick={clearFilters}>
              Clear
            </Button>
          )}
        </div>

        {showFilters && (
          <div className="mt-4 grid gap-3 border-t border-surface-border pt-4 sm:grid-cols-2 lg:grid-cols-3">
            <label className="text-xs text-content-secondary">
              Creator
              <Select
                className="mt-1"
                value={creatorId}
                onValueChange={(value) => {
                  setCreatorId(value);
                  setOffset(0);
                }}
                options={[
                  { value: "", label: "Any creator" },
                  ...creators.map((creator) => ({ value: creator.id, label: creator.name })),
                ]}
              />
            </label>
            <label className="text-xs text-content-secondary">
              Download status
              <Select
                className="mt-1"
                value={status}
                onValueChange={(value) => {
                  setStatus(value);
                  setOffset(0);
                }}
                options={[
                  { value: "", label: "Any status" },
                  ...STATUSES.map((value) => ({ value, label: humanize(value) })),
                ]}
              />
            </label>
            <label className="text-xs text-content-secondary">
              Published from
              <input
                type="date"
                value={publishedAfter}
                onChange={(event) => {
                  setPublishedAfter(event.target.value);
                  setOffset(0);
                }}
                className={`mt-1 ${inputClass}`}
              />
            </label>
            <label className="text-xs text-content-secondary">
              Published until
              <input
                type="date"
                value={publishedBefore}
                onChange={(event) => {
                  setPublishedBefore(event.target.value);
                  setOffset(0);
                }}
                className={`mt-1 ${inputClass}`}
              />
            </label>
            <label className="text-xs text-content-secondary">
              Minimum duration (seconds)
              <input
                type="number"
                min={0}
                value={minDuration}
                onChange={(event) => {
                  setMinDuration(event.target.value);
                  setOffset(0);
                }}
                placeholder="Any length"
                className={`mt-1 ${inputClass}`}
              />
            </label>
            <label className="text-xs text-content-secondary">
              Minimum width (pixels)
              <input
                type="number"
                min={0}
                value={minWidth}
                onChange={(event) => {
                  setMinWidth(event.target.value);
                  setOffset(0);
                }}
                placeholder="Any size"
                className={`mt-1 ${inputClass}`}
              />
            </label>
            <p className="self-end text-xs leading-relaxed text-content-muted lg:col-span-3">
              Duration and resolution are kept when the source did not report them: a post nobody
              measured is not a post that is too short or too small.
            </p>
          </div>
        )}
      </Card>

      <div className="mb-4 flex flex-wrap items-center gap-3">
        <Button
          onClick={downloadSelected}
          disabled={busy || selected.length === 0}
          loading={busy}
        >
          Download selected ({selected.length})
        </Button>
        <Button
          variant="ghost"
          onClick={() => setSelected(selectableIds)}
          disabled={selectableIds.length === 0}
        >
          Select new on this page
        </Button>
        <Button variant="ghost" onClick={() => setSelected([])} disabled={selected.length === 0}>
          Clear selection
        </Button>
        <div className="flex-1" />
        {/* One export control, two formats: a select keeps the row to one line
            and makes the shared action obvious, rather than two buttons that
            differ by a format name. */}
        <div className="flex items-center gap-2">
          <Select
            value={exportFormat}
            onValueChange={(value) => setExportFormat(value as "json" | "csv")}
            options={[
              { value: "json", label: "Export JSON" },
              { value: "csv", label: "Export CSV" },
            ]}
            aria-label="Export format"
            className="w-36"
          />
          <Button variant="secondary" onClick={() => void runExport(exportFormat)} disabled={exporting}>
            Export
          </Button>
        </div>
      </div>

      <ErrorNotice
        error={error}
        onDismiss={() => setError(null)}
        hint={error ? (recoveryHint(error) ?? undefined) : undefined}
      />

      {notice && (
        <div className="mb-4 flex items-center gap-3 rounded-xl border border-success/30 bg-success/5 px-4 py-3">
          <span className="text-sm text-content-primary">{notice}</span>
          <div className="flex-1" />
          <Button variant="ghost" size="sm" onClick={() => setNotice(null)}>
            Dismiss
          </Button>
        </div>
      )}

      {exported && (
        <div className="mb-4 rounded-xl border border-surface-border bg-surface-base px-4 py-3">
          <p className="text-sm text-content-primary">
            Exported {exported.records} {exported.records === 1 ? "record" : "records"} to{" "}
            <span className="font-mono text-xs">{exported.path}</span>
          </p>
          <p className="mt-1 text-xs leading-relaxed text-content-secondary">
            The file holds everything the current view, search and filters match — not only the
            rows on this page.
          </p>
          <div className="mt-2 flex gap-2">
            <Button
              variant="ghost"
              size="sm"
              onClick={() => void api.revealExport(exported.path).catch(() => undefined)}
            >
              Show in folder
            </Button>
            <Button variant="ghost" size="sm" onClick={() => setExported(null)}>
              Dismiss
            </Button>
          </div>
        </div>
      )}

      {loading && !page && <p className="text-sm text-content-muted">Loading the library…</p>}

      {loading && page && (
        // A refresh over rows already on screen. Silently keeping the old rows
        // made a large-library query feel stalled; this says it is working.
        <p className="text-xs text-content-muted" role="status">
          Updating the library…
        </p>
      )}

      {page && entries.length === 0 && (
        <EmptyState
          title={filtersActive ? "Nothing matches" : "The library is empty"}
          description={
            filtersActive
              ? "No item matches the current view, search and filters. Clearing them will widen the search again."
              : "Download something, or load a profile in the Profile Downloader, and it will appear here."
          }
          action={
            filtersActive ? <Button onClick={clearFilters}>Clear filters</Button> : undefined
          }
        />
      )}

      {entries.length > 0 && layout === "grid" && (
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5">
          {entries.map((entry) => (
            <LibraryCard
              key={entry.post.id}
              entry={entry}
              selected={selected.includes(entry.post.id)}
              onToggle={() => toggle(entry.post.id)}
            />
          ))}
        </div>
      )}

      {entries.length > 0 && layout === "list" && (
        /*
          Scrollable rather than clipped. The seven columns are wider than the
          content area, and `overflow-hidden` cut the last one off with no way
          to reach it — the Published column was simply not there at 1280 wide.
          `overflow-x-auto` keeps the rounded corners and lets the table be
          scrolled when it does not fit.
        */
        <Card flush className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead className="bg-surface-overlay text-left text-xs uppercase tracking-wide text-content-muted">
              <tr>
                <th className="w-10 px-3 py-2" />
                <th className="px-3 py-2">Title</th>
                <th className="px-3 py-2">Creator</th>
                <th className="px-3 py-2">Type</th>
                <th className="px-3 py-2">Status</th>
                <th className="px-3 py-2">Detail</th>
                <th className="px-3 py-2">Published</th>
              </tr>
            </thead>
            <tbody>
              {entries.map((entry) => (
                <LibraryRow
                  key={entry.post.id}
                  entry={entry}
                  selected={selected.includes(entry.post.id)}
                  onToggle={() => toggle(entry.post.id)}
                />
              ))}
            </tbody>
          </table>
        </Card>
      )}

      {page && page.total > 0 && (
        <div className="mt-4 flex items-center gap-3 text-sm text-content-secondary">
          <Button
            variant="ghost"
            disabled={offset === 0}
            onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}
          >
            Previous
          </Button>
          <span>
            {offset + 1}–{Math.min(offset + page.limit, page.total)} of {page.total}
            {page.scanned > page.total ? ` (from ${page.scanned} scanned)` : ""}
          </span>
          {page.truncated && (
            // The read stopped at its limit, so everything on screen — the
            // total, the tab counts, the pager — is a floor. Saying so here is
            // what makes the bound honest rather than a silent lie.
            <span
              className="text-xs text-warning"
              role="status"
              title="The library holds more posts than one query reads, so these totals and tab counts cover the first part of it."
            >
              Showing the first {page.scanned} of the match; the rest is not included.
            </span>
            )}
          <Button
            variant="ghost"
            disabled={offset + page.limit >= page.total}
            onClick={() => setOffset(offset + PAGE_SIZE)}
          >
            Next
          </Button>
        </div>
      )}
    </>
  );
}

interface LibraryRowProps {
  entry: LibraryEntry;
  selected: boolean;
  onToggle: () => void;
}

function LibraryRow({ entry, selected, onToggle }: LibraryRowProps) {
  const downloadable = DOWNLOADABLE.includes(entry.status);
  const resolution =
    entry.width !== null && entry.height !== null ? `${entry.width}×${entry.height}` : null;
  const file = fileName(entry.localPath);

  // The file's own state, when there is something to say about it.
  //
  // Silent for a file that is simply there: a column of "Present" badges would
  // bury the one row worth looking at, which is the rule the Downloads badge
  // already follows. What is left is the two states a user needs to tell apart —
  // a file that is confirmed gone, and a file nobody could check. The wording
  // comes from the same helpers that screen uses, so one condition is described
  // identically wherever it appears.
  const presence = useMemo(() => {
    const label = fileStateLabel(entry.fileState);
    if (!label) return null;
    return {
      label,
      tone: (entry.fileState === "missing" ? "warning" : "neutral") as BadgeTone,
      hint: fileStateHint(entry.fileState, entry.fileStateReason ?? undefined),
    };
  }, [entry.fileState, entry.fileStateReason]);

  return (
    <tr className="border-t border-surface-border align-top">
      <td className="px-3 py-3">
        <input
          type="checkbox"
          checked={selected}
          onChange={onToggle}
          disabled={!downloadable}
          aria-label={`Select ${title(entry.post)}`}
        />
      </td>
      <td className="max-w-md px-3 py-3">
        <p className="line-clamp-2 text-content-primary">{title(entry.post)}</p>
        <p className="mt-1 font-mono text-[11px] text-content-muted">
          {entry.post.platformPostId}
        </p>
        {entry.error && (
          <p className="mt-1 text-xs leading-relaxed text-content-secondary">{entry.error}</p>
        )}
      </td>
      <td className="px-3 py-3 text-content-secondary">
        {entry.creatorName ?? entry.post.creatorHandle ?? "—"}
      </td>
      <td className="px-3 py-3">
        <Badge tone="neutral">{humanize(entry.post.mediaType)}</Badge>
      </td>
      <td className="px-3 py-3">
        <Badge tone={STATUS_TONES[entry.status]}>{humanize(entry.status)}</Badge>
        {presence && (
          // Beside the status rather than folded into it: "Downloaded, but the
          // file could not be checked" is two facts, and collapsing them into
          // one badge would have to choose which to drop.
          <p className="mt-1" title={presence.hint ?? undefined}>
            <Badge tone={presence.tone}>{presence.label}</Badge>
          </p>
        )}
      </td>
      <td className="px-3 py-3 text-xs text-content-secondary">
        <p>{[resolution, formatDuration(entry.durationSeconds)].filter(Boolean).join(" · ") || "—"}</p>
        {entry.mediaItemCount > 0 && (
          <p className="mt-1">
            {entry.downloadedItemCount} of {entry.mediaItemCount} items on disk
          </p>
        )}
        {entry.downloadedAt && <p className="mt-1">Downloaded {formatDateTime(entry.downloadedAt)}</p>}
        {file && (
          <p className="mt-1 break-all font-mono text-[11px] text-content-muted">{file}</p>
        )}
      </td>
      <td className="px-3 py-3 text-xs text-content-secondary">
        {entry.post.publishedAt ? formatDateTime(entry.post.publishedAt) : "—"}
      </td>
    </tr>
  );
}

/** The icon that stands for a media type when there is no picture. */
const TYPE_ICONS = {
  video: IconObject.video,
  image: IconObject.image,
  carousel: IconObject.image,
  audio: IconObject.audio,
  text: IconObject.text,
} as const;

interface LibraryCardProps {
  entry: LibraryEntry;
  selected: boolean;
  onToggle: () => void;
}

/**
 * One library item, as a card.
 *
 * The post's own thumbnail leads; the state badges and the selection control
 * sit beneath it, so the grid reads as pictures first and facts second. The
 * selection checkbox is the only always-visible control — a card grid with a
 * button under every picture is a toolbar pretending to be a gallery.
 *
 * What the row layout says, this card says too: the presence hint travels on
 * the badge's title, and a post with something to explain keeps its sentence.
 * A layout that drops the explanation because the picture was prettier without
 * it would be two screens telling two truths.
 */
function LibraryCard({ entry, selected, onToggle }: LibraryCardProps) {
  const downloadable = DOWNLOADABLE.includes(entry.status);
  const TypeIcon = TYPE_ICONS[entry.post.mediaType] ?? IconObject.text;
  const file = fileName(entry.localPath);

  // The same presence reading the row uses, from the same helpers: one
  // condition described identically in both layouts.
  const presence = useMemo(() => {
    const label = fileStateLabel(entry.fileState);
    if (!label) return null;
    return {
      label,
      tone: (entry.fileState === "missing" ? "warning" : "neutral") as BadgeTone,
      hint: fileStateHint(entry.fileState, entry.fileStateReason ?? undefined),
    };
  }, [entry.fileState, entry.fileStateReason]);

  return (
    <div
      className={`overflow-hidden rounded-xl border bg-surface-raised transition-colors ${
        selected ? "border-brand ring-1 ring-brand/40" : "border-surface-border"
      }`}
    >
      <div className="relative">
        {entry.post.thumbnailUrl ? (
          <img
            src={entry.post.thumbnailUrl}
            alt=""
            loading="lazy"
            className="aspect-[4/5] w-full object-cover"
          />
        ) : (
          <div className="flex aspect-[4/5] w-full items-center justify-center bg-surface-overlay text-content-muted">
            <TypeIcon className="h-10 w-10" />
          </div>
        )}
        {/* Status over the picture: seen first, told apart by word as well as
            tone, and out of the way of the checkbox below. */}
        <span className="absolute left-2 top-2">
          <Badge tone={STATUS_TONES[entry.status]}>{humanize(entry.status)}</Badge>
        </span>
        {presence && (
          <span className="absolute right-2 top-2" title={presence.hint ?? undefined}>
            <Badge tone={presence.tone}>{presence.label}</Badge>
          </span>
        )}
      </div>

      <div className="space-y-1.5 p-3">
        <p
          className="line-clamp-2 min-h-8 text-xs leading-snug text-content-primary"
          title={title(entry.post)}
        >
          {title(entry.post)}
        </p>
        <p className="flex items-center gap-1.5 text-[11px] text-content-muted">
          {/* The card is narrow and creator names are not bounded, so the whole
              name travels on the title — a clamped name with nothing to hover
              leaves the card unable to say whose post it is. */}
          <span
            className="truncate"
            title={entry.creatorName ?? entry.post.creatorHandle ?? undefined}
          >
            {entry.creatorName ?? entry.post.creatorHandle ?? "—"}
          </span>
          <span aria-hidden="true">·</span>
          <span className="shrink-0">{humanize(entry.post.mediaType)}</span>
        </p>
        {entry.error && (
          <p className="line-clamp-2 text-[11px] leading-relaxed text-content-secondary">
            {entry.error}
          </p>
        )}
        {file && (
          <p className="truncate font-mono text-[10px] text-content-muted" title={file}>
            {file}
          </p>
        )}
        <div className="flex items-center justify-between pt-0.5">
          <label className="flex cursor-pointer items-center gap-1.5 text-[11px] text-content-secondary">
            <input
              type="checkbox"
              checked={selected}
              onChange={onToggle}
              disabled={!downloadable}
              aria-label={`Select ${title(entry.post)}`}
            />
            {selected ? "Selected" : downloadable ? "Select" : "—"}
          </label>
          {entry.mediaItemCount > 0 && (
            <span className="text-[10px] text-content-muted">
              {entry.downloadedItemCount}/{entry.mediaItemCount} on disk
            </span>
          )}
        </div>
      </div>
    </div>
  );
}
