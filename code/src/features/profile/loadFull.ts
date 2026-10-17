/**
 * Paging a whole profile, one page at a time.
 *
 * The listing the source exposes is a rendered grid, which this application
 * reads in slices of its own page size. "Load full" is that walk taken to its
 * end — and the interesting part is not the loop, it is every way the loop can
 * go wrong. A source that keeps handing back the same cursor, a page that adds
 * nothing twice in a row, a page that fails transiently and one that fails
 * for good, and a user who wants it to stop: each has to end somewhere
 * defensible rather than spinning or throwing away what has already been read.
 *
 * Kept out of the component so all of that can be tested against a source that
 * does exactly what each case needs, with no browser and no waiting.
 */

import type { ProfilePostsPage, ProfilePostView } from "@/types";

/** Where a run has got to. */
export type LoadFullStatus =
  | "running"
  | "stopped"
  | "complete"
  | "withheld"
  | "failed";

export interface LoadFullProgress {
  /** Pages fetched during this run. */
  pages: number;
  /** Posts held once the last page was merged. */
  posts: number;
  /** Posts this run added that were not already there. */
  added: number;
  status: LoadFullStatus;
  /** Why it ended, in a sentence, when the ending needs explaining. */
  note: string | null;
  /**
   * The profile's own post count, when the source states one.
   *
   * Carried so the screen can show a real fraction rather than inventing a
   * percentage: without it, the only honest thing to show is that work is
   * happening.
   */
  totalKnown: number | null;
}

export interface LoadFullInput {
  /** Where the listing currently stands. */
  cursor: string | null;
  hasMore: boolean;
  /** Posts already held, so "added" counts only what is new. */
  known: ProfilePostView[];
  /** The source's own count for the profile, when it states one. */
  totalKnown: number | null;
}

export interface LoadFullDeps {
  fetchPage: (cursor: string | null) => Promise<ProfilePostsPage>;
  onProgress: (progress: LoadFullProgress) => void;
  /** Checked between requests, so Stop takes effect without one in flight. */
  shouldStop: () => boolean;
  /** Waits between attempts. Injected so a test does not spend the time. */
  wait: (milliseconds: number) => Promise<void>;
}

export interface LoadFullResult {
  posts: ProfilePostView[];
  cursor: string | null;
  hasMore: boolean;
  progress: LoadFullProgress;
}

/** How many times one page is attempted before the run gives up on it. */
const MAX_ATTEMPTS = 3;

/** Wait before the second and third attempt at a page. */
const BACKOFF_MS = [500, 1200];

/**
 * How many consecutive pages may add nothing before the walk stops.
 *
 * Two, not one: pages of a live listing overlap when a post is pinned or
 * deleted, so a page that adds nothing is not by itself evidence that the walk
 * has stopped advancing. Two in a row is.
 */
const EMPTY_PAGES_BEFORE_GIVING_UP = 2;

/**
 * Walk the listing to its end, reporting as it goes.
 *
 * Returns whatever was retrieved, always: a run that failed on page nine leaves
 * the caller with the first eight so the next attempt continues from there
 * rather than starting over.
 */
export async function loadFull(
  input: LoadFullInput,
  deps: LoadFullDeps,
): Promise<LoadFullResult> {
  const held = new Map(input.known.map((view) => [view.post.id, view]));
  const seenCursors = new Set<string>();
  if (input.cursor) seenCursors.add(input.cursor);

  let cursor = input.cursor;
  let hasMore = input.hasMore;
  let pages = 0;
  let added = 0;
  let emptyPages = 0;

  const report = (state: LoadFullStatus, note: string | null = null) =>
    deps.onProgress({ pages, posts: held.size, added, status: state, note, totalKnown: input.totalKnown });

  const finish = (state: LoadFullStatus, note: string | null = null): LoadFullResult => {
    const progress: LoadFullProgress = {
      pages,
      posts: held.size,
      added,
      status: state,
      note,
      totalKnown: input.totalKnown,
    };
    deps.onProgress(progress);
    return { posts: [...held.values()], cursor, hasMore, progress };
  };

  report("running");

  while (hasMore) {
    if (deps.shouldStop()) {
      return finish("stopped", "Stopped. Everything retrieved so far has been kept.");
    }

    const page = await fetchWithRetries(cursor, deps, (attempt, error) => {
      report(
        "running",
        `Page ${pages + 1} failed (${error}). Retrying — attempt ${attempt + 2} of ${MAX_ATTEMPTS}.`,
      );
    });

    if (!page.ok) {
      // Already-retrieved posts are kept; the run reports the failure and the
      // screen leaves the error on show with everything loaded so far intact.
      return finish(
        "failed",
        page.retryable
          ? `Page ${pages + 1} could not be read after ${MAX_ATTEMPTS} attempts. Everything retrieved so far has been kept; loading full again will continue from here.`
          : `Page ${pages + 1} was refused. Everything retrieved so far has been kept.`,
      );
    }

    pages += 1;
    let newOnThisPage = 0;
    for (const post of page.value.posts) {
      const postId = post.post.id;
      if (!held.has(postId)) {
        held.set(postId, post);
        newOnThisPage += 1;
      }
    }
    added += newOnThisPage;
    emptyPages = newOnThisPage === 0 ? emptyPages + 1 : 0;

    const next = page.value.nextCursor;
    const moreFromSource = page.value.hasMore;
    // What the *source* said about this page, which is the truth the caller
    // should resume from. Leaving the loop's own flag alone would have a stop
    // during the last page report that more pages remain — the walk never
    // reached the line that clears it — and the screen would offer to load
    // more when there is nothing to load.
    hasMore = moreFromSource;
    report("running");

    // A Stop pressed while this page was in flight is honoured now that it has
    // settled. The page that was already being fetched is merged — it is real
    // data, and discarding it would throw away work the source did — and
    // nothing further is asked for.
    //
    // Checked *after* the merge and before the end-of-listing decision below,
    // because otherwise a Stop arriving during the last page changed nothing:
    // the walk reached its own ending first and reported that instead, so the
    // button appeared not to work exactly when a person would press it.
    if (deps.shouldStop()) {
      // Resuming continues from where this page left off rather than asking
      // for a page that has already been merged.
      if (next && !seenCursors.has(next)) {
        cursor = next;
      }
      return finish("stopped", "Stopped. Everything retrieved so far has been kept.");
    }

    // The source says there is more but names nowhere to go. Continuing would
    // mean inventing a cursor, and a fabricated one asks for a page the source
    // never offered.
    if (moreFromSource && !next) {
      return finish(
        "withheld",
        `The source says more posts exist but named no next page after ${held.size}.`,
      );
    }

    if (next && seenCursors.has(next)) {
      return finish(
        "stopped",
        `The source returned a page it had already given, so the walk stopped rather than loop forever. ${held.size} posts are held.`,
      );
    }

    if (emptyPages >= EMPTY_PAGES_BEFORE_GIVING_UP) {
      return finish(
        "stopped",
        `Two pages in a row added nothing new, so the walk stopped. ${held.size} posts are held.`,
      );
    }

    if (!moreFromSource) {
      // The end, and the only place the three endings are told apart.
      //
      // A stated count that is larger than what was retrieved means the source
      // stopped short — a different thing from having nothing further to give.
      const shortOfDeclaredCount =
        input.totalKnown != null && held.size < input.totalKnown;
      if (shortOfDeclaredCount) {
        return finish(
          "withheld",
          `The source stopped returning pages at ${held.size} of the ${input.totalKnown} posts it says this profile has.`,
        );
      }

      // The source listed *more* than its own count states. Both numbers come
      // from the same page, and neither is discarded to make the other true:
      // the count is not trusted over what the page actually served, and the
      // extra posts are kept rather than dropped to reach an exact fit.
      //
      // The walk did reach the end — the source said there was nothing more —
      // so this stays "complete". What must not happen is the surplus passing
      // unmentioned, which would read as the count confirming the walk.
      const surplus =
        input.totalKnown != null && held.size > input.totalKnown
          ? held.size - input.totalKnown
          : 0;
      return finish(
        "complete",
        surplus > 0
          ? `Every page the source made available has been read — ${held.size} posts, ${surplus} more than the ${input.totalKnown} the profile states.`
          : `Every page the source made available has been read — ${held.size} posts.`,
      );
    }

    if (!next) {
      // Unreachable: a source that says "more" without a cursor returned above.
      return finish("stopped", "The source named no next page.");
    }
    seenCursors.add(next);
    cursor = next;
  }

  return finish(
    "complete",
    `Every page the source made available has been read — ${held.size} posts.`,
  );
}

type Attempt<T> =
  | { ok: true; value: T }
  | { ok: false; retryable: boolean; error: string };

/** One page, retried while the failure is one that retrying could fix. */
async function fetchWithRetries(
  cursor: string | null,
  deps: LoadFullDeps,
  onRetry: (attempt: number, error: string) => void,
): Promise<Attempt<ProfilePostsPage>> {
  let last: { retryable: boolean; error: string } = {
    retryable: false,
    error: "unknown failure",
  };

  for (let attempt = 0; attempt < MAX_ATTEMPTS; attempt += 1) {
    if (attempt > 0) await deps.wait(BACKOFF_MS[attempt - 1] ?? BACKOFF_MS[0]);
    try {
      return { ok: true, value: await deps.fetchPage(cursor) };
    } catch (raw) {
      // A failure that repeating cannot change — a refused resource, a bad
      // cursor — stops here rather than spending the remaining attempts on it.
      const retryable = isRetryable(raw);
      last = { retryable, error: describe(raw) };
      if (!retryable) break;
      if (deps.shouldStop()) break;
      if (attempt + 1 < MAX_ATTEMPTS) onRetry(attempt, last.error);
    }
  }

  return { ok: false, ...last };
}

function isRetryable(raw: unknown): boolean {
  return (
    typeof raw === "object" &&
    raw !== null &&
    "retryable" in raw &&
    (raw as { retryable?: unknown }).retryable === true
  );
}

function describe(raw: unknown): string {
  if (raw instanceof Error) return raw.message;
  return String(raw);
}
