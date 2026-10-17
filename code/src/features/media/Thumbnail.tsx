import { convertFileSrc } from "@tauri-apps/api/core";
import { useEffect, useRef, useState } from "react";

import { api } from "@/services/api";
import { Icon as IconObject } from "@/components/ui/icons";
import type { MediaKind } from "@/types";

/**
 * The preview pictures the Dashboard draws.
 *
 * Three things make this more than an `<img>` tag, and each of them is about not
 * doing work the window does not need to do.
 *
 * **Lazy.** A folder can hold two thousand files, and asking for two thousand
 * previews the moment it opens would spawn extraction work for all of them. A
 * card asks only once it is on screen.
 *
 * **Deduplicated.** Two cards for one file — a folder card and a file card, or
 * the same file reached twice — share one request. The backend caches the
 * picture; this stops it being asked for twice before the first answer arrives.
 *
 * **Bounded.** Four requests are in flight at once and the rest wait their turn.
 * Without a limit, scrolling a large folder opens a request per visible card and
 * the backend's own limit does the queueing far less evenly.
 *
 * A `null` answer is not a failure. It means there is no picture to be had — no
 * FFmpeg, a format that will not decode, audio, anything that is not a picture —
 * and the card shows a placeholder. A request that *does* fail is treated the
 * same way, because a broken picture is not a reason for the screen to fail.
 */

/** How many preview requests may be outstanding at once. */
const MAX_IN_FLIGHT = 4;

/** What is known about one file's preview. */
export interface Preview {
  /** The generated picture, or null when there is none to be had. */
  src: string | null;
  /**
   * True when the *file* could not be read at all — it moved, it was deleted,
   * the drive is gone. Distinct from `src: null`, which means the file was read
   * and simply has no picture (audio, an undecodable format, no FFmpeg).
   *
   * The two must not be shown alike: one says "nothing to show you here", the
   * other says "this file is not there any more", and only the second is
   * something the user needs to act on.
   */
  unreadable: boolean;
}

/** path -> what is known about its preview. */
const cache = new Map<string, Preview>();
/** path -> the request already running for it. */
const inFlight = new Map<string, Promise<Preview>>();

let active = 0;
const waiting: Array<() => void> = [];

function acquire(): Promise<void> {
  if (active < MAX_IN_FLIGHT) {
    active += 1;
    return Promise.resolve();
  }
  return new Promise((resolve) => {
    waiting.push(() => {
      active += 1;
      resolve();
    });
  });
}

function release(): void {
  active -= 1;
  waiting.shift()?.();
}

/** The preview for one file, asked for once however many cards want it. */
export function thumbnailSource(path: string): Promise<Preview> {
  const known = cache.get(path);
  if (known) return Promise.resolve(known);

  const running = inFlight.get(path);
  if (running) return running;

  const request = (async (): Promise<Preview> => {
    await acquire();
    try {
      const found = await api.mediaThumbnail(path);
      const preview: Preview = { src: found, unreadable: false };
      cache.set(path, preview);
      return preview;
    } catch {
      // The backend refused the path: it is not inside the download folder any
      // more, or not a readable file. Recorded so the card does not ask again on
      // every render.
      const preview: Preview = { src: null, unreadable: true };
      cache.set(path, preview);
      return preview;
    } finally {
      inFlight.delete(path);
      release();
    }
  })();

  inFlight.set(path, request);
  return request;
}

/** Forget everything. Only for tests, which must not inherit each other's state. */
export function resetThumbnailCache(): void {
  cache.clear();
  inFlight.clear();
}

/** Whether a card is on screen, so it can wait until it is. */
function useOnScreen(ref: React.RefObject<HTMLElement | null>): boolean {
  const [seen, setSeen] = useState(false);

  useEffect(() => {
    if (seen) return;
    const element = ref.current;
    // A webview without the observer — or a test — shows everything at once,
    // which is the behaviour a card would have had before it was made lazy.
    if (!element || typeof IntersectionObserver === "undefined") {
      setSeen(true);
      return;
    }
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) setSeen(true);
      },
      // A screenful ahead, so a card is usually ready before it is scrolled to.
      { rootMargin: "200px" },
    );
    observer.observe(element);
    return () => observer.disconnect();
  }, [ref, seen]);

  return seen;
}

/** The icon for a file kind, used where a unicode glyph used to sit. */
function KindIcon({ kind }: { kind: MediaKind }) {
  const Component =
    kind === "video"
      ? IconObject.video
      : kind === "audio"
        ? IconObject.audio
        : kind === "image"
          ? IconObject.image
          : IconObject.external;
  return <Component className="h-6 w-6" />;
}

export interface ThumbnailProps {
  /** The media file to preview, or nothing for a card that has none. */
  path?: string;
  kind: MediaKind;
  /** Class for the square the picture sits in. */
  className?: string;
  /** Shown only to assistive technology; the card carries the label. */
  alt?: string;
  /** Drawn over the picture — a play badge, a state chip. */
  children?: React.ReactNode;
}

/**
 * One card's picture: the preview, or an honest placeholder.
 *
 * The placeholder is not decorative. A row of grey squares with no distinction
 * would be worse than the file names it replaced, so each kind gets its own
 * glyph and a video says it is a video even when no frame could be taken.
 */
export function Thumbnail({ path, kind, className = "", alt = "", children }: ThumbnailProps) {
  const ref = useRef<HTMLSpanElement>(null);
  const onScreen = useOnScreen(ref);
  const [preview, setPreview] = useState<Preview | null>(() =>
    path ? cache.get(path) ?? null : null,
  );
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    setFailed(false);
    if (!path) {
      setPreview(null);
      return;
    }
    const known = cache.get(path);
    if (known) {
      setPreview(known);
      return;
    }
    if (!onScreen) return;
    let live = true;
    void thumbnailSource(path).then((found) => {
      if (live) setPreview(found);
    });
    return () => {
      live = false;
    };
  }, [path, onScreen]);

  const src = preview?.src ?? null;
  const unreadable = preview?.unreadable ?? false;

  return (
    <span
      ref={ref}
      className={`relative flex items-center justify-center overflow-hidden bg-surface-overlay ${className}`}
    >
      {src && !failed ? (
        <img
          src={convertFileSrc(src)}
          alt={alt}
          loading="lazy"
          // A picture that will not render leaves the placeholder underneath,
          // rather than a broken-image icon in the middle of the grid.
          onError={() => setFailed(true)}
          className="h-full w-full object-cover"
        />
      ) : (
        <span className="select-none text-content-muted" aria-hidden="true">
          <KindIcon kind={kind} />
        </span>
      )}
      {unreadable && (
        <span className="absolute inset-x-0 bottom-0 flex items-center justify-center gap-1 bg-warning/85 px-1 py-0.5 text-center text-[9px] font-medium text-black">
          <IconObject.warning className="h-2.5 w-2.5" />
          Unavailable
        </span>
      )}
      {children}
    </span>
  );
}
