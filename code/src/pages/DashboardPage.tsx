import { convertFileSrc } from "@tauri-apps/api/core";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  Badge,
  Button,
  Card,
  EmptyState,
  ErrorNotice,
  PageHeader,
} from "@/components/ui";
import { FOCUS_RING } from "@/lib/a11y";
import { Icon as IconObject } from "@/components/ui/icons";
import { Thumbnail } from "@/features/media/Thumbnail";
import { formatBytes, formatDateTime } from "@/lib/format";
import { api } from "@/services/api";
import { normalizeError, type BackendError } from "@/services/ipc";
import type { FolderContents, FolderListing, MediaFile, MediaFolder } from "@/types";

/**
 * The download folder, browsed directly.
 *
 * The Library answers "what did this application download", from the database.
 * This answers "what is on this disk", from the filesystem — so a file put there
 * by hand appears, and a database row whose file is gone does not pretend to be
 * something you can play. Nothing here writes, moves or deletes anything: it
 * looks, and it shows what it found.
 *
 * The scan happens when the screen is opened and when Refresh is pressed, never
 * on a render. Reading a large library is not something a re-render should be
 * able to trigger.
 */
export function DashboardPage() {
  const [listing, setListing] = useState<FolderListing | null>(null);
  const [contents, setContents] = useState<FolderContents | null>(null);
  /** The folders opened to get here, outermost first. Empty at the top level. */
  const [trail, setTrail] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<BackendError | null>(null);
  const [preview, setPreview] = useState<MediaFile | null>(null);

  const atTop = trail.length === 0;

  const loadFolders = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      setListing(await api.browseMediaFolders());
      setContents(null);
      setTrail([]);
    } catch (raw) {
      setError(normalizeError(raw));
    } finally {
      setBusy(false);
    }
  }, []);

  const openFolder = useCallback(async (path: string, append: boolean) => {
    setBusy(true);
    setError(null);
    try {
      const next = await api.browseMediaFolder(path);
      setContents(next);
      setTrail((current) => (append ? [...current, path] : [path]));
      setPreview(null);
    } catch (raw) {
      setError(normalizeError(raw));
      // The listing is cleared rather than left standing. The error says the
      // folder could not be read; leaving the previous folder's files on screen
      // underneath it would suggest they are what this folder holds.
      setContents(null);
      setTrail([]);
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    void loadFolders();
  }, [loadFolders]);

  /** Refresh whatever is on screen, at the level it is on. */
  const refresh = useCallback(() => {
    if (atTop) void loadFolders();
    else void openFolder(trail[trail.length - 1], false);
  }, [atTop, trail, loadFolders, openFolder]);

  const goTo = useCallback(
    (depth: number) => {
      if (depth === 0) void loadFolders();
      else void openFolder(trail[depth - 1], false);
    },
    [trail, loadFolders, openFolder],
  );

  return (
    <div className="mx-auto max-w-6xl px-6 py-6">
      <PageHeader
        title="Download Folder"
        description="What is actually in your download folder, by folder. Read from the disk, so a file you put there yourself shows up and one that has been deleted does not. For a searchable view of everything this application has recorded, use the Library."
      >
        <Button size="sm" onClick={refresh} loading={busy} disabled={busy}>
          Refresh
        </Button>
      </PageHeader>

      {listing && (
        <p className="mb-4 break-all text-xs text-content-muted">
          Looking in <span className="font-mono text-content-secondary">{listing.root}</span>
        </p>
      )}

      {error && (
        <div className="mb-4">
          <ErrorNotice error={error} onDismiss={() => setError(null)} />
        </div>
      )}

      <Breadcrumbs listing={listing} trail={trail} atTop={atTop} onGoTo={goTo} />

      {busy && !listing && !contents && (
        <p className="py-12 text-center text-sm text-content-muted">Reading the folder…</p>
      )}

      {atTop && listing && (
        <>
          {listing.folders.length === 0 && listing.looseMediaCount === 0 && (
            <EmptyState
              title="Nothing downloaded yet"
              description="This folder holds no media. Downloads go here once you use Single URL or Profile Downloader."
            />
          )}
          {listing.folders.length > 0 && (
            <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
              {listing.folders.map((folder) => (
                <FolderCard
                  key={folder.path}
                  folder={folder}
                  onOpen={() => void openFolder(folder.path, true)}
                />
              ))}
            </div>
          )}
          {listing.looseMediaCount > 0 && (
            <p className="mt-4 text-xs text-content-muted">
              {listing.looseMediaCount === 1
                ? "1 media file sits directly in the download folder, outside any folder."
                : `${listing.looseMediaCount} media files sit directly in the download folder, outside any folder.`}
            </p>
          )}
        </>
      )}

      {!atTop && contents && (
        <>
          {contents.folders.length > 0 && (
            <div className="mb-4 grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
              {contents.folders.map((folder) => (
                <FolderCard
                  key={folder.path}
                  folder={folder}
                  onOpen={() => void openFolder(folder.path, true)}
                />
              ))}
            </div>
          )}

          {contents.files.length === 0 && contents.folders.length === 0 && (
            <EmptyState
              title="This folder is empty"
              description="There is nothing in it and nothing below it."
            />
          )}

          {contents.files.length > 0 && (
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-5 xl:grid-cols-6">
              {contents.files.map((file) => (
                <FileCard key={file.path} file={file} onOpen={() => setPreview(file)} />
              ))}
            </div>
          )}

          {contents.truncated && (
            <p className="mt-4 text-xs text-content-muted">
              This folder holds more files than one listing shows, so some are not displayed.
            </p>
          )}
        </>
      )}

      {preview && <Preview key={preview.path} file={preview} onClose={() => setPreview(null)} />}
    </div>
  );
}

/** Where you are, and the way back to the top. */
function Breadcrumbs({
  listing,
  trail,
  atTop,
  onGoTo,
}: {
  listing: FolderListing | null;
  trail: string[];
  atTop: boolean;
  onGoTo: (depth: number) => void;
}) {
  const rootName = listing ? (listing.root.split(/[\\/]/).filter(Boolean).pop() ?? "Download") : "";
  const nameOf = (path: string) => path.split(/[\\/]/).filter(Boolean).pop() ?? path;

  return (
    <nav aria-label="Folder" className="mb-4 flex flex-wrap items-center gap-1.5 text-xs">
      <button
        type="button"
        onClick={() => onGoTo(0)}
        disabled={atTop}
        className={`rounded px-1.5 py-0.5 ${FOCUS_RING} ${
          atTop ? "font-medium text-content-primary" : "text-brand underline"
        }`}
      >
        {atTop ? (rootName || "Folders") : "All folders"}
      </button>
      {trail.map((path, index) => (
        <span key={path} className="flex items-center gap-1.5">
          <span aria-hidden="true" className="text-content-muted">
            /
          </span>
          <button
            type="button"
            onClick={() => onGoTo(index + 1)}
            disabled={index === trail.length - 1}
            className={`rounded px-1.5 py-0.5 ${FOCUS_RING} ${
              index === trail.length - 1
                ? "font-medium text-content-primary"
                : "text-brand underline"
            }`}
          >
            {nameOf(path)}
          </button>
        </span>
      ))}
    </nav>
  );
}

/** One folder, with what it holds and something to look at. */
function FolderCard({ folder, onOpen }: { folder: MediaFolder; onOpen: () => void }) {
  const counts = useMemo(() => {
    const parts: string[] = [];
    if (folder.imageCount > 0) parts.push(`${folder.imageCount} image${folder.imageCount === 1 ? "" : "s"}`);
    if (folder.videoCount > 0) parts.push(`${folder.videoCount} video${folder.videoCount === 1 ? "" : "s"}`);
    if (folder.audioCount > 0) parts.push(`${folder.audioCount} audio`);
    return parts.join(" · ");
  }, [folder]);

  return (
    <button
      type="button"
      onClick={onOpen}
      className={`flex items-start gap-3 rounded-xl border border-surface-border bg-surface-raised p-3 text-left transition-colors hover:border-brand ${FOCUS_RING}`}
    >
      {/*
        The folder's own face. A picture from inside it when there is one,
        otherwise a frame from the first video — taken once and cached, so the
        card costs one extraction the first time and nothing after that.
      */}
      <Thumbnail
        path={folder.previewPath}
        kind={folder.previewKind ?? "other"}
        className="h-16 w-16 shrink-0 rounded-lg border border-surface-border"
      >
        {folder.previewKind === "video" && <PlayBadge />}
      </Thumbnail>
      <span className="min-w-0 flex-1">
        {/*
          The full name travels on the title. Folders are named after creators,
          and a creator's name can be longer than the card — a truncated name
          with nothing to hover left the folder effectively unnamed, which is
          the same treatment the file cards below already give their file names.
        */}
        <span
          className="block truncate text-sm font-medium text-content-primary"
          title={folder.name}
        >
          {folder.name}
        </span>
        <span className="mt-1 block text-[11px] text-content-muted">
          {folder.mediaCount === 0
            ? folder.state === "unreadable"
              ? "Could not be read"
              : "No media in this folder"
            : `${folder.mediaCount} file${folder.mediaCount === 1 ? "" : "s"} · ${formatBytes(folder.totalBytes)}`}
        </span>
        {counts && <span className="mt-0.5 block text-[11px] text-content-muted">{counts}</span>}
      </span>
    </button>
  );
}

/** A play mark over a video's picture, so a still frame is not mistaken for one. */
function PlayBadge() {
  return (
    <span className="pointer-events-none absolute inset-0 flex items-center justify-center">
      <span
        aria-hidden="true"
        className="flex h-6 w-6 items-center justify-center rounded-full bg-black/55 pl-0.5 text-white"
      >
        <IconObject.video className="h-3.5 w-3.5" />
      </span>
    </span>
  );
}

/** One file on disk. */
function FileCard({ file, onOpen }: { file: MediaFile; onOpen: () => void }) {
  const meta = useMemo(() => {
    const parts = [formatBytes(file.sizeBytes)];
    if (file.modified) parts.push(formatDateTime(file.modified));
    return parts.join(" · ");
  }, [file]);

  // What the download called it, when something recorded it — a carousel frame
  // is a carousel frame, not an anonymous picture. Otherwise what the extension
  // says, which is all a folder listing can offer.
  const label = file.mediaType ?? file.kind;

  return (
    <div className="overflow-hidden rounded-xl border border-surface-border bg-surface-raised">
      <button
        type="button"
        onClick={onOpen}
        // The whole card, read aloud: what it is, what it is called, how big it
        // is. A grid of bare pictures says nothing to a screen reader.
        aria-label={`${file.name}, ${label}, ${meta}`}
        className={`block w-full ${FOCUS_RING}`}
      >
        <Thumbnail path={file.path} kind={file.kind} className="aspect-square w-full">
          {file.kind === "video" && <PlayBadge />}
        </Thumbnail>
      </button>
      <div className="p-2.5">
        <p className="truncate text-xs font-medium text-content-primary" title={file.name}>
          {file.name}
        </p>
        <p className="mt-1 flex items-center gap-1.5 text-[11px] text-content-muted">
          <Badge tone="neutral">{label}</Badge>
          <span className="truncate">{meta}</span>
        </p>
        {file.caption && (
          <p className="mt-1 line-clamp-2 text-[11px] leading-relaxed text-content-secondary">
            {file.caption}
          </p>
        )}
        {file.creator && (
          <p className="mt-0.5 truncate text-[11px] text-content-muted">{file.creator}</p>
        )}
      </div>
    </div>
  );
}

/**
 * Show one file.
 *
 * Images and video are loaded through Tauri's asset protocol, which is scoped to
 * the download folder — the window is not given the filesystem, it is given one
 * directory that the user configured. Nothing is copied, decoded or buffered by
 * this application for a video: the media element asks for the ranges it needs.
 *
 * A file this application cannot show is offered to the desktop instead of being
 * opened into a viewer that would fail without saying why.
 */
function Preview({ file, onClose }: { file: MediaFile; onClose: () => void }) {
  const closeRef = useRef<HTMLButtonElement>(null);
  const [failed, setFailed] = useState<string | null>(null);

  useEffect(() => {
    closeRef.current?.focus();
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const source = convertFileSrc(file.path);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={file.name}
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/70 p-6"
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <Card
        className="max-h-full w-full max-w-4xl overflow-auto"
        title={file.name}
        description={`${file.kind} · ${formatBytes(file.sizeBytes)}${
          file.creator ? ` · ${file.creator}` : ""
        }`}
        action={
          <div className="flex items-center gap-2">
            {/*
             * Offered for anything this window cannot show, and for anything it
             * tried to show and could not. A video whose codec this engine has
             * no decoder for fails at play time, not at classification time, so
             * deciding this from the file type alone would leave the user with
             * an error message and nothing to do about it.
             */}
            {(!file.playable || failed) && (
              <Button
                size="sm"
                onClick={() => void openOutside(file.path)}
              >
                Open externally
              </Button>
            )}
            <Button size="sm" onClick={onClose} ref={closeRef}>
              Close
            </Button>
          </div>
        }
      >
        {failed && (
          <p className="mb-3 rounded-lg border border-warning/30 bg-warning/10 px-3 py-2 text-xs text-warning">
            {failed}
          </p>
        )}

        {file.kind === "image" && (
          <img
            src={source}
            alt={file.caption ?? file.name}
            onError={() => setFailed("This picture could not be shown. The file may be damaged or in a format this window cannot display.")}
            className="mx-auto max-h-[70vh] w-auto max-w-full object-contain"
          />
        )}

        {file.kind === "video" && (
          <video
            src={source}
            controls
            autoPlay
            onError={() => setFailed("This video could not be played. The format may not be supported by this window.")}
            className="mx-auto max-h-[70vh] w-full bg-black"
          />
        )}

        {file.kind === "audio" && (
          <audio
            src={source}
            controls
            autoPlay
            onError={() => setFailed("This audio could not be played.")}
            className="w-full"
          />
        )}

        {file.kind === "other" && (
          <p className="text-sm text-content-secondary">
            This application does not show {file.extension ? `.${file.extension}` : "this kind of"}{" "}
            file in its own window. It can be opened with whatever the desktop uses for it.
          </p>
        )}
      </Card>
    </div>
  );
}

/** Hand a file to the desktop, by path — never by building a command line. */
async function openOutside(path: string) {
  // The same command the Downloads screen uses to reveal a finished file, so
  // there is one place that decides what may be opened.
  try {
    await api.openMediaFile(path);
  } catch {
    // Nothing to say: the file manager either opened or it did not, and the
    // media itself is unaffected.
  }
}
