/**
 * Paging a whole profile.
 *
 * Every one of these is a way the walk can go wrong rather than a way it goes
 * right, because going right is a loop. The source is a fake that does exactly
 * what each case needs — hands back a cursor twice, returns an empty page while
 * claiming there is more, fails once and then works, fails for good — so each
 * ending is pinned to a decision rather than to whatever Douyin did today.
 */

import { describe, expect, it, vi } from "vitest";

import { loadFull, type LoadFullDeps, type LoadFullProgress } from "@/features/profile/loadFull";
import type { ProfilePostsPage, ProfilePostView } from "@/types";



/** A post row, only as complete as these tests need. */
function view(postId: string): ProfilePostView {
  return {
    post: { id: postId } as ProfilePostView["post"],
    status: "new",
    downloadState: null,
    activeTaskId: null,
    note: null,
  };
}

function page(
  postIds: string[],
  next: string | null,
  hasMore: boolean,
  totalKnown: number | null = null,
): ProfilePostsPage {
  return {
    creator: { id: "creator-1" } as ProfilePostsPage["creator"],
    posts: postIds.map(view),
    nextCursor: next,
    hasMore,
    totalKnown,
    loadedTotal: 0,
  };
}

/** A source that answers from a fixed script, recording what it was asked. */
function source(answers: (cursor: string | null) => ProfilePostsPage | Promise<ProfilePostsPage>) {
  const asked: (string | null)[] = [];
  const fetchPage = vi.fn(async (cursor: string | null) => {
    asked.push(cursor);
    return answers(cursor);
  });
  return { fetchPage, asked };
}

function deps(
  fetchPage: LoadFullDeps["fetchPage"],
  overrides: Partial<LoadFullDeps> = {},
): { deps: LoadFullDeps; progress: LoadFullProgress[]; waits: number[] } {
  const progress: LoadFullProgress[] = [];
  const waits: number[] = [];
  return {
    progress,
    waits,
    deps: {
      fetchPage,
      onProgress: (value) => progress.push(value),
      shouldStop: () => false,
      // Instant, so a backoff does not cost the suite the waiting it avoids.
      wait: async (milliseconds) => {
        waits.push(milliseconds);
      },
      ...overrides,
    },
  };
}

describe("walking a listing to its end", () => {
  it("merges overlapping pages without duplicating a post", async () => {
    const { fetchPage } = source((cursor) => {
      if (cursor === null) return page(["a", "b", "c"], "p2", true, 5);
      if (cursor === "p2") return page(["c", "d", "e"], "p3", true, 5);
      return page(["e", "f"], null, false, 5);
    });

    const result = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: 5 },
      deps(fetchPage).deps,
    );

    expect(result.posts.map((p) => p.post.id)).toEqual(["a", "b", "c", "d", "e", "f"]);
    expect(result.progress.added).toBe(6);
    expect(result.progress.status).toBe("complete");
  });

  it("stops when the source says there is more but names no next page", async () => {
    // Inventing a cursor here would ask for a page the source never offered,
    // and every post after this point would be fabricated.
    const { fetchPage, asked } = source(() => page(["a", "b"], null, true, 40));

    const result = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: 40 },
      deps(fetchPage).deps,
    );

    expect(asked).toEqual([null]);
    expect(result.progress.status).toBe("withheld");
    expect(result.progress.note).toMatch(/named no next page/i);
    expect(result.posts).toHaveLength(2);
  });

  it("stops when the source hands back a cursor it has already given", async () => {
    const { fetchPage, asked } = source((cursor) => {
      if (cursor === null) return page(["a"], "same", true, null);
      return page(["b"], "same", true, null);
    });

    const result = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: null },
      deps(fetchPage).deps,
    );

    expect(asked).toEqual([null, "same"]);
    expect(result.progress.status).toBe("stopped");
    expect(result.progress.note).toMatch(/already given/i);
  });

  it("stops when two pages in a row add nothing", async () => {
    // A page that adds nothing once is normal — a pinned or deleted post
    // shifts the window. Twice in a row is the listing no longer advancing.
    let index = 0;
    const pages = [
      page(["a", "b"], "p2", true, null),
      page(["a", "b"], "p3", true, null),
      page(["a", "b"], "p4", true, null),
    ];
    const { fetchPage, asked } = source(() => pages[Math.min(index++, pages.length - 1)]);

    const result = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: null },
      deps(fetchPage).deps,
    );

    expect(asked).toHaveLength(3);
    expect(result.progress.status).toBe("stopped");
    expect(result.progress.note).toMatch(/added nothing new/i);
  });

  it("tells an exhausted listing apart from one the source stopped serving", async () => {
    // Every page read, and the count agrees: nothing further exists.
    const exhausted = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: 3 },
      deps(source(() => page(["a", "b", "c"], null, false, 3)).fetchPage).deps,
    );
    expect(exhausted.progress.status).toBe("complete");
    expect(exhausted.progress.note).toMatch(/every page the source made available/i);

    // The source states 498 posts and stops at three. That is not the same
    // thing, and the note says which happened rather than promising the rest.
    const withheld = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: 498 },
      deps(source(() => page(["a", "b", "c"], null, false, 498)).fetchPage).deps,
    );
    expect(withheld.progress.status).toBe("withheld");
    expect(withheld.progress.note).toMatch(/3 of the 498/);
  });

  it("says so when the source lists more posts than its own count states", async () => {
    // The shape a live authenticated run actually produced: the page's own grid
    // served 54 cards while the page's own label said 50. The walk did reach the
    // end, so it is not "withheld" — but a bare "complete" would let the surplus
    // pass as an exact fit. Neither number is bent to make the other true, and
    // the extra posts are kept rather than dropped to reach the stated count.
    const ids = Array.from({ length: 54 }, (_, index) => `p${index}`);
    const result = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: 50 },
      deps(source(() => page(ids, null, false, 50)).fetchPage).deps,
    );

    expect(result.progress.status).toBe("complete");
    expect(result.progress.note).toMatch(/54 posts, 4 more than the 50/);
    expect(result.posts).toHaveLength(54);
  });

  it("still calls an exact fit an exact fit", async () => {
    // The guard above must not turn every completion into a caveat: when the
    // count and the listing agree, the note says so plainly.
    const ids = Array.from({ length: 50 }, (_, index) => `p${index}`);
    const result = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: 50 },
      deps(source(() => page(ids, null, false, 50)).fetchPage).deps,
    );

    expect(result.progress.status).toBe("complete");
    expect(result.progress.note).toMatch(/every page the source made available/i);
    expect(result.progress.note).not.toMatch(/more than/);
  });
});

describe("a page that fails", () => {
  it("retries a failure the backend says is worth retrying, and carries on", async () => {
    let attempts = 0;
    const fetchPage = vi.fn(async () => {
      attempts += 1;
      if (attempts === 1) {
        // Shaped like a BackendError: the flag is what the retry keys on.
        throw Object.assign(new Error("the browser was not ready"), { retryable: true });
      }
      return page(["a", "b"], null, false, 2);
    });

    const { deps: run, progress, waits } = deps(fetchPage);
    const result = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: 2 },
      run,
    );

    expect(attempts).toBe(2);
    expect(waits).toEqual([500]);
    expect(result.progress.status).toBe("complete");
    expect(result.posts).toHaveLength(2);
    // The retry is reported while it is happening, not only if it fails.
    expect(progress.some((p) => p.note?.includes("attempt 2 of 3"))).toBe(true);
  });

  it("gives up after the bounded attempts and keeps what it has", async () => {
    const fetchPage = vi.fn(async () => {
      throw Object.assign(new Error("the browser was not ready"), { retryable: true });
    });
    const { deps: run, waits } = deps(fetchPage);

    const result = await loadFull(
      { cursor: null, hasMore: true, known: [view("kept")], totalKnown: 90 },
      run,
    );

    expect(fetchPage).toHaveBeenCalledTimes(3);
    expect(waits).toEqual([500, 1200]);
    expect(result.progress.status).toBe("failed");
    expect(result.posts.map((p) => p.post.id)).toEqual(["kept"]);
    expect(result.progress.note).toMatch(/loading full again will continue/i);
  });

  it("does not retry a failure that repeating cannot change", async () => {
    const fetchPage = vi.fn(async () => {
      throw Object.assign(new Error("the source refused this profile"), { retryable: false });
    });
    const { deps: run, waits } = deps(fetchPage);

    const result = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: null },
      run,
    );

    expect(fetchPage).toHaveBeenCalledTimes(1);
    expect(waits).toEqual([]);
    expect(result.progress.status).toBe("failed");
    expect(result.progress.note).toMatch(/refused/i);
  });

  it("keeps the pages already read when a later one fails for good", async () => {
    let index = 0;
    const fetchPage = vi.fn(async () => {
      index += 1;
      if (index === 1) return page(["a", "b"], "p2", true, 100);
      throw Object.assign(new Error("the profile page listed no posts"), { retryable: false });
    });

    const result = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: 100 },
      deps(fetchPage).deps,
    );

    expect(result.progress.status).toBe("failed");
    expect(result.progress.pages).toBe(1);
    expect(result.posts.map((p) => p.post.id)).toEqual(["a", "b"]);
    // The cursor it did reach is handed back, so the next run continues rather
    // than starting from the top.
    expect(result.cursor).toBe("p2");
  });
});

describe("stopping", () => {
  it("stops between pages and keeps everything retrieved", async () => {
    let stop = false;
    let index = 0;
    const fetchPage = vi.fn(async () => {
      index += 1;
      if (index === 2) stop = true;
      return page([`post-${index}`], `p${index + 1}`, true, null);
    });

    const { deps: run } = deps(fetchPage, { shouldStop: () => stop });
    const result = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: null },
      run,
    );

    expect(fetchPage).toHaveBeenCalledTimes(2);
    expect(result.progress.status).toBe("stopped");
    expect(result.posts).toHaveLength(2);
    expect(result.cursor).toBe("p3");
  });

  it("is honoured when it arrives during the last page", async () => {
    // There is no next iteration for a stop to be noticed at, so it has to be
    // noticed once this page settles. Without that, the button did nothing at
    // exactly the moment somebody would press it: the walk reached its own
    // ending first and reported that instead.
    let stop = false;
    const fetchPage = vi.fn(async () => {
      stop = true; // pressed while this request was out
      return page(["a", "b"], null, false, 2);
    });

    const { deps: run } = deps(fetchPage, { shouldStop: () => stop });
    const result = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: 2 },
      run,
    );

    expect(result.progress.status).toBe("stopped");
    expect(result.progress.note).toMatch(/kept/i);
    // The page that was already being fetched is kept: it is real data.
    expect(result.posts.map((p) => p.post.id)).toEqual(["a", "b"]);
    // And the source said that page was the last one, so the screen must not
    // go on offering to load more. The loop's own flag is only cleared on the
    // path a stop skips, so it has to come from the page that was read.
    expect(result.hasMore).toBe(false);
  });

  it("can be resumed from where it stopped", async () => {
    const first = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: null },
      deps(
        source((cursor) => (cursor === null ? page(["a"], "p2", true, null) : page(["b"], "p3", true, null)))
          .fetchPage,
        { shouldStop: () => true },
      ).deps,
    );
    // Stopped immediately, so nothing was read at all.
    expect(first.progress.status).toBe("stopped");

    // Resuming from what the stopped run handed back reaches the rest.
    const second = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: null },
      deps(source((cursor) => (cursor === "p2" ? page(["b", "c"], null, false, 3) : page(["a"], "p2", true, 3))).fetchPage).deps,
    );

    expect(second.progress.status).toBe("complete");
    expect(second.posts.map((p) => p.post.id)).toEqual(["a", "b", "c"]);
  });

  it("takes effect while a request is in flight", async () => {
    let release: (value: ProfilePostsPage) => void = () => {};
    let stop = false;
    const fetchPage = vi.fn(
      () =>
        new Promise<ProfilePostsPage>((resolve) => {
          release = resolve;
        }),
    );

    const run = loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: null },
      deps(fetchPage, { shouldStop: () => stop }).deps,
    );

    // Stopped while the first page is still out. The answer that arrives is
    // still merged — it is real data, and discarding it would throw away work
    // the source already did — but no further page is asked for.
    stop = true;
    release(page(["a", "b"], "p2", true, null));
    const result = await run;

    expect(fetchPage).toHaveBeenCalledTimes(1);
    expect(result.progress.status).toBe("stopped");
    expect(result.posts).toHaveLength(2);
  });
});

describe("what the screen is told while it runs", () => {
  it("reports pages, posts held and posts added as they arrive", async () => {
    let index = 0;
    const pages = [
      page(["a", "b"], "p2", true, 4),
      page(["b", "c"], "p3", true, 4),
      page(["c", "d"], null, false, 4),
    ];
    const { deps: run, progress } = deps(source(() => pages[Math.min(index++, 2)]).fetchPage);

    await loadFull({ cursor: null, hasMore: true, known: [], totalKnown: 4 }, run);

    const final = progress[progress.length - 1];
    expect(final.pages).toBe(3);
    expect(final.posts).toBe(4);
    expect(final.added).toBe(4);
    // Every intermediate report is a status the screen can render, and the
    // first one is emitted before anything is fetched.
    expect(progress[0].status).toBe("running");
    expect(progress.some((p) => p.pages === 1 && p.posts === 2)).toBe(true);
  });

  it("counts only posts that were not already held", async () => {
    const { deps: run, progress } = deps(
      source(() => page(["a", "b", "new"], null, false, null)).fetchPage,
    );

    await loadFull(
      { cursor: null, hasMore: true, known: [view("a"), view("b")], totalKnown: null },
      run,
    );

    expect(progress[progress.length - 1].added).toBe(1);
    expect(progress[progress.length - 1].posts).toBe(3);
  });
});

describe("a profile that exposes only a few posts", () => {
  it("reads what it is given and says the source stopped there", async () => {
    // What an anonymous visitor actually gets: a handful of posts and then a
    // page that states a far larger count and offers nothing more. The honest
    // report is the one the user can act on — this is not the whole profile.
    let index = 0;
    const pages = [page(["a", "b", "c"], "p2", true, 498), page([], null, false, 498)];
    const { deps: run } = deps(source(() => pages[Math.min(index++, 1)]).fetchPage);

    const result = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: 498 },
      run,
    );

    expect(result.progress.status).toBe("withheld");
    expect(result.posts).toHaveLength(3);
    expect(result.progress.note).toMatch(/3 of the 498/);
  });

  it("reads an empty page that claims more as the source withholding", async () => {
    const { deps: run } = deps(source(() => page([], null, true, 22)).fetchPage);

    const result = await loadFull(
      { cursor: null, hasMore: true, known: [], totalKnown: 22 },
      run,
    );

    expect(result.progress.status).toBe("withheld");
    expect(result.posts).toHaveLength(0);
  });
});
