import { useCallback, useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { Link } from "react-router-dom";

import {
  Badge,
  Button,
  buttonClasses,
  Card,
  EmptyState,
  ErrorNotice,
  Field,
  KeyValue,
  KeyValueList,
  PageHeader,
  Select,
  TextInput,
  type BadgeTone,
} from "@/components/ui";
import { FOCUS_RING } from "@/lib/a11y";
import { loadFull, type LoadFullProgress } from "@/features/profile/loadFull";
import { LoginPanel } from "@/features/profile/LoginPanel";
import { useLoginStore } from "@/services/login";
import { isTerminal, useDownloadStore } from "@/stores/useDownloadStore";
import { errorKindLabel, recoveryHint } from "@/lib/errors";
import { formatDateTime } from "@/lib/format";
import { api } from "@/services/api";
import { normalizeError, type BackendError } from "@/services/ipc";
import type {
  AppErrorKind,
  AnalyzedUrl,
  ContentStatus,
  DownloadTask,
  Id,
  MediaItem,
  MediaType,
  ProfileDownloadResult,
  ProfilePostResult,
  ProfileIdentity,
  ProfilePostView,
  SavedText,
} from "@/types";
import { mediaTypeLabel } from "@/types";

/**
 * The creator's posts, browsed a page at a time.
 *
 * All of the Douyin knowledge lives behind the three profile commands — this
 * screen renders what the resolver reported and never learns how a profile is
 * paged, how a post is resolved, or where a media URL comes from.
 *
 * Two things it is careful about, because they are easy to get wrong:
 *
 * * **A selection is keyed by post id, not by position.** Filtering and paging
 *   therefore cannot silently move a tick onto a different post.
 * * **A stale response is discarded.** Switching profiles while a request is in
 *   flight must not pour the previous creator's posts onto the new screen.
 */

/**
 * Seconds since it appeared.
 *
 * Mounted only while a read is running, so it starts at nought and its own
 * lifetime is the measurement — no start time has to be passed down, and
 * unmounting to clean up costs nothing.
 */
function Elapsed() {
  const [seconds, setSeconds] = useState(0);
  useEffect(() => {
    const timer = window.setInterval(() => setSeconds((value) => value + 1), 1000);
    return () => window.clearInterval(timer);
  }, []);
  return <span className="text-[11px] tabular-nums text-content-muted">{seconds}s</span>;
}

/**
 * What a post turned out to be, once it was actually resolved.
 *
 * A profile listing states that a post exists and roughly what kind it is; it
 * cannot say whether a note is a set of pictures or a caption with nothing but
 * words, because its card shows the same shape for both. So the *resolved* type
 * is what the screen reports, and this is where it is kept.
 *
 * `pictures` and `text` are not alternatives: a post with pictures keeps its
 * caption here too, and neither is invented for the other.
 */
interface ResolvedPost {
  /** The type the resolver settled on from what it actually found. */
  kind: MediaType;
  /** Its pictures, in the source's order. Empty when it has none. */
  pictures: MediaItem[];
  /** Its words, as the source stated them. `null` when it stated none. */
  text: string | null;
  /** The publish time exactly as the page printed it, never converted. */
  publishedAt: string | null;
}

/**
 * Read a resolved post out of what the resolver returned.
 *
 * `listed` is the type the profile listing claimed, and it is the fallback when
 * the resolution stated none. Falling back to `video` instead would be this
 * screen inventing a type — and it would be the one type that turns a picture
 * post into "this downloads as itself, with nothing to choose inside it".
 */
function describeResolvedPost(analysed: AnalyzedUrl, listed: MediaType): ResolvedPost {
  const post = analysed.post;
  const metadata = post?.metadata ?? {};
  const stated = metadata.text;
  const text =
    typeof stated === "string" && stated.trim() ? stated : (post?.caption ?? null);
  return {
    kind: post?.mediaType ?? listed,
    pictures: analysed.media.filter((item) => item.mediaType === "image"),
    text: text && text.trim() ? text : null,
    publishedAt:
      typeof metadata.publishedAt === "string" ? metadata.publishedAt : null,
  };
}

/**
 * Why a post's pictures are not on screen.
 *
 * `none` is the source's answer — it was reached, and it holds no pictures.
 * `signedOut` and `unavailable` and `failed` are all ways of *not* having asked
 * successfully, and each calls for different words: sign in, try later, or a
 * read that went wrong. Collapsing them into one "no images" would tell the user
 * the source had answered when it had not.
 */
interface GalleryProblem {
  kind: "none" | "signedOut" | "unavailable" | "failed";
  message: string | null;
}

/**
 * A read that finished after the profile it was asked for left the screen.
 *
 * Not an error and never shown: it exists so the answer can be discarded at the
 * one place answers are handled, rather than at each of them.
 */
class StaleRead extends Error {}

/** Which of those a backend error means. */
function problemKind(kind: AppErrorKind): GalleryProblem["kind"] {
  switch (kind) {
    case "authentication_required":
      return "signedOut";
    case "unavailable_content":
    case "browser_unavailable":
    case "not_found":
      return "unavailable";
    default:
      return "failed";
  }
}

/**
 * What to say when a post's pictures are not there.
 *
 * The distinction the screen has to keep: the source answering "nothing here" is
 * a fact about the post, and the source not being reached is not. Saying the
 * first when the second is true is how a user concludes a post has no pictures
 * when it may well have several.
 */
function galleryProblemText(problem: GalleryProblem): string {
  switch (problem.kind) {
    case "none":
      return "The source returned this post and it holds no pictures.";
    case "signedOut":
      return "Reading this post's pictures needs a signed-in session. Sign in above, then try again.";
    case "unavailable":
      return "The source could not be read just now. This is about the request, not about the post.";
    case "failed":
      return problem.message ?? "This post's pictures could not be read.";
  }
}

/** Whether the listing currently *shows* a post. */
const TYPE_FILTERS: { value: "all" | MediaType; label: string }[] = [  { value: "all", label: "All types" },
  { value: "video", label: "Video" },
  { value: "carousel", label: "Image posts" },
  { value: "image", label: "Image" },
  { value: "audio", label: "Audio" },
  { value: "text", label: "Text" },
];

const STATUS_TONES: Record<ContentStatus, BadgeTone> = {
  new: "neutral",
  ready: "brand",
  downloading: "accent",
  downloaded: "success",
  failed: "danger",
  unavailable: "warning",
  private: "warning",
  unsupported: "warning",
};

const STATUS_LABELS: Record<ContentStatus, string> = {
  new: "New",
  ready: "Ready",
  downloading: "Downloading",
  downloaded: "Downloaded",
  failed: "Failed",
  unavailable: "Unavailable",
  private: "Private",
  unsupported: "Unsupported",
};

/** Statuses a submission will act on. The rest are passed over, and said so. */
const SUBMITTABLE: ContentStatus[] = ["new", "ready", "failed"];

export function ProfileDownloaderPage() {
  const [url, setUrl] = useState("");
  const [identity, setIdentity] = useState<ProfileIdentity | null>(null);
  const [posts, setPosts] = useState<ProfilePostView[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [totalKnown, setTotalKnown] = useState<number | null>(null);
  const [selection, setSelection] = useState<Set<string>>(() => new Set());
  const [typeFilter, setTypeFilter] = useState<"all" | MediaType>("all");
  const [busy, setBusy] = useState<"resolve" | "page" | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [result, setResult] = useState<ProfileDownloadResult | null>(null);
  const [error, setError] = useState<BackendError | null>(null);
  /**
   * The pictures of a post the user has opened, keyed by post.
   *
   * The listing says a post is a carousel without saying what is in it, so a
   * post's media is read when its section is opened rather than for every row of
   * every page — fifteen renders to answer a question nobody asked.
   */
  const [slidesByPost, setSlidesByPost] = useState<Map<string, MediaItem[]>>(() => new Map());
  /** Which of a post's pictures the user kept. Absent means "all of them". */
  const [chosenSlides, setChosenSlides] = useState<Map<string, Set<string>>>(() => new Map());
  /**
   * The posts whose pictures are being read right now.
   *
   * A set rather than one id: reading a post is a browser render that takes as
   * long as it takes, and the rest of the screen — other posts, the page, the
   * submit button — stays usable while it runs. One id would have made the whole
   * screen "the post being read".
   */
  const [readingSlides, setReadingSlides] = useState<Set<string>>(() => new Set());
  /**
   * Why a post's pictures are not on screen, when they are not.
   *
   * Five different things, and the screen must not say one of them when it means
   * another: the source answered that there are none, the source wants a
   * sign-in, the source could not be reached, the read failed, or the post
   * itself is not a kind that has pictures. "Nothing found" is an answer;
   * "could not ask" is not, and a screen that shows them the same way teaches
   * the user to distrust both.
   */
  const [galleryProblem, setGalleryProblem] = useState<Map<string, GalleryProblem>>(() => new Map());
  /** Whether a post's pictures came from the copy already on file. */
  const [slidesOrigin, setSlidesOrigin] = useState<Map<string, "cache" | "source">>(() => new Map());
  /** What a post turned out to be, once it was resolved. */
  const [resolvedByPost, setResolvedByPost] = useState<Map<string, ResolvedPost>>(() => new Map());
  /** A text post whose words are being written out, and what came of it. */
  const [savingText, setSavingText] = useState<string | null>(null);
  const [savedText, setSavedText] = useState<Map<string, SavedText>>(() => new Map());
  const [textProblem, setTextProblem] = useState<Map<string, string>>(() => new Map());

  /**
   * Reading a post's pictures, once.
   *
   * Two maps and a set, held in refs because they are about work in flight
   * rather than about what is on screen. `inFlight` is what makes opening and
   * closing a chooser four times cost one render instead of four; `resolved`
   * keeps a finished read even when the view that asked for it was cancelled, so
   * reopening shows the pictures instead of paying for them twice.
   */
  const slidesByPostRef = useRef<Map<string, MediaItem[]>>(new Map());
  const postsResolved = useRef<Map<string, ResolvedPost>>(new Map());
  const slidesInFlight = useRef<Map<string, Promise<ResolvedPost>>>(new Map());
  const slidesCancelled = useRef<Set<string>>(new Set());

  /** The profile whose posts are on screen. Responses for another are dropped. */
  const profileInView = useRef<string | null>(null);
  /** Guards against a second page request being sent while one is in flight. */
  const pageInFlight = useRef(false);
  /** The state of a "Load full" walk, while one is running or after it ends. */
  const [run, setRun] = useState<LoadFullProgress | null>(null);
  /** Set by Stop; read between pages by the walk. */
  const stopFull = useRef(false);

  /*
   * The field starts empty, and nothing is fetched until the user asks.
   *
   * It used to arrive holding one of the project's own test profile links,
   * fetched from the backend on mount — so a fresh visit to this screen looked
   * at a stranger's profile before the user had done anything. The placeholder
   * says what to paste instead.
   */

  // A download that finishes while the profile is open moves its post's status,
  // so the row shows what actually happened rather than what was true when the
  // listing was fetched. The status comes from the shared task store — the same
  // row the Downloads screen is reading — instead of a second copy folded out of
  // the event stream here, which is how two screens came to disagree.
  //
  // Driven by the store's *state* counter rather than by every event: a post's
  // row shows a status, not a progress bar, and re-rendering the listing for
  // every progress frame would be work done to change nothing.
  const stateRevision = useDownloadStore((state) => state.stateRevision);
  useEffect(() => {
    if (stateRevision === 0) return;
    const { tasks } = useDownloadStore.getState();
    // The association comes from the task itself, not from this screen's memory
    // of what it started. A ref of "tasks I began" can only speak for those: a
    // download started from another screen, or one whose entry was lost when the
    // profile was re-analysed, would never move its row — while the Downloads
    // screen, reading the task's own `postId`, showed it finished.
    const byPost = new Map<string, DownloadTask>();
    for (const task of Object.values(tasks)) {
      if (!task.postId) continue;
      const seen = byPost.get(task.postId);
      byPost.set(task.postId, seen ? newerTask(seen, task) : task);
    }

    setPosts((current) =>
      current.map((view) => {
        const task = byPost.get(view.post.id);
        return task ? applyTaskState(view, task) : view;
      }),
    );
  }, [stateRevision]);

  const loadPage = useCallback(
    async (profileUrl: string, nextCursor: string | null, append: boolean) => {
      // One page request at a time: a double click on "Load more" must not send
      // the same page twice.
      if (pageInFlight.current) return;
      pageInFlight.current = true;
      setBusy("page");
      setError(null);
      try {
        const page = await api.listProfilePosts(profileUrl, nextCursor);
        // A response for a profile that is no longer on screen is discarded.
        if (profileInView.current !== profileUrl) return;

        setPosts((current) => (append ? mergePosts(current, page.posts) : page.posts));
        setCursor(page.nextCursor);
        setHasMore(page.hasMore);
        setTotalKnown(page.totalKnown);
      } catch (raw) {
        if (profileInView.current !== profileUrl) return;
        const failure = normalizeError(raw);
        // A failed page leaves everything already loaded exactly as it was.
        setError(failure);
        // The source asking for a sign-in is the one failure here the user can
        // do something about, so the sign-in section is told about it and says
        // so rather than leaving them to work it out from the message.
        if (failure.kind === "authentication_required") {
          useLoginStore.getState().markRequiredBySource();
        }
      } finally {
        pageInFlight.current = false;
        setBusy(null);
      }
    },
    [],
  );

  /**
   * Load a profile the user supplied.
   *
   * Its only caller is the form's submit handler, so this runs when — and only
   * when — the user has asked for it with a link in the box.
   */
  const analyze = useCallback(
    async (candidate: string) => {
      const trimmed = candidate.trim();
      if (trimmed === "" || busy === "resolve") return;

      // Everything belonging to the previous profile goes, so its posts and
      // selections cannot be read as the new creator's.
      profileInView.current = trimmed;
      setBusy("resolve");
      setError(null);
      setIdentity(null);
      setPosts([]);
      setCursor(null);
      setHasMore(false);
      setTotalKnown(null);
      setSelection(new Set());
      setResult(null);
      setSlidesByPost(new Map());
      setChosenSlides(new Map());
      setReadingSlides(new Set());
      setGalleryProblem(new Map());
      setSlidesOrigin(new Map());
      slidesByPostRef.current = new Map();
      postsResolved.current = new Map();
      slidesInFlight.current = new Map();
      slidesCancelled.current = new Set();
      setResolvedByPost(new Map());
      setSavedText(new Map());
      setTextProblem(new Map());
      // A previous walk belonged to the previous profile; its counts and its
      // note would read as if they described this one.
      setRun(null);
      stopFull.current = true;

      try {
        const resolved = await api.resolveProfile(trimmed);
        if (profileInView.current !== trimmed) return;
        setIdentity(resolved);
      } catch (raw) {
        if (profileInView.current !== trimmed) return;
        setError(normalizeError(raw));
        setBusy(null);
        return;
      }

      setBusy(null);
      await loadPage(trimmed, null, false);
    },
    [busy, loadPage],
  );

  const loadMore = useCallback(() => {
    const profileUrl = profileInView.current;
    if (!profileUrl || !hasMore || busy !== null) return;
    void loadPage(profileUrl, cursor, true);
  }, [busy, cursor, hasMore, loadPage]);

  /**
   * Walk the listing to its end.
   *
   * The loop itself lives in `features/profile/loadFull`, where every way it can
   * end is tested against a source that does exactly that; this is the part that
   * knows about this screen — which profile is in view, what is already loaded,
   * and where the results go.
   */
  const runningFull = run?.status === "running";

  const loadEverything = useCallback(async () => {
    const profileUrl = profileInView.current;
    // One walk at a time. A second click while one is running would fetch the
    // same pages twice and merge them into the same list.
    if (!profileUrl || runningFull) return;

    stopFull.current = false;
    setError(null);

    const result = await loadFull(
      { cursor, hasMore, known: posts, totalKnown },
      {
        fetchPage: (next: string | null) => api.listProfilePosts(profileUrl, next),
        onProgress: setRun,
        // The profile changing under a running walk stops it: its pages belong
        // to a listing the user is no longer looking at.
        shouldStop: () => stopFull.current || profileInView.current !== profileUrl,
        wait: (milliseconds) =>
          new Promise((resolve) => window.setTimeout(resolve, milliseconds)),
      },
    );

    // A run for a profile that is no longer on screen contributes nothing.
    if (profileInView.current !== profileUrl) return;

    setPosts(result.posts);
    setCursor(result.cursor);
    setHasMore(result.hasMore);
  }, [cursor, hasMore, posts, runningFull, totalKnown]);

  const stopLoadingEverything = useCallback(() => {
    // Cooperative: the page in flight finishes and is merged — it is real data
    // and discarding it would throw away work the source already did — but no
    // further page is asked for.
    stopFull.current = true;
  }, []);

  const visible = useMemo(
    () => (typeFilter === "all" ? posts : posts.filter((view) => view.post.mediaType === typeFilter)),
    [posts, typeFilter],
  );

  const setMembership = useCallback((ids: string[], member: boolean) => {
    setSelection((current) => {
      const next = new Set(current);
      for (const id of ids) {
        if (member) next.add(id);
        else next.delete(id);
      }
      return next;
    });
  }, []);

  const toggle = useCallback((id: string) => {
    setSelection((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const selectedPosts = useMemo(
    () => posts.filter((view) => selection.has(view.post.id)),
    [posts, selection],
  );
  /**
   * Whether the user has narrowed this post's pictures.
   *
   * A post whose pictures were partly downloaded reads as `downloaded`, and the
   * gate below would refuse it — so the other pictures could never be asked for,
   * however clearly the chooser showed them. An explicit, non-default choice is
   * the user saying which ones, and the backend skips whatever is already there,
   * so honouring it cannot re-fetch a picture.
   */
  const isNarrowed = useCallback(
    (postId: string) => {
      const chosen = chosenSlides.get(postId);
      if (!chosen) return false;
      return chosen.size !== (slidesByPost.get(postId)?.length ?? 0);
    },
    [chosenSlides, slidesByPost],
  );

  const submitReady = useMemo(
    () =>
      selectedPosts.filter(
        (view) =>
          // A post this screen has already read and found to be words has
          // nothing to download, so it is not counted as ready to submit. It is
          // not a failure and its row stays exactly where it is — it simply is
          // not one of the posts "Download selected" is offering to download.
          resolvedByPost.get(view.post.id)?.kind !== "text" &&
          (SUBMITTABLE.includes(view.status) || isNarrowed(view.post.id)),
      ),
    [selectedPosts, isNarrowed, resolvedByPost],
  );

  /**
   * The creator resolved but the source declined to list their posts.
   *
   * This is a distinct outcome from a creator with nothing published, and the
   * screen keeps the two apart: only the backend's `unavailable_content` — the
   * error it raises when it asked for a listing and was refused — produces it.
   * A creator who genuinely has no posts comes back as a successful empty
   * listing, with no error at all.
   */
  // A creator that resolved but whose posts could not be listed. Two kinds: the
  // source withheld the listing, and the source wants a signed-in visitor. Both
  // belong here rather than in the generic error slot, because both are
  // statements about this creator's listing rather than about the request.
  const listingUnavailable =
    identity !== null &&
    posts.length === 0 &&
    (error?.kind === "unavailable_content" || error?.kind === "authentication_required");

  /** Put what a post turned out to be on screen. */
  const installResolved = useCallback(
    (postId: string, resolved: ResolvedPost, origin: "cache" | "source") => {
      // What was on screen a moment ago. Read from a ref rather than from state
      // because both the pictures and the choice are being replaced in one go,
      // and the choice is decided from the pictures that used to be there.
      const previousPictures = slidesByPostRef.current.get(postId) ?? [];
      slidesByPostRef.current = new Map(slidesByPostRef.current).set(postId, resolved.pictures);

      setResolvedByPost((current) => new Map(current).set(postId, resolved));
      setSlidesByPost((current) => new Map(current).set(postId, resolved.pictures));
      setChosenSlides((current) => {
        const next = new Map(current);
        const known = new Set(previousPictures.map((picture) => picture.id));
        const present = new Set(resolved.pictures.map((picture) => picture.id));
        // A choice already made is kept — narrowed to the pictures that are
        // still there — because re-reading a post is how a chooser is refreshed,
        // and dropping a careful selection just because the pictures were
        // confirmed would punish the user for asking.
        const chosen = new Set(
          [...(current.get(postId) ?? [])].filter((id) => present.has(id)),
        );
        // Everything else is chosen to begin with: narrowing is the exception,
        // and making it the default would be a screen that does nothing until it
        // is touched. A picture that was not here before is new, whatever the
        // user decided about the others.
        for (const picture of resolved.pictures) {
          if (!known.has(picture.id)) chosen.add(picture.id);
        }
        next.set(postId, chosen);
        return next;
      });
      setSlidesOrigin((current) => new Map(current).set(postId, origin));
      setGalleryProblem((current) => {
        const next = new Map(current);
        next.delete(postId);
        return next;
      });
    },
    [],
  );

  /**
   * Read a post's pictures, or show the ones already known.
   *
   * Opening the chooser used to mean a fresh browser render every time, which is
   * ninety seconds of waiting to see something the application often already
   * had: the listing carries each post's stored media, and a post that was
   * resolved once keeps its pictures. So there are three ways this ends, and
   * only the last one costs anything —
   *
   * 1. the pictures are already on screen: nothing happens at all;
   * 2. the post on file already has image items: they are shown immediately and
   *    no browser is started;
   * 3. neither, so the source is asked — once, however many times the chooser is
   *    opened, because the work itself is shared rather than restarted.
   */
  const resolvePost = useCallback(
    async (post: ProfilePostView["post"], force = false) => {
      if (!force && slidesByPost.has(post.id)) return;

      const remembered = force ? undefined : postsResolved.current.get(post.id);
      if (remembered) {
        installResolved(post.id, remembered, "source");
        return;
      }

      // What the listing already knew. A post resolved on an earlier run keeps
      // its media items, and those are enough to choose from — the download
      // re-resolves the URL it actually fetches, so a signed URL that has since
      // expired costs nothing here. Skipped when the user asked for a refresh,
      // which is precisely the request "show me what the source has now".
      //
      // Only pictures are read from here. A stored post says nothing about
      // whether it has *words*, and a caption is not a text post — so the copy
      // on file answers questions about pictures and is not allowed to answer
      // the one about what kind of post this is.
      const stored = force
        ? []
        : post.mediaItems.filter((item) => item.mediaType === "image");
      if (stored.length > 0) {
        postsResolved.current.set(post.id, {
          kind: post.mediaType,
          pictures: stored,
          text: null,
          publishedAt: null,
        });
        installResolved(
          post.id,
          { kind: post.mediaType, pictures: stored, text: null, publishedAt: null },
          "cache",
        );
        return;
      }

      slidesCancelled.current.delete(post.id);
      setReadingSlides((current) => new Set(current).add(post.id));
      setGalleryProblem((current) => {
        const next = new Map(current);
        next.delete(post.id);
        return next;
      });

      let work = slidesInFlight.current.get(post.id);
      if (!work) {
        // The profile on screen when the read started. A resolve can outlive the
        // screen it was asked for — the user switches creators while a browser is
        // still working — and its answer belongs to the profile that asked.
        const asked = profileInView.current;
        work = (async () => {
          const analysed = await api.analyzeUrl(post.canonicalUrl ?? post.url);
          if (profileInView.current !== asked) {
            throw new StaleRead();
          }
          return describeResolvedPost(analysed, post.mediaType);
        })();
        slidesInFlight.current.set(post.id, work);
      }

      try {
        const resolved = await work;
        // Kept whether or not the view that asked is still interested, so a
        // cancelled read is not paid for a second time.
        postsResolved.current.set(post.id, resolved);
        if (slidesCancelled.current.has(post.id)) return;
        // Every type is installed, not only the picture kinds: a post that
        // turned out to be a video, a text post, or nothing this application can
        // fetch still has a type worth reporting, and the row has to say which.
        installResolved(post.id, resolved, "source");
      } catch (raw) {
        // A read that finished for a profile no longer on screen is dropped
        // rather than reported: it is not a failure of this one.
        if (raw instanceof StaleRead) return;
        if (slidesCancelled.current.has(post.id)) return;
        const failure = normalizeError(raw);
        setGalleryProblem((current) =>
          new Map(current).set(post.id, {
            kind: problemKind(failure.kind),
            message: failure.message,
          }),
        );
      } finally {
        setReadingSlides((current) => {
          const next = new Set(current);
          next.delete(post.id);
          return next;
        });
        slidesInFlight.current.delete(post.id);
      }
    },
    [slidesByPost, installResolved],
  );

  /**
   * Stop waiting for a post's pictures, without throwing away what was found.
   *
   * Cancelling is about the wait, not about the pictures: a read that has
   * already produced something keeps it, and a read that is still running keeps
   * running — it is the same browser render either way, and abandoning the
   * request while the browser works would leave it running where nobody could
   * see it. When it lands it is stored, so reopening is instant.
   */
  const cancelSlides = useCallback((postId: string) => {
    slidesCancelled.current.add(postId);
    setReadingSlides((current) => {
      const next = new Set(current);
      next.delete(postId);
      return next;
    });
  }, []);

  /** Ask the source again for a post whose pictures are not on file. */
  const rereadSlides = useCallback(
    (post: ProfilePostView["post"]) => {
      postsResolved.current.delete(post.id);
      slidesInFlight.current.delete(post.id);
      slidesCancelled.current.delete(post.id);
      setSlidesByPost((current) => {
        const next = new Map(current);
        next.delete(post.id);
        return next;
      });
      setResolvedByPost((current) => {
        const next = new Map(current);
        next.delete(post.id);
        return next;
      });
      setTextProblem((current) => {
        const next = new Map(current);
        next.delete(post.id);
        return next;
      });
      // Forced: the copy on file and the answer already given are both exactly
      // what the user has just asked to look past.
      void resolvePost(post, true);
    },
    [resolvePost],
  );

  const toggleSlide = useCallback((postId: string, mediaItemId: string) => {
    setChosenSlides((current) => {
      const next = new Map(current);
      const chosen = new Set(next.get(postId) ?? []);
      if (chosen.has(mediaItemId)) chosen.delete(mediaItemId);
      else chosen.add(mediaItemId);
      next.set(postId, chosen);
      return next;
    });
  }, []);

  /**
   * Write a post's words out.
   *
   * Deliberately not a download: it goes through the text export command, which
   * writes a `.txt` and records nothing in the queue or the history. A caption
   * appearing in the download history as a "completed download" would be a
   * record of a transfer that never happened.
   */
  const saveText = useCallback(async (post: ProfilePostView["post"]) => {
    setSavingText(post.id);
    setTextProblem((current) => {
      const next = new Map(current);
      next.delete(post.id);
      return next;
    });
    try {
      const saved = await api.savePostText(post.id);
      setSavedText((current) => new Map(current).set(post.id, saved));
    } catch (raw) {
      const failure = normalizeError(raw);
      setTextProblem((current) => new Map(current).set(post.id, failure.message));
    } finally {
      setSavingText(null);
    }
  }, []);

  const submit = useCallback(async () => {
    if (submitReady.length === 0 || submitting) return;
    setSubmitting(true);
    setError(null);
    try {
      // Only the posts the user actually narrowed are described. A post whose
      // pictures were never opened keeps the screen's default — every picture it
      // has — so the backend is not told what it already knows.
      const narrowed: Record<string, string[]> = {};
      for (const view of selectedPosts) {
        // Including "none of them": a post the user emptied is a post they asked
        // to leave alone, and saying so is better than quietly downloading all
        // of it.
        if (isNarrowed(view.post.id)) {
          narrowed[view.post.id] = [...(chosenSlides.get(view.post.id) ?? [])];
        }
      }

      const outcome = await api.submitProfileDownloads({
        postIds: selectedPosts.map((view) => view.post.id),
        ...(Object.keys(narrowed).length > 0 ? { selectedItems: narrowed } : {}),
      });
      setResult(outcome);
      // Applied straight away so the rows answer the click, and reconciled
      // against the task store from then on: the backend's per-post outcome is
      // the persisted truth for posts that produced no task at all, and the
      // task itself carries the link to the post for those that did.
      setPosts((current) =>
        current.map((view) => {
          const entry = outcome.posts.find((o) => o.postId === view.post.id);
          return entry ? applyPostResult(view, entry) : view;
        }),
      );
    } catch (raw) {
      setError(normalizeError(raw));
    } finally {
      setSubmitting(false);
    }
  }, [selectedPosts, chosenSlides, isNarrowed, submitReady.length, submitting]);

  const visibleSelected = visible.filter((view) => selection.has(view.post.id)).length;
  const newCount = posts.filter((view) => view.status === "new").length;
  const failedCount = posts.filter((view) => view.status === "failed").length;
  const filterLabel = TYPE_FILTERS.find((option) => option.value === typeFilter)?.label ?? "All types";

  return (
    <>
      <PageHeader
        title="Profile Downloader"
        description="Paste a public Douyin profile link to browse a creator's posts and download the ones you want."
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
              label="Douyin profile link"
              hint="A creator profile link, or a short link that expands to one."
              error={error?.kind === "validation" ? error.message : undefined}
            >
              <TextInput
                value={url}
                onChange={(event) => setUrl(event.target.value)}
                placeholder="Paste a Douyin profile link"
                spellCheck={false}
                autoComplete="off"
              />
            </Field>

            <div className="flex flex-wrap items-center gap-2">
              <Button
                type="submit"
                variant="primary"
                loading={busy === "resolve"}
                disabled={runningFull}
              >
                Load profile
              </Button>
              {busy === "page" && <Badge mono tone="brand">loading posts</Badge>}
              {identity && (
                <Badge mono tone={identity.resolutionStage === "resolved" ? "success" : "neutral"}>
                  {identity.resolutionStage === "resolved" ? "creator loaded" : "identity only"}
                </Badge>
              )}
            </div>

            {/* Says what the wait is and where it comes from. Reading Douyin
                needs a real browser, the app keeps one for the whole session
                rather than opening one per action, and a user who is told that
                is not left wondering why a page takes a few seconds. */}
            <p className="text-xs leading-relaxed text-content-muted">
              {busy === "resolve"
                ? "Identifying the creator…"
                : busy === "page"
                  ? "Loading posts…"
                  : submitting
                    ? "Creating download tasks…"
                    : null}
              {busy !== null || submitting ? (
                <>
                  {" "}
                  Douyin pages are read with a browser the app manages, which can take a few
                  seconds per page.
                </>
              ) : (
                <>
                  Douyin pages are read with a browser the app manages. It is kept off-screen and
                  reused for the whole session, so one listing does not mean a window per page.
                </>
              )}
            </p>
          </form>
        </Card>

        <LoginPanel />

        {/* A creator that resolved but whose posts could not be listed is its
            own outcome, not a generic error and emphatically not "no posts".
            It gets one panel that says both things, beside the creator the
            screen has already identified. */}
        {listingUnavailable && (
          <ListingUnavailable
            identity={identity}
            message={error?.message ?? ""}
            busy={busy !== null}
            onRetry={() => void loadPage(profileInView.current ?? "", null, false)}
          />
        )}

        {/* Suppressed while that panel is showing: it already carries the same
            message, and two copies of one failure reads as two failures. */}
        {!listingUnavailable && (
          <ErrorNotice
            error={error}
            onDismiss={() => setError(null)}
            title={error ? errorKindLabel(error) : undefined}
            hint={error ? (recoveryHint(error) ?? undefined) : undefined}
          />
        )}

        {identity && <CreatorHeader identity={identity} />}

        {identity && !listingUnavailable && (
          <Card
            title="Posts"
            description={
              totalKnown === null
                ? `${posts.length} loaded`
                : posts.length > totalKnown
                  ? // The page's own grid served more cards than the page's own
                    // label states. Showing "54 loaded of 50" states a
                    // contradiction without saying so; this names it instead.
                    `${posts.length} loaded — more than the ${totalKnown} the creator's profile states`
                  : `${posts.length} loaded of ${totalKnown} the creator has published`
            }
            action={
              <div className="flex flex-wrap items-center gap-2">
                <Select
                  value={typeFilter}
                  options={TYPE_FILTERS}
                  onValueChange={(value) => setTypeFilter(value as "all" | MediaType)}
                  className="w-40"
                />
                <Button size="sm" onClick={() => setMembership(visible.map((v) => v.post.id), true)} disabled={visible.length === 0}>
                  Select {filterLabel.toLowerCase()} ({visible.length})
                </Button>
                <Button size="sm" onClick={() => setMembership(posts.filter((v) => v.status === "new").map((v) => v.post.id), true)} disabled={newCount === 0}>
                  Select new ({newCount})
                </Button>
                <Button size="sm" onClick={() => setMembership(posts.filter((v) => v.status === "failed").map((v) => v.post.id), true)} disabled={failedCount === 0}>
                  Select failed ({failedCount})
                </Button>
                <Button size="sm" onClick={() => setSelection(new Set())} disabled={selection.size === 0}>
                  Deselect all
                </Button>
              </div>
            }
          >
            <div className="space-y-4">
              <SelectionSummary
                posts={posts}
                selected={selection.size}
                visibleSelected={visibleSelected}
                ready={submitReady.length}
                loaded={posts.length}
                loading={busy === "page" && posts.length > 0}
                submitting={submitting}
                onSubmit={() => void submit()}
              />

              {posts.length === 0 && busy !== "page" && (
                <EmptyState
                  title="This creator has no public posts"
                  description="The source returned a listing for this profile and it was empty. That is a statement the source made, not one made on its behalf."
                />
              )}

              {posts.length > 0 && visible.length === 0 && (
                <p className="rounded-lg border border-surface-border px-3 py-6 text-center text-xs text-content-muted">
                  No loaded post is of type “{filterLabel.toLowerCase()}”. Change the filter, or load
                  more posts.
                </p>
              )}

              <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
                {visible.map((view) => (
                  <PostCard
                    key={view.post.id}
                    view={view}
                    selected={selection.has(view.post.id)}
                    onToggle={() => toggle(view.post.id)}
                    slides={slidesByPost.get(view.post.id) ?? null}
                    chosen={chosenSlides.get(view.post.id) ?? new Set()}
                    readingSlides={readingSlides.has(view.post.id)}
                    fromCache={slidesOrigin.get(view.post.id) === "cache"}
                    problem={galleryProblem.get(view.post.id) ?? null}
                    resolved={resolvedByPost.get(view.post.id) ?? null}
                    savingText={savingText === view.post.id}
                    savedText={savedText.get(view.post.id) ?? null}
                    textProblem={textProblem.get(view.post.id) ?? null}
                    onShowSlides={() => void resolvePost(view.post)}
                    onCancelSlides={() => cancelSlides(view.post.id)}
                    onRereadSlides={() => rereadSlides(view.post)}
                    onSaveText={() => void saveText(view.post)}
                    onToggleSlide={(mediaItemId) => toggleSlide(view.post.id, mediaItemId)}
                  />
                ))}
              </div>

              <div className="flex flex-wrap items-center justify-between gap-3 border-t border-surface-border pt-3">
                <span className="text-xs text-content-muted">
                  {hasMore
                    ? "More posts are available."
                    : "That is the whole listing the source returned."}
                </span>
                <div className="flex items-center gap-2">
                  {/* A failed page keeps everything already loaded, so retrying
                      it resumes the listing rather than restarting it. */}
                  {error && (
                    <Button
                      size="sm"
                      onClick={() =>
                        void loadPage(profileInView.current ?? "", cursor, posts.length > 0)
                      }
                      disabled={busy !== null}
                    >
                      Retry
                    </Button>
                  )}
                  <Button size="sm" onClick={loadMore} loading={busy === "page"} disabled={!hasMore || busy !== null || runningFull}>
                    Load more
                  </Button>
                  {runningFull ? (
                    <Button size="sm" variant="secondary" onClick={stopLoadingEverything}>
                      Stop
                    </Button>
                  ) : (
                    <Button
                      size="sm"
                      variant="secondary"
                      onClick={() => void loadEverything()}
                      disabled={!hasMore || busy !== null}
                    >
                      Load full
                    </Button>
                  )}
                  <Button
                    size="sm"
                    variant="ghost"
                    onClick={() => {
                      // Refresh means one thing: start this profile's listing
                      // again from the first page. Pagination is reset rather
                      // than resumed, because a cursor from a listing that has
                      // since changed asks for a page that no longer means what
                      // it meant, and mixing the two produces a list that is
                      // neither.
                      //
                      // What it must *not* do is throw away what has already
                      // been read. Re-reading the listing is how a user gets
                      // past a source that stopped early — and since an early
                      // stop clears `hasMore`, which disables Load full, this
                      // was the only way forward they had. Charging them the
                      // posts they already had for it is how "Refresh then Load
                      // full" came to look like the fix. Merging by post id
                      // makes that same sequence cost nothing, and lose nothing.
                      setRun(null);
                      stopFull.current = true;
                      void loadPage(profileInView.current ?? "", null, posts.length > 0);
                    }}
                    disabled={busy !== null || runningFull}
                  >
                    Refresh
                  </Button>
                </div>
              </div>

              {run && <LoadFullStatus progress={run} />}
            </div>
          </Card>
        )}

        {result && <SubmissionResult result={result} />}

        {!identity && busy === null && !error && (
          <EmptyState
            title="Paste a profile link to begin"
            description="The creator's public posts are listed a page at a time, so you can pick the ones you want rather than downloading everything."
          />
        )}
      </div>
    </>
  );
}

/** Fold a new page into the ones already loaded, without repeating a post. */
function mergePosts(current: ProfilePostView[], incoming: ProfilePostView[]): ProfilePostView[] {
  const byId = new Map(current.map((view) => [view.post.id, view]));
  // The incoming page wins on a collision, so a refreshed status replaces a
  // stale one — while the post keeps its position and its selection.
  for (const view of incoming) byId.set(view.post.id, view);
  return [...byId.values()];
}

/**
 * Apply a submission result to the post row it belongs to.
 *
 * The row speaks for the *post*, so it follows the post's own aggregate rather
 * than whichever of its pictures happened to be handled last. What that means
 * for a carousel is the point: fifteen pictures queued is "downloading", nine
 * queued and one refused is "downloading" with a note saying so, and the row
 * only reads as downloaded once every one of its tasks has finished — which the
 * task store decides, not this function.
 */
function applyPostResult(view: ProfilePostView, post: ProfilePostResult): ProfilePostView {
  const first = post.media[0];
  const refused = post.media.filter((entry) => entry.outcome === "failed").length;

  switch (post.outcome) {
    case "completed":
      return { ...view, status: "downloaded", downloadState: "completed", note: null };
    case "queued":
      return {
        ...view,
        status: "downloading",
        downloadState: "queued",
        activeTaskId: first?.taskId ?? null,
        // A single item keeps the backend's own message — it is the one that
        // says a post was re-fetched because its file had gone missing.
        note: post.media.length > 1 ? null : (first?.message ?? null),
      };
    case "partial":
      return {
        ...view,
        status: "downloading",
        note: `${refused} of ${post.media.length} could not be queued.`,
      };
    case "unsupported":
      return { ...view, status: "unsupported", note: first?.message ?? null };
    default:
      return { ...view, status: "failed", note: first?.message ?? null };
  }
}

/** Fold a task's authoritative state into the row for the post it belongs to. */
/**
 * Which of two tasks for one post decides that post's row.
 *
 * Work still going outranks work that has ended: a post with one finished
 * download and a second one now running is being downloaded, not done — which
 * is the whole point of saying so per post rather than per file. Between two
 * tasks that have both ended, the later one is what happened, so a retry that
 * failed after a download succeeded does not leave the row claiming the older
 * success.
 */
function newerTask(a: DownloadTask, b: DownloadTask): DownloadTask {
  const aLive = !isTerminal(a.state);
  const bLive = !isTerminal(b.state);
  if (aLive !== bLive) return bLive ? b : a;
  // A task whose stored time could not be read is not evidence that it is the
  // newer one: `null` compares as neither earlier nor later, so the task that
  // *does* have a readable time wins, and two unreadable ones keep the order
  // they arrived in. Reading a missing time as "now" — which is what a bare
  // comparison against a Date would do — would make a corrupt row look like the
  // most recent thing that ever happened.
  if (a.updatedAt === null) return b.updatedAt === null ? a : b;
  if (b.updatedAt === null) return a;
  return b.updatedAt >= a.updatedAt ? b : a;
}

function applyTaskState(view: ProfilePostView, task: DownloadTask): ProfilePostView {
  switch (task.state) {
    case "completed":
      return { ...view, status: "downloaded", downloadState: "completed", note: null };
    case "failed":
      return { ...view, status: "failed", downloadState: "failed", note: task.error };
    case "cancelled":
      // Not a failure to show against the post: it is back to being something
      // that has not been downloaded, which is what the user asked for.
      return { ...view, status: "new", downloadState: "cancelled", note: null };
    default:
      return { ...view, status: "downloading", downloadState: task.state };
  }
}

/** What a "Load full" walk has done, while it runs and after it ends. */
function LoadFullStatus({ progress }: { progress: LoadFullProgress }) {
  const running = progress.status === "running";
  const tone: BadgeTone =
    progress.status === "running"
      ? "brand"
      : progress.status === "complete"
        ? "success"
        : progress.status === "failed"
          ? "danger"
          : "warning";

  // One label per state, spelled out. A fallback arm is how "the source
  // stopped returning pages" came to be announced as "stopped early" — which
  // reads as something the user did, and it was not.
  const labels: Record<LoadFullProgress["status"], string> = {
    running: "Loading",
    complete: "Complete",
    failed: "Failed",
    stopped: "Stopped",
    withheld: "Source stopped early",
  };
  const label = labels[progress.status];

  // A fraction only when the source stated a count. Without one there is no
  // denominator, and a bar that invented one would be a number the user could
  // believe and should not.
  const known = progress.totalKnown;
  const percent = known && known > 0 ? Math.min(100, Math.round((progress.posts / known) * 100)) : null;

  return (
    <div
      role="status"
      // Named for what it is doing *now*. A fixed "Loading the full profile"
      // went on announcing itself as loading after the walk had finished, which
      // is the one thing a status region must not do.
      aria-label={`Full profile load: ${label}`}
      className="mt-3 rounded-lg border border-surface-border bg-surface-raised px-3 py-2"
    >
      <div className="flex flex-wrap items-center gap-3">
        <Badge tone={tone}>{label}</Badge>
        <span className="text-xs text-content-secondary">
          {progress.pages} {progress.pages === 1 ? "page" : "pages"} ·{" "}
          {progress.posts} {progress.posts === 1 ? "post" : "posts"} · {progress.added} newly added
          {known != null && (
            <> {progress.posts > known ? "against" : "of"} {known} the profile reports</>
          )}
        </span>
      </div>

      <div
        role="progressbar"
        aria-label="Posts retrieved"
        aria-valuemin={0}
        aria-valuemax={percent === null ? undefined : 100}
        aria-valuenow={percent ?? undefined}
        aria-valuetext={
          percent === null
            ? `${progress.posts} posts retrieved, the profile reports no total`
            : progress.posts > (known ?? 0)
              ? `${progress.posts} posts retrieved, more than the ${known} the profile reports`
              : `${progress.posts} of ${known} posts`
        }
        className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-surface-overlay"
      >
        {percent === null ? (
          // Indeterminate: the source never said how many posts there are, so
          // the bar shows that something is happening and the count beside it
          // says how much. A stripe that pretended to be 40% would be a lie the
          // user cannot check.
          <div
            className={`h-full w-1/3 rounded-full bg-brand ${running ? "animate-[pulse_1.6s_ease-in-out_infinite]" : ""}`}
          />
        ) : (
          <div className="h-full rounded-full bg-brand" style={{ width: `${percent}%` }} />
        )}
      </div>

      {progress.note && (
        <p className="mt-2 text-xs leading-relaxed text-content-muted">{progress.note}</p>
      )}
    </div>
  );
}

function CreatorHeader({ identity }: { identity: ProfileIdentity }) {
  const { creator, notice, resolvedVia } = identity;
  const [avatarFailed, setAvatarFailed] = useState(false);

  useEffect(() => setAvatarFailed(false), [creator.avatarUrl]);

  return (
    <Card
      title="Creator"
      description="As the profile page reported"
      action={
        <div className="flex items-center gap-2">
          {creator.verified && <Badge tone="brand">verified</Badge>}
          {/* Says why loading this profile took a few seconds, which is the
              only thing about the resolver the user needs to know. */}
          {resolvedVia === "browser" && <Badge tone="neutral">read via browser</Badge>}
        </div>
      }
    >
      <div className="flex flex-wrap items-start gap-5">
        {creator.avatarUrl && !avatarFailed ? (
          <img
            src={creator.avatarUrl}
            alt=""
            onError={() => setAvatarFailed(true)}
            className="h-20 w-20 shrink-0 rounded-full border border-surface-border object-cover"
          />
        ) : (
          <div className="flex h-20 w-20 shrink-0 items-center justify-center rounded-full border border-surface-border bg-surface-overlay text-xs text-content-muted">
            No avatar
          </div>
        )}

        <div className="min-w-0 flex-1">
          <p className="text-base font-medium text-content-primary">{creator.displayName}</p>
          {creator.handle && (
            <p className="mt-0.5 font-mono text-xs text-content-muted">{creator.handle}</p>
          )}

          <div className="mt-3">
            <KeyValueList>
              <KeyValue label="Creator id" mono>
                {creator.platformId}
              </KeyValue>
              {/* The count is only shown when the profile actually published
                  one; an unknown count is not rendered as zero. */}
              <KeyValue label="Posts published">
                {creator.postCount === null ? "not reported" : creator.postCount}
              </KeyValue>
              <KeyValue label="Profile URL" mono>
                {creator.profileUrl}
              </KeyValue>
            </KeyValueList>
          </div>

          {creator.description && (
            <p className="mt-3 border-t border-surface-border pt-3 text-xs leading-relaxed text-content-secondary">
              {creator.description}
            </p>
          )}

          {notice && (
            <p className="mt-3 rounded-lg border border-warning/30 bg-warning/5 px-3 py-2 text-xs text-content-secondary">
              {notice}
            </p>
          )}
        </div>
      </div>
    </Card>
  );
}

/**
 * The creator was identified, but their posts could not be retrieved.
 *
 * Shown in place of the post list, never alongside an empty one, so the screen
 * cannot be read as "this creator has nothing". Everything the screen already
 * learned about the creator stays on the page above, and the two ways forward
 * are stated plainly: try again, or work with a single post link instead.
 */
function ListingUnavailable({
  identity,
  message,
  busy,
  onRetry,
}: {
  identity: ProfileIdentity;
  message: string;
  busy: boolean;
  onRetry: () => void;
}) {
  const declared = identity.creator.postCount;

  return (
    <Card
      title="Posts could not be retrieved"
      description="The creator was identified; their post list was not provided"
      action={
        <Badge tone="warning" mono>
          listing unavailable
        </Badge>
      }
    >
      <div className="space-y-4">
        <p className="text-xs leading-relaxed text-content-secondary">{message}</p>

        <div className="rounded-lg border border-surface-border bg-surface-overlay px-3 py-2.5">
          <p className="text-xs leading-relaxed text-content-secondary">
            This is <span className="font-medium text-content-primary">not</span> the same as a
            creator with no posts.
            {declared !== null && declared > 0 && (
              <>
                {" "}
                The profile itself states{" "}
                <span className="font-medium text-content-primary">{declared}</span> published
                {declared === 1 ? " post" : " posts"}.
              </>
            )}{" "}
            Nothing was assumed on the creator's behalf, and no list was invented to fill the gap.
          </p>
        </div>

        <div className="flex flex-wrap items-center gap-2">
          <Button size="sm" onClick={onRetry} loading={busy} disabled={busy}>
            Try again
          </Button>
          {/* A retry is offered because the source's behaviour can differ
              between sessions, but it is not presented as likely to help. */}
          <span className="text-[11px] text-content-muted">
            The source refused this time; it may refuse again.
          </span>
        </div>

        <div className="border-t border-surface-border pt-4">
          <p className="text-xs leading-relaxed text-content-secondary">
            Individual posts are unaffected: a post link still resolves and downloads exactly as
            it does today. That is a way to work with one post, not a replacement for browsing
            this creator's posts.
          </p>
          <Link to="/single" className={buttonClasses("secondary", "sm", "mt-3")}>
            Open the Single URL screen
          </Link>
        </div>
      </div>
    </Card>
  );
}

interface SelectionSummaryProps {
  posts: ProfilePostView[];
  selected: number;
  visibleSelected: number;
  ready: number;
  loaded: number;
  loading: boolean;
  submitting: boolean;
  onSubmit: () => void;
}

function SelectionSummary({
  selected,
  visibleSelected,
  ready,
  loaded,
  loading,
  submitting,
  onSubmit,
}: SelectionSummaryProps) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-surface-border bg-surface-overlay px-3 py-2.5">
      <div className="text-xs text-content-secondary">
        <span className="font-medium text-content-primary">{selected}</span> selected
        {visibleSelected !== selected && (
          <>
            {" "}
            (<span className="font-medium text-content-primary">{visibleSelected}</span> of them visible)
          </>
        )}
        {" · "}
        <span className="font-medium text-content-primary">{ready}</span> will be submitted
        {" · "}
        <span className="text-content-muted">{loaded} loaded</span>
        {loading && <span className="ml-2 text-content-muted">loading…</span>}
      </div>

      <Button
        variant="primary"
        size="sm"
        loading={submitting}
        disabled={ready === 0 || submitting}
        onClick={onSubmit}
        title={
          ready === 0
            ? "Select at least one post that can be downloaded"
            : "Queue the selected posts for download"
        }
      >
        Download selected
      </Button>
    </div>
  );
}

function PostCard({
  view,
  selected,
  onToggle,
  slides,
  chosen,
  readingSlides,
  fromCache,
  problem,
  resolved,
  savingText,
  savedText,
  textProblem,
  onShowSlides,
  onCancelSlides,
  onRereadSlides,
  onSaveText,
  onToggleSlide,
}: {
  view: ProfilePostView;
  selected: boolean;
  onToggle: () => void;
  /** The post's pictures once they have been read, or `null` until asked for. */
  slides: MediaItem[] | null;
  chosen: Set<Id>;
  readingSlides: boolean;
  /** Whether these pictures came from the copy already on file. */
  fromCache: boolean;
  /** Why there are none, when there are none. */
  problem: GalleryProblem | null;
  /** What the post turned out to be, once it was resolved. */
  resolved: ResolvedPost | null;
  savingText: boolean;
  savedText: SavedText | null;
  textProblem: string | null;
  onShowSlides: () => void;
  onCancelSlides: () => void;
  onRereadSlides: () => void;
  onSaveText: () => void;
  onToggleSlide: (mediaItemId: Id) => void;
}) {
  const { post, status, note } = view;
  const [failed, setFailed] = useState(false);
  const [open, setOpen] = useState(false);
  useEffect(() => setFailed(false), [post.thumbnailUrl]);

  const duration = listingDuration(post.metadata);
  // The listing's own idea of the post, which is a claim about a card rather
  // than about the content: Douyin draws a video card and a note card, and a
  // note card looks the same whether it holds ten pictures or one sentence.
  const listedAsImages = post.mediaType === "image" || post.mediaType === "carousel";
  // What the post actually is, once it has been resolved. Until then the row
  // shows the listing's claim and says so by offering to look closer.
  const kind = resolved?.kind ?? post.mediaType;
  const isText = kind === "text";
  // Pictures to choose from: the resolved ones, or the listing's cached items.
  const pictures = slides ?? [];
  const hasPictures = kind === "image" || kind === "carousel";
  const shownBy = resolved ? mediaTypeLabel(kind) : mediaTypeLabel(post.mediaType);
  const sourceLabel = listedAsImages ? "Choose images" : "Open post";

  return (
    <div
      className={`flex gap-3 rounded-lg border p-3 transition-colors ${
        selected ? "border-brand bg-brand/5" : "border-surface-border"
      }`}
    >
      <input
        type="checkbox"
        checked={selected}
        onChange={onToggle}
        aria-label={post.caption ?? post.platformPostId}
        className="mt-0.5 h-4 w-4 shrink-0 accent-brand"
      />

      <div className="w-20 shrink-0">
        {post.thumbnailUrl && !failed ? (
          <img
            src={post.thumbnailUrl}
            alt=""
            onError={() => setFailed(true)}
            className="aspect-[9/16] w-full rounded-md border border-surface-border object-cover"
          />
        ) : (
          <div className="flex aspect-[9/16] w-full items-center justify-center rounded-md border border-surface-border bg-surface-overlay text-[10px] text-content-muted">
            No preview
          </div>
        )}
      </div>

      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-1.5">
          {/* The resolved type once there is one. A listing's claim is a claim
              about a card, and a note card cannot say whether it is ten pictures
              or one sentence — so the row stops repeating the guess as soon as
              the post has actually been read. */}
          <Badge tone={resolved && kind !== post.mediaType ? "brand" : "neutral"}>
            {shownBy}
          </Badge>
          <Badge tone={STATUS_TONES[status]}>{STATUS_LABELS[status]}</Badge>
        </div>

        <p className="mt-1.5 line-clamp-2 text-xs leading-relaxed text-content-primary">
          {post.caption ?? <span className="text-content-muted">No caption</span>}
        </p>

        <p className="mt-1.5 text-[11px] text-content-muted">
          {formatDateTime(post.publishedAt)}
          {duration !== null && <> · {duration.toFixed(1)}s</>}
          {" · "}
          <span className="font-mono">{post.platformPostId}</span>
        </p>

        {note && (
          <p className="mt-1.5 text-[11px] leading-relaxed text-content-muted">{note}</p>
        )}

        {/* Always offered. A listing cannot say what a note post holds, so the
            way to find out has to be available on the row rather than only on
            the ones the listing happened to guess right about. */}
        <div className="mt-2">
          <div className="flex flex-wrap items-center gap-2">
            <button
              type="button"
              onClick={() => {
                const next = !open;
                setOpen(next);
                if (next && resolved === null && !readingSlides) onShowSlides();
              }}
              className={`rounded px-1.5 py-0.5 text-[11px] text-brand underline ${FOCUS_RING}`}
            >
              {open ? (hasPictures ? "Hide images" : "Hide") : sourceLabel}
            </button>
            {/* A controlled refresh, for the one case the copy on file is not
                enough: the user wants what the source has now. Never started
                on its own — a browser render is the expensive thing this
                section exists to avoid. */}
            {open && resolved !== null && (
              <button
                type="button"
                onClick={onRereadSlides}
                className={`rounded px-1.5 py-0.5 text-[11px] text-content-muted underline ${FOCUS_RING}`}
              >
                Refresh from source
              </button>
            )}
          </div>

          {open && readingSlides && (
            <div className="mt-1.5 flex items-center gap-2">
              <p className="text-[11px] text-content-muted">
                Reading this post from the page. It can take a minute.
              </p>
              {/* How long this has been running. The wait really is tens of
                  seconds, and a count that keeps moving is the difference
                  between "working" and "stuck" for someone watching it. */}
              <Elapsed />
              <button
                type="button"
                onClick={() => {
                  // Cancelling dismisses the section as well as the wait. The
                  // read itself carries on — it is one browser render either
                  // way — so reopening shows the answer instead of paying for
                  // it again.
                  setOpen(false);
                  onCancelSlides();
                }}
                className={`rounded px-1.5 py-0.5 text-[11px] text-content-muted underline ${FOCUS_RING}`}
              >
                Cancel
              </button>
            </div>
          )}

          {open && !readingSlides && resolved === null && problem !== null && (
            <div className="mt-1.5">
              <p className="text-[11px] text-content-muted">{galleryProblemText(problem)}</p>
              {/* Only where asking again could plausibly answer differently. */}
              {(problem.kind === "unavailable" || problem.kind === "failed") && (
                <button
                  type="button"
                  onClick={onRereadSlides}
                  className={`mt-1 rounded px-1.5 py-0.5 text-[11px] text-brand underline ${FOCUS_RING}`}
                >
                  Try again
                </button>
              )}
            </div>
          )}

          {/* A post whose content is its words. It is not a failed download and
              not an empty gallery: the source published text, and the way to
              keep it is to write it out. Saving is deliberately not a download —
              no task, no queue entry, no history row. */}
          {open && !readingSlides && isText && resolved !== null && (
            <div className="mt-1.5">
              {resolved.text ? (
                <p className="whitespace-pre-wrap break-words text-xs leading-relaxed text-content-primary">
                  {resolved.text}
                </p>
              ) : (
                <p className="text-[11px] text-content-muted">
                  The source returned this post but no text could be read from it.
                </p>
              )}
              {resolved.publishedAt && (
                <p className="mt-1.5 text-[11px] text-content-muted">
                  Published {resolved.publishedAt}{" "}
                  <span className="text-content-muted">(as the source stated it)</span>
                </p>
              )}
              <div className="mt-2 flex flex-wrap items-center gap-2">
                <Button
                  size="sm"
                  onClick={onSaveText}
                  disabled={savingText || !resolved.text}
                >
                  {savingText ? "Saving…" : "Save text"}
                </Button>
                {savedText && (
                  <span className="text-[11px] text-content-secondary">
                    Saved to <span className="font-mono break-all">{savedText.path}</span>
                    {savedText.renamed && " — an existing file was there, so this one was named beside it."}
                  </span>
                )}
              </div>
              {textProblem && (
                <p className="mt-1.5 text-[11px] text-danger">{textProblem}</p>
              )}
            </div>
          )}

          {/* A post that really is a video or a track. Saying which is the point:
              it is not offered a text export and not offered a picture chooser,
              and the row stops repeating the listing's guess. */}
          {open && !readingSlides && resolved !== null && !isText && !hasPictures && (
            <p className="mt-1.5 text-[11px] text-content-muted">
              The source returned this post as {mediaTypeLabel(kind).toLowerCase()}. It
              downloads as itself, with nothing to choose inside it.
            </p>
          )}

          {open && !readingSlides && hasPictures && pictures.length === 0 && resolved !== null && (
            <p className="mt-1.5 text-[11px] text-content-muted">
              The source returned this post and it holds no pictures.
            </p>
          )}

          {open && hasPictures && pictures.length > 0 && (
            <>
              <p className="mt-1.5 text-[11px] text-content-muted">
                {`${chosen.size} of ${pictures.length} selected`}
                {fromCache && " · from the copy already on file"}
              </p>
              <ul className="mt-1.5 grid grid-cols-4 gap-1.5">
                  {pictures.map((item, index) => (
                    <li key={item.id}>
                      <label className="block cursor-pointer">
                        <span className="flex items-center gap-1">
                          <input
                            type="checkbox"
                            checked={chosen.has(item.id)}
                            onChange={() => onToggleSlide(item.id)}
                            aria-label={`Image ${index + 1}`}
                            className={FOCUS_RING}
                          />
                          <span className="text-[10px] text-content-muted">{index + 1}</span>
                        </span>
                        <img
                          src={item.thumbnailUrl ?? item.sourceUrl}
                          alt=""
                          loading="lazy"
                          className="mt-1 aspect-square w-full rounded border border-surface-border object-cover"
                        />
                      </label>
                    </li>
                  ))}
                </ul>
              </>
            )}
          </div>
      </div>
    </div>
  );
}

/** The duration a listing reported for a post, if it reported one. */
function listingDuration(metadata: Record<string, unknown>): number | null {
  const value = metadata?.listing_duration_seconds;
  return typeof value === "number" && Number.isFinite(value) && value > 0 ? value : null;
}

function SubmissionResult({ result }: { result: ProfileDownloadResult }) {
  // One post can contribute many items — a carousel is fifteen downloads — so
  // the lists a person reads are made of *items*, each still attributed to the
  // post it came from. A failure that named the post alone would leave the user
  // to work out which of its pictures it meant.
  const items = result.posts.flatMap((post) => post.media.map((entry) => ({ post, entry })));
  const failures = items.filter(({ entry }) => entry.outcome === "failed");
  const skipped = items.filter(
    ({ entry }) => entry.outcome === "alreadyDownloaded" || entry.outcome === "alreadyDownloading",
  );
  const unsupported = items.filter(({ entry }) => entry.outcome === "unsupported");

  return (
    <Card
      title="Submission"
      description="What happened to each selected post"
      action={
        <Badge tone={result.failed > 0 ? "warning" : "success"} mono>
          {result.tasksCreated} queued
        </Badge>
      }
    >
      <div className="space-y-4">
        <KeyValueList>
          <KeyValue label="Posts selected">{result.selected}</KeyValue>
          <KeyValue label="Posts resolved">{result.resolved}</KeyValue>
          <KeyValue label="Download tasks created">{result.tasksCreated}</KeyValue>
          <KeyValue label="Already handled">{result.skipped}</KeyValue>
          <KeyValue label="Nothing to download">{result.unsupported}</KeyValue>
          <KeyValue label="Failed">{result.failed}</KeyValue>
        </KeyValueList>

        {/* A created task is a queued download, not a finished one. Saying so
            is the difference between a report and a claim. */}
        <p className="text-xs leading-relaxed text-content-muted">
          A created task has been queued through the download pipeline. It is not finished media
          yet — progress and the saved file appear on the Downloads screen.
        </p>

        {failures.length > 0 && (
          <div className="space-y-1.5">
            <p className="text-xs font-medium text-content-primary">Could not be submitted</p>
            {failures.map(({ post, entry }) => (
              <p
                key={`${post.postId}:${entry.postId}`}
                className="text-[11px] leading-relaxed text-content-secondary"
              >
                <span className="text-content-primary">{post.title}</span>
                {entry.message ? ` — ${entry.message}` : ""}
              </p>
            ))}
          </div>
        )}

        {unsupported.length > 0 && (
          <div className="space-y-1.5">
            <p className="text-xs font-medium text-content-primary">Nothing to download</p>
            {unsupported.map(({ post, entry }) => (
              <p
                key={`${post.postId}:${entry.postId}`}
                className="text-[11px] leading-relaxed text-content-secondary"
              >
                <span className="text-content-primary">{post.title}</span>
                {entry.message ? ` — ${entry.message}` : ""}
              </p>
            ))}
          </div>
        )}

        {skipped.length > 0 && (
          <div className="space-y-1.5">
            <p className="text-xs font-medium text-content-primary">Already handled</p>
            {skipped.map(({ post, entry }) => (
              <p
                key={`${post.postId}:${entry.postId}`}
                className="text-[11px] leading-relaxed text-content-secondary"
              >
                <span className="text-content-primary">{post.title}</span>
                {entry.message ? ` — ${entry.message}` : ""}
              </p>
            ))}
          </div>
        )}
      </div>
    </Card>
  );
}
