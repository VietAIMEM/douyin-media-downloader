import { useCallback, useEffect, useMemo, useState, type FormEvent } from "react";

import {
  Badge,
  Button,
  Card,
  EmptyState,
  ErrorNotice,
  Field,
  KeyValue,
  KeyValueList,
  PageHeader,
  ProgressBar,
  TextInput,
  type BadgeTone,
} from "@/components/ui";
import { FOCUS_RING } from "@/lib/a11y";
import { errorKindLabel, recoveryHint } from "@/lib/errors";
import { formatBytes, formatDateTime, humanize } from "@/lib/format";
import { api } from "@/services/api";
import { normalizeError, type BackendError } from "@/services/ipc";
import { useDownloadStore, useDownloadTask } from "@/stores/useDownloadStore";
import type {
  AnalyzedUrl,
  DownloadState,
  DownloadTask,
  Id,
  MediaItem,
  MediaVariant,
  Metadata,
  SavedText,
  TrackLayout,
} from "@/types";
import { mediaTrackLayout, mediaTypeLabel } from "@/types";

/**
 * The explicit state model the screen renders.
 *
 * Every outcome is named rather than collapsed into "failed", so a user can tell
 * an invalid link from a source that would not answer from content that is
 * unsupported:
 *
 * ```text
 * idle → validating → resolving → resolved → downloading → completed
 *                              ↘ unsupported | unavailable | error
 * ```
 */
type Status =
  | "idle"
  | "validating"
  | "resolving"
  | "resolved"
  | "unsupported"
  | "unavailable"
  | "error"
  | "downloading"
  | "completed";

const STATUS_TONES: Record<DownloadState, BadgeTone> = {
  queued: "neutral",
  downloading: "brand",
  paused: "warning",
  completed: "success",
  failed: "danger",
  cancelled: "neutral",
};

interface Selection {
  mediaItemId: string;
  variantId: string | null;
}

/** Best variant to offer by default: a clean source if one is available. */
function preferredVariant(item: MediaItem): MediaVariant | undefined {
  return item.variants.find((variant) => variant.isOriginal) ?? item.variants[0];
}

/** One logical rendition, and every candidate URL that can serve it. */
export interface VariantGroup {
  /** The variant the selection and the transfer use. */
  representative: MediaVariant;
  /** Every variant of this rendition, the representative included. */
  members: MediaVariant[];
}

/**
 * The item's variants, one entry per logical rendition rather than per URL.
 *
 * The resolver records every candidate the source offered as its own variant,
 * because any of them may be the one that actually serves the bytes. That is
 * right for the transfer and wrong for a list of choices: Douyin was observed
 * serving a single "Original / Clean" rendition from three different hosts,
 * which appeared as three identical options, and one "Play endpoint" from two,
 * which appeared as two. A user cannot choose between mirrors, and asking them
 * to is noise.
 *
 * The grouping key is the rendition's own identity — kind, container, codec,
 * geometry, bitrate — and never the label, so two genuinely different
 * renditions that happen to share a label stay separate options. Nothing is
 * discarded: the group's first variant is what gets submitted, which is the
 * resolver's own ranking, and the transfer still falls back through the item's
 * remaining variants when one mirror refuses.
 */
export function logicalVariantGroups(item: MediaItem): VariantGroup[] {
  const identity = (variant: MediaVariant) =>
    [
      variant.kind,
      variant.mimeType ?? "",
      variant.extension ?? "",
      variant.width ?? "",
      variant.height ?? "",
      variant.bitrateKbps ?? "",
    ].join("|");

  const groups = new Map<string, VariantGroup>();
  for (const variant of item.variants) {
    const key = identity(variant);
    const group = groups.get(key);
    if (group) group.members.push(variant);
    else groups.set(key, { representative: variant, members: [variant] });
  }
  return [...groups.values()];
}

/**
 * The tracks an item was found to have, as a badge.
 *
 * Douyin serves a video and its audio as two separate renditions, so what the
 * download will contain is worth stating before it is started rather than
 * discovered afterwards. A video whose audio could not be checked is *not*
 * described as one that has none.
 */
function TrackLayoutBadge({ layout }: { layout: TrackLayout | null }) {
  switch (layout) {
    case "audio_video":
      return <Badge tone="success">video + audio</Badge>;
    case "video_only":
      return <Badge tone="warning">video only — source has no audio</Badge>;
    case "audio_only":
      return <Badge tone="neutral">audio only</Badge>;
    case "unknown":
      return <Badge tone="neutral">audio not checked</Badge>;
    default:
      return null;
  }
}

/** Which explicit status a backend failure maps onto. */
function statusForError(error: BackendError): Status {
  if (error.kind === "unsupported_content") return "unsupported";
  if (error.kind === "unavailable_content") return "unavailable";
  return "error";
}

export function SingleUrlPage() {
  const [url, setUrl] = useState("");
  const [analysis, setAnalysis] = useState<AnalyzedUrl | null>(null);
  const [selection, setSelection] = useState<Selection | null>(null);
  /**
   * Which pictures the user has chosen. A post's images are chosen as a set
   * rather than one at a time; the video path keeps its own single selection,
   * because a video is one thing with renditions, not a group of items.
   */
  const [selectedImages, setSelectedImages] = useState<Set<Id>>(new Set());
  /**
   * Only the id. The task itself lives in the shared store, so this screen and
   * the Downloads screen are looking at the same row rather than at two copies
   * that can disagree — and a completion that arrives while this screen is
   * hidden still lands, because something is always listening.
   */
  const [taskId, setTaskId] = useState<Id | null>(null);
  const task = useDownloadTask(taskId);
  const [error, setError] = useState<BackendError | null>(null);
  const [busy, setBusy] = useState<"analyze" | "download" | null>(null);

  /*
   * The field starts empty and nothing is read until the user asks for it.
   *
   * It used to arrive holding one of the project's own test links, fetched
   * from the backend on mount. That put a stranger's video in the box before
   * the user had done anything, and it meant a fresh visit to this screen
   * always woke the resolver. Both are gone: the placeholder says what to
   * paste, and the first request of any kind is the one the user submits.
   */

  // The live stream is subscribed to once, by the application root; this screen
  // reads the store it feeds. See `stores/useDownloadStore`.

  /**
   * Analyze a link the user supplied.
   *
   * The only caller is the form's submit handler, so this runs when — and only
   * when — the user has asked for it with a link in the box. An empty box is
   * not a request, and does nothing at all rather than resolving nothing.
   */
  const analyze = useCallback(
    async (candidate: string) => {
      const trimmed = candidate.trim();
      if (trimmed === "") return;

      setBusy("analyze");
      setError(null);
      setAnalysis(null);
      setSelection(null);
      setTaskId(null);
      setSelectedImages(new Set());

      try {
        const result = await api.analyzeUrl(trimmed);
        setAnalysis(result);
        // Default to the first media item and its preferred variant.
        const first = result.media[0];
        if (first) {
          setSelection({ mediaItemId: first.id, variantId: preferredVariant(first)?.id ?? null });
        }
        // Every picture starts chosen: the point of showing a carousel as a
        // gallery is that it is one action, not one per slide.
        setSelectedImages(
          new Set(
            result.media
              .filter((item) => item.mediaType === "image")
              .map((item) => item.id),
          ),
        );
      } catch (raw) {
        setError(normalizeError(raw));
      } finally {
        setBusy(null);
      }
    },
    [],
  );

  const startDownload = useCallback(async () => {
    if (!analysis?.post || !selection) return;
    setBusy("download");
    setError(null);
    try {
      const created = await api.createDownloadTask({
        postId: analysis.post.id,
        mediaItemId: selection.mediaItemId,
        variantId: selection.variantId,
      });
      setTaskId(created.id);
      useDownloadStore.getState().putTasks([created]);
    } catch (raw) {
      setError(normalizeError(raw));
    } finally {
      setBusy(null);
    }
  }, [analysis, selection]);

  /** Choose or unchoose one picture, without disturbing the others. */
  const toggleImage = useCallback((id: Id) => {
    setSelectedImages((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  /**
   * Queue every chosen picture, one task each.
   *
   * A picture that will not queue does not stop the rest: what was created is
   * kept, and the failure is reported. The gallery then shows a task against
   * every slide that has one, so which of them needs attention is visible
   * without reading the message.
   */
  const startImageDownloads = useCallback(async () => {
    const post = analysis?.post;
    if (!post) return;
    const chosen = analysis.media.filter((item) => selectedImages.has(item.id));
    if (chosen.length === 0) return;

    setBusy("download");
    setError(null);
    const created: DownloadTask[] = [];
    let firstFailure: unknown = null;
    try {
      for (const item of chosen) {
        try {
          created.push(
            await api.createDownloadTask({
              postId: post.id,
              mediaItemId: item.id,
              variantId: preferredVariant(item)?.id ?? null,
            }),
          );
        } catch (raw) {
          if (firstFailure === null) firstFailure = raw;
        }
      }
      if (created.length > 0) useDownloadStore.getState().putTasks(created);
      if (firstFailure !== null) setError(normalizeError(firstFailure));
    } finally {
      setBusy(null);
    }
  }, [analysis, selectedImages]);

  /** Try one failed picture again, leaving its neighbours alone. */
  const retryImage = useCallback(async (imageTaskId: Id) => {
    setError(null);
    try {
      const updated = await api.retryDownload(imageTaskId);
      useDownloadStore.getState().putTasks([updated]);
    } catch (raw) {
      setError(normalizeError(raw));
    }
  }, []);

  const runTaskAction = useCallback(
    async (action: "cancel" | "retry") => {
      if (!task) return;
      setError(null);
      try {
        const updated =
          action === "cancel" ? await api.cancelDownload(task.id) : await api.retryDownload(task.id);
        useDownloadStore.getState().putTasks([updated]);
      } catch (raw) {
        setError(normalizeError(raw));
      }
    },
    [task],
  );

  const openPath = useCallback(
    async (kind: "file" | "folder") => {
      if (!task) return;
      setError(null);
      try {
        await api.openDownloadPath(task.id, kind);
      } catch (raw) {
        setError(normalizeError(raw));
      }
    },
    [task],
  );

  const status = deriveStatus(busy, analysis, error, task);

  return (
    <>
      <PageHeader
        title="Single URL"
        description="Paste one public Douyin post link to analyze it, preview what is available, and download the media you choose."
      />

      <div className="space-y-5">
        <Card>
          <form
            onSubmit={(event: FormEvent) => {
              event.preventDefault();
              void analyze(url);
            }}
            className="space-y-4"
          >
            <Field
              label="Douyin link"
              hint="A single video, image, or carousel post — a short link is fine."
              error={error?.kind === "validation" ? error.message : undefined}
            >
              <TextInput
                value={url}
                onChange={(event) => setUrl(event.target.value)}
                placeholder="Paste a Douyin video or post link"
                spellCheck={false}
                autoComplete="off"
              />
            </Field>

            <div className="flex flex-wrap items-center gap-2">
              <Button type="submit" variant="primary" loading={busy === "analyze"}>
                Analyze
              </Button>
              <StatusBadge status={status} />
            </div>

            {/* The wait, and where it comes from. A post page is read with a
                browser the app manages, and saying so is better than leaving a
                user to wonder what is taking the time. */}
            <p className="text-xs leading-relaxed text-content-muted">
              Douyin pages are read with a browser the app manages. It is kept off-screen and
              reused for the whole session, so resolving several links does not open several
              windows.
            </p>
          </form>
        </Card>

        <ErrorNotice
          error={error}
          onDismiss={() => setError(null)}
          title={error ? errorKindLabel(error) : undefined}
          hint={error ? (recoveryHint(error) ?? undefined) : undefined}
        />

        {analysis?.notice && (
          <Card title="Not a single post">
            <p className="text-xs leading-relaxed text-content-secondary">{analysis.notice}</p>
            <div className="mt-3">
              <KeyValueList>
                <KeyValue label="Link type" mono>
                  {analysis.classification.label}
                </KeyValue>
                <KeyValue label="Creator id" mono>
                  {analysis.creator?.platformId ?? "—"}
                </KeyValue>
              </KeyValueList>
            </div>
          </Card>
        )}

        {analysis?.post && (
          <>
            <PreviewCard analysis={analysis} />
            {isTextPost(analysis) ? (
              <TextPostCard analysis={analysis} />
            ) : isImageGallery(analysis) ? (
              <ImageGallery
                items={analysis.media}
                selected={selectedImages}
                onToggle={toggleImage}
                onSelectAll={() =>
                  setSelectedImages(new Set(analysis.media.map((item) => item.id)))
                }
                onClear={() => setSelectedImages(new Set())}
                onDownload={() => void startImageDownloads()}
                onRetry={(id) => void retryImage(id)}
                downloading={busy === "download"}
              />
            ) : (
              <MediaCard
                analysis={analysis}
                selection={selection}
                onSelect={setSelection}
                onDownload={() => void startDownload()}
                downloading={busy === "download" || isActive(task)}
              />
            )}
          </>
        )}

        {task && (
          <DownloadCard
            task={task}
            onCancel={() => void runTaskAction("cancel")}
            onRetry={() => void runTaskAction("retry")}
            onOpenFile={() => void openPath("file")}
            onOpenFolder={() => void openPath("folder")}
          />
        )}

        {status === "idle" && !analysis && (
          <EmptyState
            title="Paste a Douyin link to begin"
            description="The link is validated and the media it offers is read by the application itself, then shown here before anything is downloaded."
          />
        )}
      </div>
    </>
  );
}

/** Whether the task is still doing something, and so worth showing progress for. */
function isActive(task: DownloadTask | undefined | null): boolean {
  return (
    task != null &&
    (task.state === "queued" || task.state === "downloading" || task.state === "paused")
  );
}

function deriveStatus(
  busy: "analyze" | "download" | null,
  analysis: AnalyzedUrl | null,
  error: BackendError | null,
  task: DownloadTask | null | undefined,
): Status {
  if (busy === "analyze") return "validating";
  if (busy === "download") return "resolving";
  if (task) {
    if (task.state === "completed") return "completed";
    return "downloading";
  }
  if (error) return statusForError(error);
  if (analysis?.intent === "profile") return "resolved";
  if (analysis?.post) return analysis.resolutionStage === "resolved" ? "resolved" : "resolving";
  return "idle";
}

/**
 * What each state is called on screen.
 *
 * The state model is the resolver's, so the words would otherwise be the
 * resolver's too — "validating", "resolving". These name what is happening to
 * the user's link instead: it is being checked, the page is being read, and the
 * media is ready to choose from.
 */
const STATUS_LABELS: Record<Status, { label: string; tone: BadgeTone }> = {
  idle: { label: "Idle", tone: "neutral" },
  validating: { label: "Checking link", tone: "brand" },
  resolving: { label: "Reading the page", tone: "brand" },
  resolved: { label: "Ready to choose", tone: "success" },
  unsupported: { label: "Not supported", tone: "warning" },
  unavailable: { label: "Not available", tone: "warning" },
  error: { label: "Could not read", tone: "danger" },
  downloading: { label: "Downloading", tone: "brand" },
  completed: { label: "Downloaded", tone: "success" },
};

function StatusBadge({ status }: { status: Status }) {
  // Nothing to report before anything has been asked of the screen. A badge
  // reading "Idle" is a state of the machine, not of the user's work.
  if (status === "idle") return null;

  const { label, tone } = STATUS_LABELS[status];
  return (
    <Badge tone={tone}>
      {label}
    </Badge>
  );
}

function PreviewCard({ analysis }: { analysis: AnalyzedUrl }) {
  const { post, creator } = analysis;
  if (!post) return null;

  const video = analysis.media.find((item) => item.mediaType === "video");
  const soundtrack = describeSoundtrack(post.metadata);

  return (
    <Card
      title="Preview"
      description="What this post holds, as the source described it"
      action={
        /*
         * Only what adds something to the state badge beside Analyze. The
         * resolver's own stage is already said by that badge — repeating it here
         * in its internal vocabulary ("resolved", "partial") is the kind of
         * detail that makes a screen read like a log.
         *
         * Reading a page through a browser is different, and slower, so it is
         * worth naming: it explains a wait the user just sat through.
         */
        analysis.resolvedVia === "browser" ? <Badge tone="neutral">read via browser</Badge> : null
      }
    >
      <div className="grid gap-5 sm:grid-cols-[minmax(0,180px)_minmax(0,1fr)]">
        <Thumbnail url={post.thumbnailUrl} />

        <div className="space-y-3">
          <KeyValueList>
            <KeyValue label="Creator">
              {creator?.displayName ?? post.creatorHandle ?? "—"}
              {creator?.handle ? (
                <span className="ml-1.5 text-content-muted">{creator.handle}</span>
              ) : null}
            </KeyValue>
            <KeyValue label="Type">{mediaTypeLabel(post.mediaType)}</KeyValue>
            <KeyValue label="Duration">
              {video?.durationSeconds ? `${video.durationSeconds.toFixed(1)}s` : "—"}
            </KeyValue>
            <KeyValue label="Resolution">
              {video?.width && video.height ? `${video.width}×${video.height}` : "—"}
            </KeyValue>
            <KeyValue label="Post id" mono>
              {post.platformPostId}
            </KeyValue>
            <KeyValue label="Published">{formatDateTime(post.publishedAt)}</KeyValue>
            {soundtrack && (
              <KeyValue label="Soundtrack">
                {[soundtrack.title, soundtrack.author].filter(Boolean).join(" — ") ||
                  "credited by the source without a title"}
                {/* The credit is shown either way. Whether there is a file to go
                    with it is a separate fact, and saying so is the difference
                    between a post whose music can be downloaded and one whose
                    music the source merely named. */}
                {!soundtrack.downloadable && (
                  <span className="ml-1.5 text-content-muted">no downloadable audio</span>
                )}
              </KeyValue>
            )}
          </KeyValueList>

          {post.caption && (
            <p className="border-t border-surface-border pt-3 text-xs leading-relaxed text-content-secondary">
              {post.caption}
            </p>
          )}
        </div>
      </div>
    </Card>
  );
}

function Thumbnail({ url }: { url: string | null }) {
  const [failed, setFailed] = useState(false);
  // A fresh analysis can bring a working thumbnail for a previously broken one.
  useEffect(() => setFailed(false), [url]);

  if (!url || failed) {
    return (
      <div className="flex aspect-[9/16] max-h-64 items-center justify-center rounded-lg border border-surface-border bg-surface-overlay text-xs text-content-muted">
        No preview
      </div>
    );
  }
  return (
    <img
      src={url}
      alt=""
      onError={() => setFailed(true)}
      className="aspect-[9/16] max-h-64 w-full rounded-lg border border-surface-border object-cover"
    />
  );
}

interface MediaCardProps {
  analysis: AnalyzedUrl;
  selection: Selection | null;
  onSelect: (selection: Selection) => void;
  onDownload: () => void;
  downloading: boolean;
}

/**
 * Whether this post is shown as a gallery rather than a list of items.
 *
 * A post is a gallery when what it holds is pictures — the type the resolver
 * settled on from what it actually found, not the route it was reached by. A
 * video post keeps the list and its rendition selector, because a video is one
 * thing with several renditions rather than a group of things.
 */
function isImageGallery(analysis: AnalyzedUrl): boolean {  const kind = analysis.post?.mediaType;
  if (kind !== "image" && kind !== "carousel") return false;
  return analysis.media.some((item) => item.mediaType === "image");
}

/** Whether this post's content is its words. */
export function isTextPost(analysis: AnalyzedUrl): boolean {
  return analysis.post?.mediaType === "text";
}

/**
 * A post whose content is its words.
 *
 * Douyin publishes caption-only posts, and this is what one looks like when the
 * resolver reports it honestly. It is not a failed video and not a post with a
 * broken download: there is nothing to fetch, and the way to keep it is to write
 * it out. Saving is therefore separate from the download path — no task, no
 * queue entry, no history row — because a caption that appeared in the download
 * history as a "completed download" would be a record of a transfer that never
 * happened.
 */
function TextPostCard({ analysis }: { analysis: AnalyzedUrl }) {
  const post = analysis.post;
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState<SavedText | null>(null);
  const [failure, setFailure] = useState<BackendError | null>(null);

  // What the source stated, in the order it stated it. The full text lives in
  // metadata when the post has one; the caption is where the same words are for
  // a post that also carries media.
  const metadataText = post?.metadata?.text;
  const text =
    typeof metadataText === "string" && metadataText.trim()
      ? metadataText
      : (post?.caption ?? "");
  const stated = post?.metadata?.publishedAt;

  async function save() {
    if (!post || saving) return;
    setSaving(true);
    setFailure(null);
    try {
      setSaved(await api.savePostText(post.id));
    } catch (raw) {
      setFailure(normalizeError(raw));
    } finally {
      setSaving(false);
    }
  }

  return (
    <Card
      title="Text post"
      description="This post is its words. There is no video or picture to download, so nothing here is queued or recorded as a download."
      action={
        <Button onClick={() => void save()} disabled={saving || !text.trim()}>
          {saving ? "Saving…" : "Save text"}
        </Button>
      }
    >
      {text.trim() ? (
        <p className="whitespace-pre-wrap break-words text-sm leading-relaxed text-content-primary">
          {text}
        </p>
      ) : (
        // An honest empty state: the source returned the post, and it holds no
        // words this application could read. Saying "no text" is different from
        // showing an empty box and letting the user wonder.
        <p className="text-xs text-content-muted">
          The source returned this post but no text could be read from it.
        </p>
      )}

      {typeof stated === "string" && stated && (
        <p className="mt-3 text-[11px] text-content-muted">
          Published {stated} <span className="text-content-muted">(as the source stated it)</span>
        </p>
      )}

      {saved && (
        <p className="mt-3 text-[11px] text-content-secondary">
          Saved to <span className="font-mono break-all">{saved.path}</span>
          {saved.renamed && " — an existing file was there, so this one was named beside it."}
        </p>
      )}

      {failure && (
        <div className="mt-3">
          <ErrorNotice
            error={failure}
            title={errorKindLabel(failure)}
            hint={recoveryHint(failure) ?? undefined}
            onDismiss={() => setFailure(null)}
          />
        </div>
      )}
    </Card>
  );
}

/**
 * The post's soundtrack as the source described it.
 *
 * Read defensively: this is metadata that arrived from a source, and a shape
 * that is not what was expected is treated as "the source said nothing" rather
 * than as a reason to fail a screen. `downloadable` is deliberately `=== true`:
 * anything else — absent, a string, a number — means this application has no
 * file to offer, and the safe reading of that is "no download".
 */
function describeSoundtrack(metadata: Metadata | undefined): {
  title: string | null;
  author: string | null;
  durationSeconds: number | null;
  downloadable: boolean;
} | null {
  const raw = metadata?.soundtrack;
  if (!raw || typeof raw !== "object") return null;
  const track = raw as Record<string, unknown>;
  const text = (value: unknown): string | null =>
    typeof value === "string" && value.trim() ? value : null;
  return {
    title: text(track.title),
    author: text(track.author),
    durationSeconds: typeof track.durationSeconds === "number" ? track.durationSeconds : null,
    downloadable: track.downloadable === true,
  };
}

/** What a picture is, in the terms the source actually gave. */
function describeImage(item: MediaItem): string {
  const format = (item.extension ?? "").toUpperCase();
  const size = item.width && item.height ? `${item.width}×${item.height}` : "";
  return [format, size].filter(Boolean).join(" · ") || "image";
}

/** The state one picture's own download is in. */
const IMAGE_STATE_LABELS: Record<string, string> = {
  queued: "Queued",
  downloading: "Downloading",
  paused: "Paused",
  completed: "Downloaded",
  failed: "Failed",
  cancelled: "Cancelled",
};

interface ImageGalleryProps {
  items: MediaItem[];
  selected: Set<Id>;
  onToggle: (id: Id) => void;
  onSelectAll: () => void;
  onClear: () => void;
  onDownload: () => void;
  onRetry: (taskId: Id) => void;
  downloading: boolean;
}

/**
 * The pictures of a post, in the order the source published them.
 *
 * A carousel is one thing to download, not fifteen: every slide starts chosen
 * and one action queues them all. Each picture keeps its own task, so a batch
 * that half-succeeds says so per slide and a failed one can be retried without
 * fetching its neighbours again.
 *
 * There is deliberately no per-slide progress bar. A row here shows a *state*,
 * and the state comes from the shared store by the media item its task names —
 * the same association the Downloads screen reads, so the two cannot tell
 * different stories about one slide.
 */
function ImageGallery({
  items,
  selected,
  onToggle,
  onSelectAll,
  onClear,
  onDownload,
  onRetry,
  downloading,
}: ImageGalleryProps) {
  // Gated on the store's *state* counter rather than on the task map, which
  // changes on every progress frame. Rebuilding this grid many times a second to
  // change nothing is exactly the cost that counter exists to avoid.
  const stateRevision = useDownloadStore((state) => state.stateRevision);
  const byItem = useMemo(() => {
    const { tasks } = useDownloadStore.getState();
    const map = new Map<Id, DownloadTask>();
    for (const id of Object.keys(tasks)) {
      const task = tasks[id];
      if (task.mediaItemId) map.set(task.mediaItemId, task);
    }
    return map;
  }, [stateRevision]);

  const allSelected = items.length > 0 && selected.size === items.length;

  return (
    <Card
      title="Available media"
      description={`${items.length} image${items.length === 1 ? "" : "s"} found in this post`}
      action={
        <div className="flex flex-wrap items-center gap-2">
          <Button size="sm" variant="ghost" onClick={allSelected ? onClear : onSelectAll}>
            {allSelected ? "Deselect all" : "Select all"}
          </Button>
          <Button
            variant="primary"
            size="sm"
            disabled={selected.size === 0}
            loading={downloading}
            onClick={onDownload}
          >
            {`Download selected (${selected.size})`}
          </Button>
        </div>
      }
    >
      <ul className="grid grid-cols-2 gap-3 sm:grid-cols-3">
        {items.map((item, index) => {
          const task = byItem.get(item.id);
          const label = `Image ${index + 1}`;
          return (
            <li key={item.id} className="rounded-lg border border-surface-border p-2">
              <label className="block cursor-pointer">
                <span className="flex items-center gap-2">
                  <input
                    type="checkbox"
                    checked={selected.has(item.id)}
                    onChange={() => onToggle(item.id)}
                    aria-label={`${label}, ${describeImage(item)}`}
                    className={FOCUS_RING}
                  />
                  <span className="text-xs text-content-secondary">{label}</span>
                </span>
                {item.thumbnailUrl || item.sourceUrl ? (
                  <img
                    src={item.thumbnailUrl ?? item.sourceUrl}
                    // The picture is the content here, so it is labelled rather
                    // than decorative. The source gives no caption to use, and
                    // "Image 3" is both true and useful.
                    alt={label}
                    loading="lazy"
                    className="mt-2 aspect-square w-full rounded object-cover"
                  />
                ) : null}
                <span className="mt-2 block text-[11px] text-content-muted">
                  {describeImage(item)}
                </span>
              </label>
              {task && (
                <span className="mt-1 flex items-center justify-between gap-2">
                  <Badge
                    tone={
                      task.state === "completed"
                        ? "success"
                        : task.state === "failed"
                          ? "danger"
                          : "brand"
                    }
                  >
                    {IMAGE_STATE_LABELS[task.state] ?? task.state}
                  </Badge>
                  {task.state === "failed" && (
                    <button
                      type="button"
                      onClick={() => onRetry(task.id)}
                      className={`rounded px-1.5 py-0.5 text-[11px] text-brand underline ${FOCUS_RING}`}
                    >
                      {`Retry ${label.toLowerCase()}`}
                    </button>
                  )}
                </span>
              )}
            </li>
          );
        })}
      </ul>
    </Card>
  );
}

function MediaCard({ analysis, selection, onSelect, onDownload, downloading }: MediaCardProps) {
  const selectedItem = analysis.media.find((item) => item.id === selection?.mediaItemId);
  const canDownload = selectedItem !== undefined;

  return (
    <Card
      title="Available media"
      description={`${analysis.media.length} downloadable item${analysis.media.length === 1 ? "" : "s"} in this post`}
      action={
        <Button
          variant="primary"
          size="sm"
          disabled={!canDownload}
          loading={downloading}
          onClick={onDownload}
        >
          Download
        </Button>
      }
    >
      <div className="space-y-3">
        {analysis.media.map((item) => (
          <MediaItemRow
            key={item.id}
            item={item}
            active={item.id === selection?.mediaItemId}
            selection={selection}
            onSelect={onSelect}
          />
        ))}
      </div>
    </Card>
  );
}

interface MediaItemRowProps {
  item: MediaItem;
  active: boolean;
  selection: Selection | null;
  onSelect: (selection: Selection) => void;
}

function MediaItemRow({ item, active, selection, onSelect }: MediaItemRowProps) {
  const hasVariants = item.variants.length > 0;

  return (
    <div
      className={`rounded-lg border p-3 transition-colors ${
        active ? "border-brand bg-brand/5" : "border-surface-border"
      }`}
    >
      <label className="flex cursor-pointer items-start gap-3">
        <input
          type="radio"
          name="media-item"
          checked={active}
          onChange={() => onSelect({ mediaItemId: item.id, variantId: preferredVariant(item)?.id ?? null })}
          className="mt-0.5 h-4 w-4 shrink-0 accent-brand"
        />
        <span className="min-w-0 flex-1">
          <span className="flex flex-wrap items-center gap-2">
            <span className="text-xs font-medium text-content-primary">
              {mediaTypeLabel(item.mediaType)}
            </span>
            {item.extension && <Badge mono>{item.extension}</Badge>}
            {item.width && item.height && (
              <span className="font-mono text-[11px] text-content-muted">
                {item.width}×{item.height}
              </span>
            )}
            {item.durationSeconds ? (
              <span className="text-[11px] text-content-muted">
                {item.durationSeconds.toFixed(1)}s
              </span>
            ) : null}
            {item.fileSize ? (
              <span className="text-[11px] text-content-muted">{formatBytes(item.fileSize)}</span>
            ) : null}
            <TrackLayoutBadge layout={mediaTrackLayout(item)} />
          </span>
        </span>
      </label>

      {active && hasVariants && (
        <div className="mt-3 space-y-1.5 border-t border-surface-border pl-7 pt-3">
          <p className="text-[11px] font-medium text-content-muted">Version</p>
          {logicalVariantGroups(item).map((group) => (
            <VariantRow
              key={group.representative.id}
              variant={group.representative}
              // Any mirror of this rendition counts as choosing it: the user
              // picked the rendition, not the host that will serve it.
              checked={group.members.some((member) => member.id === selection?.variantId)}
              onSelect={() =>
                onSelect({ mediaItemId: item.id, variantId: group.representative.id })
              }
            />
          ))}
        </div>
      )}
    </div>
  );
}

function VariantRow({
  variant,
  checked,
  onSelect,
}: {
  variant: MediaVariant;
  checked: boolean;
  onSelect: () => void;
}) {
  const watermarked = !variant.isOriginal && /watermark/i.test(variant.label);

  return (
    <label className="flex cursor-pointer items-center gap-3">
      <input
        type="radio"
        name="media-variant"
        checked={checked}
        onChange={onSelect}
        className="h-3.5 w-3.5 shrink-0 accent-brand"
      />
      <span className="flex min-w-0 flex-1 flex-wrap items-center gap-2">
        <span className="text-xs text-content-primary">{variant.label}</span>
        {variant.isOriginal && <Badge tone="success">clean source</Badge>}
        {watermarked && <Badge tone="warning">watermarked</Badge>}
        {variant.width && variant.height && (
          <span className="font-mono text-[11px] text-content-muted">
            {variant.width}×{variant.height}
          </span>
        )}
        {variant.fileSize ? (
          <span className="text-[11px] text-content-muted">{formatBytes(variant.fileSize)}</span>
        ) : null}
      </span>
    </label>
  );
}

interface DownloadCardProps {
  task: DownloadTask;
  onCancel: () => void;
  onRetry: () => void;
  onOpenFile: () => void;
  onOpenFolder: () => void;
}

function DownloadCard({ task, onCancel, onRetry, onOpenFile, onOpenFolder }: DownloadCardProps) {
  const active = isActive(task);
  const tone = task.state === "completed" ? "success" : task.state === "failed" ? "danger" : "active";

  return (
    <Card
      title="Download"
      description={task.fileName ?? task.sourceUrl}
      action={
        <Badge tone={STATUS_TONES[task.state]} mono>
          {humanize(task.state)}
        </Badge>
      }
    >
      <div className="space-y-2">
        <ProgressBar
          downloadedBytes={task.progress.downloadedBytes}
          totalBytes={task.progress.totalBytes}
          tone={tone}
        />
      </div>

      <div className="mt-3 space-y-3">
        <KeyValueList>
          <KeyValue label="Attempts">
            {task.attempts} / {task.maxAttempts}
          </KeyValue>
          {task.destinationPath && (
            <KeyValue label="Saved to" mono>
              {task.destinationPath}
            </KeyValue>
          )}
          {task.error && <KeyValue label="Error">{task.error}</KeyValue>}
        </KeyValueList>

        <div className="flex flex-wrap gap-2">
          {active && (
            <Button size="sm" onClick={onCancel}>
              Cancel
            </Button>
          )}
          {(task.state === "failed" || task.state === "cancelled") && (
            <Button size="sm" variant="primary" onClick={onRetry}>
              Retry
            </Button>
          )}
          {task.state === "completed" && (
            <>
              <Button size="sm" onClick={onOpenFile}>
                Open file
              </Button>
              <Button size="sm" onClick={onOpenFolder}>
                Open folder
              </Button>
            </>
          )}
        </div>
      </div>
    </Card>
  );
}
