/**
 * Navigating between screens.
 *
 * The bug this pins down: switching tabs used to destroy the screen you left.
 * Everything you had done — the link you pasted, the profile you loaded, the
 * posts you had selected, the download you had just submitted — lived in that
 * screen's own state, and React Router unmounts the route it leaves. Coming
 * back meant starting again.
 *
 * These are application-level, driving the real router, because the property
 * being checked is about which components are mounted when the route changes —
 * something a test of one screen in isolation cannot see.
 */

import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { App } from "@/App";
import { useDownloadStore } from "@/stores/useDownloadStore";
import type { ProfilePostsPage, TestFixture } from "@/types";

const listen = vi.fn(async () => () => {});
vi.mock("@tauri-apps/api/event", () => ({
  listen: (...args: unknown[]) => listen(...(args as [])),
}));

const getTestFixtures = vi.fn(async (): Promise<TestFixture[]> => []);
// One task, not none: the Downloads screen only failed once it had a row to
// draw, so an empty queue here would hide the very thing this mounting is
// meant to exercise.
const getDownloadQueue = vi.fn(async () => ({
  tasks: [
    {
      id: "task-1",
      postId: "post-1",
      mediaItemId: null,
      variantId: null,
      sourceUrl: "https://cdn.test/a.mp4",
      destinationPath: "D:\Downloads\Douyin\clip.mp4",
      fileName: "clip.mp4",
      extension: "mp4",
      mediaType: "video",
      state: "completed",
      attempts: 1,
      maxAttempts: 3,
      progress: { downloadedBytes: 4096, totalBytes: 4096 },
      error: null,
      createdAt: "2026-01-01T00:00:00Z",
      updatedAt: "2026-01-01T00:00:00Z",
      startedAt: null,
      completedAt: null,
    },
  ],
  summary: {
    total: 0,
    queued: 0,
    downloading: 0,
    paused: 0,
    completed: 0,
    failed: 0,
    cancelled: 0,
    activeDownloadedBytes: 0,
    activeTotalBytes: null,
    activeWithoutSize: 0,
    completedBytes: 0,
  },
}));

/// One post, so a loaded profile has something in it to survive a tab switch.
function profilePost(id: string) {
  return {
    post: {
      id,
      platform: "douyin",
      platformPostId: id,
      creatorId: "creator-1",
      creatorHandle: "fixture",
      url: "https://www.douyin.com/video/" + id,
      canonicalUrl: "https://www.douyin.com/video/" + id,
      caption: "a fixture post",
      mediaType: "video",
      thumbnailUrl: null,
      publishedAt: null,
      status: "ready",
      mediaItems: [],
      metadata: {},
      createdAt: "2026-01-01T00:00:00Z",
      updatedAt: "2026-01-01T00:00:00Z",
    },
    status: "new",
    downloadState: null,
    activeTaskId: null,
    note: null,
  };
}

vi.mock("@/services/api", () => ({
  api: {
    getAppInfo: vi.fn(async () => null),
    getDatabaseStatus: vi.fn(async () => null),
    getSettings: vi.fn(async () => null),
    getSettingsOptions: vi.fn(async () => null),
    getTestFixtures: () => getTestFixtures(),
    analyzeUrl: vi.fn(async () => null),
    getDownloadQueue: () => getDownloadQueue(),
    listHistory: vi.fn(async () => []),
    listProfilePosts: vi.fn(async () => ({
      creator: { id: "creator-1" },
      posts: [profilePost("111"), profilePost("222")],
      nextCursor: null,
      hasMore: false,
      totalKnown: 2,
      loadedTotal: 2,
    })),
    resolveProfile: vi.fn(async () => ({
      creator: { id: "creator-1", displayName: "Fixture Creator" },
      notice: null,
      resolvedVia: "browser",
    })),
    submitProfileDownloads: vi.fn(async (input: { postIds: string[] }) => ({
      selected: input.postIds.length,
      resolved: input.postIds.length,
      tasksCreated: input.postIds.length,
      skipped: 0,
      failed: 0,
      unsupported: 0,
      posts: input.postIds.map((postId, index) => ({
        postId,
        platformPostId: postId,
        title: postId,
        outcome: "queued",
        media: [
          {
            postId,
            platformPostId: postId,
            title: postId,
            outcome: "taskCreated",
            message: null,
            taskId: `task-${index + 9}`,
          },
        ],
      })),
    })),
    beginLogin: vi.fn(async () => null),
    getLoginStatus: vi.fn(async () => ({
      available: true,
      status: "signedOut",
      profileDirectory: "C:\data\browser\profile",
    })),
    clearLoginSession: vi.fn(async () => null),
    browseMediaFolders: vi.fn(async () => ({
      root: "D:/Downloads/Douyin",
      folders: [],
      looseMediaCount: 0,
    })),
    browseMediaFolder: vi.fn(async () => ({
      path: "D:/Downloads/Douyin",
      folders: [],
      files: [],
      truncated: false,
    })),
    openMediaFile: vi.fn(async () => null),
    mediaThumbnail: vi.fn(async () => null),
    scanDownloadedFiles: vi.fn(async () => ({
      root: "D:/Downloads/Douyin",
      scannedAt: "2026-01-01T00:00:00Z",
      entries: [],
    })),
    savePostText: vi.fn(async () => ({
      postId: "x",
      path: "D:/Downloads/Text/x.txt",
      bytes: 1,
      renamed: false,
    })),
  },
}));

beforeEach(() => {
  listen.mockClear();
  getTestFixtures.mockClear();
  getDownloadQueue.mockClear();
});

/**
 * The sidebar link for a screen, by the name it is shown under.
 *
 * Scoped to the navigation landmark: screens themselves carry links to other
 * screens (Home links to Downloads, and so on), and the property these tests
 * exercise is navigation through the application's own destinations.
 */
function tab(name: string) {
  return within(screen.getByRole("navigation", { name: "Main" })).getByRole("link", {
    name: new RegExp(name, "i"),
  });
}

describe("moving between screens", () => {
  it("keeps what was typed on one screen while another is shown", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(tab("Single URL"));
    const field = await screen.findByLabelText(/douyin link/i);
    await user.type(field, "https://www.douyin.com/video/7281234567890123456");
    expect(field).toHaveValue("https://www.douyin.com/video/7281234567890123456");

    await user.click(tab("Downloads"));
    // The screen it left is hidden rather than gone.
    expect(field.closest("[class~='hidden']")).not.toBeNull();

    await user.click(tab("Single URL"));

    // The same input, with the same text. Not a cleared one, and not a
    // different element.
    const again = screen.getByLabelText(/douyin link/i);
    expect(again).toHaveValue("https://www.douyin.com/video/7281234567890123456");
    expect(again).toBe(field);
  });

  it("keeps a screen mounted while another one is shown", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(tab("Single URL"));
    const field = await screen.findByLabelText(/douyin link/i);

    await user.click(tab("Downloads"));

    // Still in the document — hidden, not unmounted. This is what makes the
    // previous test work, and it is the difference between a session that
    // survives and one that does not.
    expect(field).toBeInTheDocument();
    expect(field.closest("[class~='hidden']")).not.toBeNull();
  });

  it("keeps a loaded profile and its selection across a tab switch", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(tab("Profile Downloader"));
    // The fixture list is empty in this test, so the link is typed rather than
    // prefilled — the same path a user takes.
    await user.type(
      await screen.findByLabelText(/Douyin profile link/i),
      "https://v.douyin.com/SMt7NsUPaGo/",
    );
    await user.click(screen.getByRole("button", { name: "Load profile" }));

    // The listing arrives.
    await waitFor(() => expect(screen.getAllByText(/a fixture post/).length).toBe(2));
    const boxes = screen.getAllByRole("checkbox");
    await user.click(boxes[boxes.length - 1]);

    // Away, and back.
    await user.click(tab("Downloads"));
    await user.click(tab("Profile Downloader"));

    // The posts are still there, and so is the selection: both would be gone
    // if the screen had been unmounted and rebuilt.
    expect(screen.getAllByText(/a fixture post/).length).toBe(2);
    expect(screen.getAllByRole("checkbox").some((box) => (box as HTMLInputElement).checked)).toBe(
      true,
    );
  });

  it("lets a carousel's pictures be chosen one by one", async () => {
    // A carousel is not one download and not necessarily all of them. What the
    // screen must get right is that the choice it shows is the choice it sends:
    // three pictures found, one taken out, two asked for.
    const user = userEvent.setup();
    render(<App />);

    await user.click(tab("Profile Downloader"));
    const { api } = await import("@/services/api");

    const carousel = profilePost("222");
    vi.mocked(api.listProfilePosts).mockResolvedValueOnce({
      creator: { id: "creator-1" },
      posts: [
        profilePost("111"),
        {
          ...carousel,
          // Already downloaded: its first picture landed in an earlier run. The
          // gate used to read that as "nothing to do here" and refuse the post,
          // so the other pictures could never be asked for however clearly the
          // chooser showed them.
          status: "downloaded",
          post: { ...carousel.post, mediaType: "carousel", platformPostId: "222" },
        },
      ],
      nextCursor: null,
      hasMore: false,
      totalKnown: 2,
      loadedTotal: 2,
    } as unknown as ProfilePostsPage);

    const picture = (id: string) => ({
      id,
      postId: "222",
      mediaType: "image",
      sourceUrl: `https://p3-cdn.douyinpic.com/${id}.jpg`,
      thumbnailUrl: `https://p3-cdn.douyinpic.com/${id}.jpg`,
      mimeType: "image/jpeg",
      extension: "jpg",
      width: 1080,
      height: 1440,
      durationSeconds: null,
      fileSize: null,
      variants: [],
      metadata: {},
    });
    vi.mocked(api.analyzeUrl).mockResolvedValueOnce({
      intent: "post",
      post: { id: "222" },
      media: [picture("m1"), picture("m2"), picture("m3")],
    } as never);

    await user.type(
      await screen.findByLabelText(/Douyin profile link/i),
      "https://v.douyin.com/SMt7NsUPaGo/",
    );
    await user.click(screen.getByRole("button", { name: "Load profile" }));
    await waitFor(() => expect(screen.getAllByText(/a fixture post/).length).toBe(2));

    // Only the carousel offers a chooser.
    await user.click(screen.getByRole("button", { name: "Choose images" }));
    expect(await screen.findByText("3 of 3 selected")).toBeInTheDocument();

    await user.click(screen.getByRole("checkbox", { name: "Image 2" }));
    expect(await screen.findByText("2 of 3 selected")).toBeInTheDocument();

    // Choose the carousel post itself and submit.
    const boxes = screen.getAllByRole("checkbox", { name: /a fixture post/i });
    await user.click(boxes[boxes.length - 1]);
    await user.click(screen.getByRole("button", { name: "Download selected" }));

    await waitFor(() => expect(api.submitProfileDownloads).toHaveBeenCalled());
    const sent = vi.mocked(api.submitProfileDownloads).mock.calls[0][0] as {
      selectedItems?: Record<string, string[]>;
    };
    expect(sent.selectedItems).toEqual({ "222": ["m1", "m3"] });
  });

  it("keeps the posts already read when the listing is refreshed", async () => {
    // The sequence a user reaches for when the source stops early: an early
    // stop clears hasMore, which disables Load full, so Refresh is the only way
    // forward they have. Re-reading the listing must not charge them the posts
    // they already had — otherwise "Refresh then Load full" costs them work to
    // make progress, which is exactly what it looked like it was doing.
    const user = userEvent.setup();
    render(<App />);

    await user.click(tab("Profile Downloader"));
    await user.type(
      await screen.findByLabelText(/Douyin profile link/i),
      "https://v.douyin.com/SMt7NsUPaGo/",
    );
    await user.click(screen.getByRole("button", { name: "Load profile" }));
    await waitFor(() => expect(screen.getAllByText(/a fixture post/).length).toBe(2));

    // The refresh comes back with the listing a moment later: one post the app
    // already holds, and one it has not seen before.
    const { api } = await import("@/services/api");
    vi.mocked(api.listProfilePosts).mockResolvedValueOnce({
      creator: { id: "creator-1" },
      posts: [profilePost("111"), profilePost("333")],
      nextCursor: null,
      hasMore: false,
      totalKnown: 3,
      loadedTotal: 3,
    } as unknown as ProfilePostsPage);

    // The listing's own Refresh, not the application's top-bar one: both carry
    // the same name, and the listing toolbar is the one that owns this action.
    const toolbar = screen.getByRole("button", { name: "Load full" }).parentElement;
    if (!toolbar) throw new Error("the listing toolbar must be in the document");
    await user.click(within(toolbar).getByRole("button", { name: "Refresh" }));

    // Three, not two: the post the refresh did not mention is still on screen.
    await waitFor(() => expect(screen.getAllByText(/a fixture post/).length).toBe(3));
  });

  it("updates a profile post when its download finishes, however the store heard", async () => {
    // The live failure: a post downloaded from Profile Downloader, the Downloads
    // screen showed the task completed, and the post row stayed on "Downloading"
    // for ever. The store had the right task — the row was patched by an effect
    // gated on a counter that a queue snapshot does not move, and a snapshot is
    // a perfectly ordinary way for the store to learn a task finished.
    useDownloadStore.setState({
      tasks: {},
      order: [],
      summary: null,
      rates: {},
      hydrated: false,
      error: null,
      revision: 0,
      taskRevision: {},
      stateRevision: 0,
    });

    const user = userEvent.setup();
    render(<App />);

    await user.click(tab("Profile Downloader"));
    await user.type(
      await screen.findByLabelText(/Douyin profile link/i),
      "https://v.douyin.com/SMt7NsUPaGo/",
    );

    // Two posts, told apart by caption so a badge can be attributed.
    const { api } = await import("@/services/api");
    const captioned = (id: string, caption: string) => {
      const base = profilePost(id);
      return { ...base, post: { ...base.post, caption } };
    };
    vi.mocked(api.listProfilePosts).mockResolvedValueOnce({
      creator: { id: "creator-1" },
      posts: [captioned("111", "the first post"), captioned("222", "the second post")],
      nextCursor: null,
      hasMore: false,
      totalKnown: 2,
      loadedTotal: 2,
    } as unknown as ProfilePostsPage);

    await user.click(screen.getByRole("button", { name: "Load profile" }));
    await waitFor(() => expect(screen.getByText("the first post")).toBeInTheDocument());

    // Submit the first post, and only the first: the second must not inherit it.
    await user.click(screen.getByRole("checkbox", { name: /the first post/i }));
    await user.click(screen.getByRole("button", { name: "Download selected" }));

    await waitFor(() => expect(screen.getAllByText("Downloading")).toHaveLength(1));

    // The store learns the outcome the way a missed event is recovered: from the
    // backend's queue, not from an event of its own.
    getDownloadQueue.mockResolvedValueOnce({
      tasks: [
        {
          id: "task-9",
          postId: "111",
          mediaItemId: null,
          variantId: null,
          sourceUrl: "https://cdn.test/a.mp4",
          destinationPath: "D:\\Downloads\\clip.mp4",
          fileName: "clip.mp4",
          extension: "mp4",
          mediaType: "video",
          state: "completed",
          attempts: 1,
          maxAttempts: 3,
          progress: { downloadedBytes: 4096, totalBytes: 4096 },
          error: null,
          createdAt: "2026-01-01T00:00:00Z",
          updatedAt: "2026-01-01T00:00:00Z",
          startedAt: null,
          completedAt: null,
        },
      ],
      summary: null,
    } as never);
    await act(async () => {
      await useDownloadStore.getState().reconcile();
    });

    // The row follows the same task the Downloads screen is reading.
    await waitFor(() => expect(screen.getAllByText("Downloaded")).toHaveLength(1));
    expect(screen.queryByText("Downloading")).not.toBeInTheDocument();
  });

  it("subscribes to the download stream once, however many screens are visited", async () => {
    const user = userEvent.setup();
    render(<App />);

    for (const name of ["Single URL", "Downloads", "History", "Single URL", "Downloads"]) {
      await user.click(tab(name));
    }

    // One listener for the application, established at the root. A subscription
    // per screen is how the same task ended up with several handlers, each with
    // its own idea of the current state.
    expect(listen).toHaveBeenCalledTimes(1);
  });

  it("survives being switched between faster than the screens can settle", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(tab("Single URL"));
    const field = await screen.findByLabelText(/douyin link/i);
    await user.type(field, "https://www.douyin.com/video/1");

    // No waiting between clicks: the screens are asked to mount and hide while
    // their own requests are still in flight.
    await user.click(tab("Profile Downloader"));
    await user.click(tab("Downloads"));
    await user.click(tab("History"));
    await user.click(tab("Single URL"));

    expect(screen.getByLabelText(/douyin link/i)).toHaveValue("https://www.douyin.com/video/1");
    expect(listen).toHaveBeenCalledTimes(1);
  });
});

/**
 * Opening a post's image chooser.
 *
 * Reading a post's pictures means rendering its page in a browser, which takes
 * about a minute and a half. The screen must therefore not do it unless it has
 * to, must not do it twice for one answer, and must never leave the user
 * guessing whether the source said "no pictures" or was simply not asked.
 */
describe("opening a post's image chooser", () => {
  // Each test here decides for itself what the source answers, so nothing may
  // survive from the one before — a queued "once" that never fired would decide
  // the next test's outcome.
  beforeEach(async () => {
    const { api } = await import("@/services/api");
    vi.mocked(api.analyzeUrl).mockReset();
    vi.mocked(api.listProfilePosts).mockReset();
  });

  /** A picture, as the resolver reports one. */
  function picture(id: string, postId: string) {
    return {
      id,
      postId,
      mediaType: "image" as const,
      sourceUrl: `https://p3-cdn.douyinpic.com/${id}.jpeg`,
      thumbnailUrl: `https://p3-cdn.douyinpic.com/${id}.jpeg`,
      mimeType: "image/jpeg",
      extension: "jpg",
      width: 1080,
      height: 1440,
      durationSeconds: null,
      fileSize: null,
      variants: [],
      metadata: {},
    };
  }

  /** A carousel row, with whatever its stored media happens to be. */
  function carousel(id: string, mediaItems: unknown[], status = "ready") {
    const base = profilePost(id);
    return {
      ...base,
      status,
      post: {
        ...base.post,
        mediaType: "carousel" as const,
        platformPostId: id,
        // Named, so a test can say which post it means without counting rows.
        caption: `post ${id}`,
        mediaItems,
      },
    };
  }

  /** One post's card, found by the caption it is showing. */
  function card(id: string): HTMLElement {
    const caption = screen.getByText(`post ${id}`);
    return caption.parentElement!.parentElement as HTMLElement;
  }

  /** That card's chooser, whatever it is currently called. */
  function chooser(id: string): HTMLElement {
    return within(card(id)).getByRole("button", { name: /images$/ });
  }

  async function loadProfileWith(user: ReturnType<typeof userEvent.setup>, posts: unknown[]) {
    const { api } = await import("@/services/api");
    vi.mocked(api.listProfilePosts).mockResolvedValueOnce({
      creator: { id: "creator-1" },
      posts,
      nextCursor: null,
      hasMore: false,
      totalKnown: posts.length,
      loadedTotal: posts.length,
    } as never);

    render(<App />);
    await user.click(tab("Profile Downloader"));
    await user.type(
      await screen.findByLabelText(/Douyin profile link/i),
      "https://v.douyin.com/SMt7NsUPaGo/",
    );
    await user.click(screen.getByRole("button", { name: "Load profile" }));
    // Wait for the posts themselves, not merely for the request: a click made
    // between the two lands on a screen that has not drawn its rows yet.
    await waitFor(() => expect(screen.getAllByText(/^post /)).toHaveLength(posts.length));
    return api;
  }

  it("shows the pictures already on file without reading the page again", async () => {
    // The post was resolved on an earlier run and its pictures are stored. Asking
    // the source again to learn something already known is the ninety-second
    // pause this exists to remove.
    const user = userEvent.setup();
    const api = await loadProfileWith(user, [
      carousel("222", [picture("m1", "222"), picture("m2", "222")]),
    ]);
    vi.mocked(api.analyzeUrl).mockClear();

    await user.click(chooser("222"));

    expect(await screen.findByText(/2 of 2 selected/)).toBeInTheDocument();
    expect(screen.getByText(/from the copy already on file/)).toBeInTheDocument();
    expect(api.analyzeUrl).not.toHaveBeenCalled();
  });

  it("says it is reading, and leaves the rest of the screen usable while it does", async () => {
    // A cold post: nothing stored, so the page has to be read. The wait must be
    // visible, cancellable, and confined to the row that asked for it.
    const user = userEvent.setup();
    const api = await loadProfileWith(user, [
      carousel("111", []),
      carousel("222", [picture("m1", "222")]),
    ]);

    let finish: (value: unknown) => void = () => {};
    vi.mocked(api.analyzeUrl).mockImplementationOnce(
      () => new Promise((resolve) => {
        finish = resolve;
      }) as never,
    );

    await user.click(chooser("111"));

    // The row says what it is doing, and offers a way out.
    expect(await screen.findByText(/Reading this post/)).toBeInTheDocument();
    expect(within(card("111")).getByRole("button", { name: "Cancel" })).toBeInTheDocument();

    // The other post's chooser still opens, from the copy it already has, so the
    // screen is not waiting on the post that is being read.
    await user.click(chooser("222"));
    expect(await within(card("222")).findByText(/1 of 1 selected/)).toBeInTheDocument();
    expect(api.analyzeUrl).toHaveBeenCalledTimes(1);

    finish({ intent: "post", post: { id: "111" }, media: [picture("cold", "111")] });
    // And when it lands, that post shows its picture too.
    expect(await within(card("111")).findByText(/1 of 1 selected/)).toBeInTheDocument();
  });

  it("asks the source once, however often the chooser is opened and closed", async () => {
    // Opening and closing a chooser is how a user compares posts. Each open must
    // not start its own browser render.
    const user = userEvent.setup();
    const api = await loadProfileWith(user, [carousel("222", [])]);
    vi.mocked(api.analyzeUrl).mockClear();

    let finish: (value: unknown) => void = () => {};
    vi.mocked(api.analyzeUrl).mockImplementation(
      () => new Promise((resolve) => {
        finish = resolve;
      }) as never,
    );

    await user.click(chooser("222")); // Choose
    await user.click(chooser("222")); // Hide
    await user.click(chooser("222")); // Choose again
    await user.click(chooser("222")); // Hide, again

    expect(api.analyzeUrl).toHaveBeenCalledTimes(1);

    finish({ intent: "post", post: { id: "222" }, media: [picture("m1", "222")] });
    // And the one answer is what the reopened chooser shows.
    await user.click(chooser("222"));
    expect(await within(card("222")).findByText(/1 of 1 selected/)).toBeInTheDocument();
    expect(api.analyzeUrl).toHaveBeenCalledTimes(1);
  });

  it("cancelling the wait keeps the pictures that were already found", async () => {
    // Cancel is about the waiting, not about the work: the answer is kept, so
    // reopening does not pay for it a second time.
    const user = userEvent.setup();
    const api = await loadProfileWith(user, [carousel("222", [])]);
    vi.mocked(api.analyzeUrl).mockClear();

    let finish: (value: unknown) => void = () => {};
    vi.mocked(api.analyzeUrl).mockImplementationOnce(
      () => new Promise((resolve) => {
        finish = resolve;
      }) as never,
    );

    await user.click(chooser("222"));
    await user.click(await within(card("222")).findByRole("button", { name: "Cancel" }));

    // The section is dismissed and nothing was claimed about the post.
    expect(screen.queryByText(/Reading this post/)).not.toBeInTheDocument();
    expect(within(card("222")).getByRole("button", { name: "Choose images" })).toBeInTheDocument();

    finish({ intent: "post", post: { id: "222" }, media: [picture("m1", "222")] });

    await user.click(chooser("222"));
    expect(await within(card("222")).findByText(/1 of 1 selected/)).toBeInTheDocument();
    expect(api.analyzeUrl).toHaveBeenCalledTimes(1);
  });

  it("tells a post with no pictures apart from a source that could not be read", async () => {
    // The distinction that matters: "the source says there are none" is a fact
    // about the post; "the source could not be reached" is not.
    const user = userEvent.setup();
    const api = await loadProfileWith(user, [carousel("222", [])]);

    vi.mocked(api.analyzeUrl).mockResolvedValueOnce({
      intent: "post",
      post: { id: "222" },
      media: [],
    } as never);
    await user.click(chooser("222"));
    expect(await within(card("222")).findByText(/it holds no pictures/)).toBeInTheDocument();
    // Nothing to offer, and no offer to try again that could not help.
    expect(screen.queryByRole("button", { name: "Try again" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Download selected" })).toBeDisabled();
  });

  it("offers to try again when the source could not be read at all", async () => {
    const user = userEvent.setup();
    const api = await loadProfileWith(user, [carousel("222", [])]);

    vi.mocked(api.analyzeUrl).mockRejectedValueOnce({
      kind: "unavailable_content",
      code: "unavailable_content",
      message: "the page did not finish loading",
    });
    await user.click(chooser("222"));
    expect(await within(card("222")).findByText(/could not be read just now/)).toBeInTheDocument();

    // Trying again does ask again — that is the whole point of the button.
    vi.mocked(api.analyzeUrl).mockResolvedValueOnce({
      intent: "post",
      post: { id: "222" },
      media: [picture("m1", "222")],
    } as never);
    await user.click(within(card("222")).getByRole("button", { name: "Try again" }));
    expect(await within(card("222")).findByText(/1 of 1 selected/)).toBeInTheDocument();
    expect(api.analyzeUrl).toHaveBeenCalledTimes(2);
  });

  it("keeps a carousel's selection while another screen is shown", async () => {    const user = userEvent.setup();
    await loadProfileWith(user, [
      carousel("222", [picture("m1", "222"), picture("m2", "222"), picture("m3", "222")]),
    ]);

    await user.click(chooser("222"));
    expect(await within(card("222")).findByText(/3 of 3 selected/)).toBeInTheDocument();
    await user.click(within(card("222")).getByRole("checkbox", { name: "Image 2" }));
    expect(await within(card("222")).findByText(/2 of 3 selected/)).toBeInTheDocument();

    await user.click(tab("Downloads"));
    await user.click(tab("Profile Downloader"));

    // The chooser is still open on the same post with the same choice.
    expect(await within(card("222")).findByText(/2 of 3 selected/)).toBeInTheDocument();
  });

  it("refreshes from the source on request, keeping the choice already made", async () => {
    // The one case the copy on file is not enough for. This is a deliberate act,
    // so it is the one thing on this screen allowed to start a browser render —
    // and it must not undo the user's selection on the way.
    const user = userEvent.setup();
    const api = await loadProfileWith(user, [
      carousel("222", [picture("c1", "222"), picture("c2", "222")]),
    ]);
    vi.mocked(api.analyzeUrl).mockClear();

    await user.click(chooser("222"));
    expect(await within(card("222")).findByText(/2 of 2 selected/)).toBeInTheDocument();
    await user.click(within(card("222")).getByRole("checkbox", { name: "Image 1" }));
    expect(await within(card("222")).findByText(/1 of 2 selected/)).toBeInTheDocument();

    // The source now has three, one of them the same picture as before.
    vi.mocked(api.analyzeUrl).mockResolvedValueOnce({
      intent: "post",
      post: { id: "222" },
      media: [picture("c1", "222"), picture("c2", "222"), picture("c3", "222")],
    } as never);

    await user.click(within(card("222")).getByRole("button", { name: "Refresh from source" }));

    expect(api.analyzeUrl).toHaveBeenCalledTimes(1);
    // Three pictures now; c1 stays deselected because that was the choice, and
    // the two that were not there before join as chosen.
    expect(await within(card("222")).findByText(/2 of 3 selected/)).toBeInTheDocument();
    expect(
      within(card("222")).queryByText(/from the copy already on file/),
    ).not.toBeInTheDocument();
  });
});

/**
 * A post discovered through a profile listing that turns out to be words.
 *
 * The listing cannot say. Douyin draws a video card and a note card, and a note
 * card looks identical whether it holds ten pictures or one sentence — so a
 * profile row could offer no text export at all, and a post the application can
 * perfectly well read had no way out of that screen.
 */
describe("a profile post that turns out to be text", () => {
  function listed(id: string, caption: string) {
    const base = profilePost(id);
    return {
      ...base,
      post: {
        ...base.post,
        mediaType: "carousel" as const,
        platformPostId: id,
        caption,
        // No stored media, and no thumbnail: the same shape an unresolved note
        // has, which is exactly why the row cannot decide this for itself.
        mediaItems: [],
        thumbnailUrl: null,
      },
    };
  }

  /** What the resolver returns for a caption-only post. */
  function resolvedText(text: string) {
    return {
      intent: "post",
      classification: { label: "Post", kind: "post", original: "u", canonical: "u" },
      post: {
        id: "222",
        platform: "douyin",
        platformPostId: "7629996475679029850",
        mediaType: "text",
        caption: text,
        canonicalUrl: "https://www.douyin.com/note/7629996475679029850",
        metadata: { text, publishedAt: "2026-04-18T15:23:11" },
      },
      creator: null,
      media: [],
      resolutionStage: "resolved",
      resolvedVia: "browser",
    } as never;
  }

  async function load(user: ReturnType<typeof userEvent.setup>, caption: string) {
    const { api } = await import("@/services/api");
    vi.mocked(api.listProfilePosts).mockResolvedValueOnce({
      creator: { id: "creator-1" },
      posts: [listed("222", caption)],
      nextCursor: null,
      hasMore: false,
      totalKnown: 1,
      loadedTotal: 1,
    } as never);
    render(<App />);
    await user.click(tab("Profile Downloader"));
    await user.type(
      await screen.findByLabelText(/Douyin profile link/i),
      "https://v.douyin.com/SMt7NsUPaGo/",
    );
    await user.click(screen.getByRole("button", { name: "Load profile" }));
    await waitFor(() => expect(screen.getAllByText(caption)).toHaveLength(1));
    return api;
  }

  beforeEach(async () => {
    const { api } = await import("@/services/api");
    vi.mocked(api.analyzeUrl).mockReset();
    vi.mocked(api.listProfilePosts).mockReset();
    vi.mocked(api.submitProfileDownloads).mockClear();
    vi.mocked(api.savePostText).mockClear();
  });

  it("reports the type it found, and offers to save the words", async () => {
    const user = userEvent.setup();
    const api = await load(user, "a caption that says nothing about the type");
    vi.mocked(api.analyzeUrl).mockResolvedValueOnce(resolvedText("开机大吉！顺顺利利！"));

    await user.click(screen.getByRole("button", { name: "Choose images" }));

    // The resolved type, not the listing's guess. Scoped to the row, because
    // the type filter's menu also has an option called "Text".
    const row = (await screen.findByText("开机大吉！顺顺利利！")).closest("div")!
      .parentElement!.parentElement as HTMLElement;
    expect(within(row).getByText("Text")).toBeInTheDocument();
    expect(within(row).getByText("开机大吉！顺顺利利！")).toBeInTheDocument();
    expect(within(row).getByText(/2026-04-18T15:23:11/)).toBeInTheDocument();
    expect(within(row).getByRole("button", { name: "Save text" })).toBeEnabled();
    // And nothing offers to choose pictures out of it.
    expect(within(row).queryByRole("checkbox", { name: "Image 1" })).not.toBeInTheDocument();
  });

  it("saves the words without queueing a download or recording one", async () => {
    const user = userEvent.setup();
    const api = await load(user, "a caption that says nothing about the type");
    vi.mocked(api.analyzeUrl).mockResolvedValueOnce(resolvedText("开机大吉！顺顺利利！"));
    vi.mocked(api.savePostText).mockResolvedValueOnce({
      postId: "222",
      path: "D:/Downloads/Douyin/郭虎/Text/7629996475679029850.txt",
      bytes: 176,
      renamed: false,
    });

    await user.click(screen.getByRole("button", { name: "Choose images" }));
    await user.click(await screen.findByRole("button", { name: "Save text" }));

    await waitFor(() => expect(api.savePostText).toHaveBeenCalledWith("222"));
    // The file it wrote, named with the creator's own characters intact.
    expect(await screen.findByText(/郭虎\/Text\/7629996475679029850\.txt/)).toBeInTheDocument();
    // And nothing was downloaded: no task, no queue, no history.
    expect(api.submitProfileDownloads).not.toHaveBeenCalled();
  });

  it("says when a save failed instead of claiming it worked", async () => {
    const user = userEvent.setup();
    const api = await load(user, "a caption that says nothing about the type");
    vi.mocked(api.analyzeUrl).mockResolvedValueOnce(resolvedText("开机大吉！顺顺利利！"));
    vi.mocked(api.savePostText).mockRejectedValueOnce({
      kind: "storage",
      code: "storage",
      message: "the text could not be saved to the download folder",
    });

    await user.click(screen.getByRole("button", { name: "Choose images" }));
    await user.click(await screen.findByRole("button", { name: "Save text" }));

    expect(await screen.findByText(/could not be saved to the download folder/)).toBeInTheDocument();
    expect(screen.queryByText(/Saved to/)).not.toBeInTheDocument();
  });

  it("does not call a post text merely because the listing showed no thumbnail", async () => {
    // The trap the whole design avoids. A missing thumbnail is also what an
    // incomplete render, an unsupported post and a failed resolution look like,
    // and turning any of those into an empty text file would be this screen
    // inventing content the source never gave.
    const user = userEvent.setup();
    const api = await load(user, "a caption that says nothing about the type");
    vi.mocked(api.analyzeUrl).mockResolvedValueOnce({
      intent: "post",
      classification: { label: "Post", kind: "post", original: "u", canonical: "u" },
      post: { id: "222", platform: "douyin", platformPostId: "1", mediaType: "video" },
      creator: null,
      media: [],
      resolutionStage: "resolved",
      resolvedVia: "browser",
    } as never);

    await user.click(screen.getByRole("button", { name: "Choose images" }));

    expect(await screen.findByText(/downloads as itself/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Save text" })).not.toBeInTheDocument();
  });

  it("stops counting a post it has found to be words as one to download", async () => {
    // "Download selected" is an offer to queue downloads. A post this screen has
    // already read and found to hold nothing but words has nothing to queue, so
    // the count drops rather than the button promising a download that cannot
    // happen. The row itself stays exactly where it is.
    const user = userEvent.setup();
    const api = await load(user, "a caption that says nothing about the type");
    vi.mocked(api.analyzeUrl).mockResolvedValueOnce(resolvedText("开机大吉！顺顺利利！"));

    await user.click(screen.getByRole("checkbox", { name: /a caption that says nothing/ }));
    expect(screen.getByRole("button", { name: "Download selected" })).toBeEnabled();

    await user.click(screen.getByRole("button", { name: "Choose images" }));
    await screen.findByRole("button", { name: "Save text" });

    // The button stops offering once there is nothing it could queue.
    expect(await screen.findByRole("button", { name: "Download selected" })).toBeDisabled();
  });

  it("keeps the row and offers to try again when the read fails", async () => {
    const user = userEvent.setup();
    const api = await load(user, "a caption that says nothing about the type");
    vi.mocked(api.analyzeUrl).mockRejectedValueOnce({
      kind: "unavailable_content",
      code: "unavailable_content",
      message: "the page did not finish loading",
    });

    await user.click(screen.getByRole("button", { name: "Choose images" }));

    expect(await screen.findByText(/could not be read just now/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Try again" })).toBeInTheDocument();
    // The post itself is still on screen, with its status untouched.
    expect(screen.getByText("a caption that says nothing about the type")).toBeInTheDocument();
  });
});

/**
 * The profile field on a fresh visit, which had the same problem the link field
 * on Single URL had: it arrived holding one of the project's own test profile
 * links, fetched on mount, so opening the screen looked at a stranger's profile
 * before the user had done anything.
 */
describe("the profile field on a fresh visit", () => {
  it("starts empty, says what to paste, and asks for nothing", async () => {
    const user = userEvent.setup();
    const { api } = await import("@/services/api");
    vi.mocked(api.resolveProfile).mockClear();
    // The fixtures are offered rather than empty, so "the field is empty" is a
    // statement about this screen and not an accident of the mock having
    // nothing to prefill it with.
    getTestFixtures.mockResolvedValueOnce([
      {
        url: "https://v.douyin.com/SMt7NsUPaGo/",
        intent: "profile",
        label: "Profile",
        expectedKind: "profile",
      },
    ]);

    render(<App />);
    await user.click(tab("Profile Downloader"));

    const input = (await screen.findByLabelText(/Douyin profile link/i)) as HTMLInputElement;

    // Let any mount-time work settle before reading the field. A prefill
    // arrives on a later tick than the first paint, so reading the value
    // immediately would pass whether or not one was coming.
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });

    expect(input.value).toBe("");
    expect(input).toHaveAttribute("placeholder", "Paste a Douyin profile link");

    // Opening the screen resolves no profile, and does not even go looking for
    // the project's fixture links.
    expect(api.resolveProfile).not.toHaveBeenCalled();
    expect(getTestFixtures).not.toHaveBeenCalled();
  });

  it("loads only the profile the user supplied, once they ask", async () => {
    const user = userEvent.setup();
    const { api } = await import("@/services/api");
    vi.mocked(api.resolveProfile).mockClear();
    vi.mocked(api.listProfilePosts).mockClear();

    render(<App />);
    await user.click(tab("Profile Downloader"));

    const link = "https://www.douyin.com/user/MS4wLjABAAAA_guohu";
    await user.type(await screen.findByLabelText(/Douyin profile link/i), link);
    // Typing alone starts nothing.
    expect(api.resolveProfile).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "Load profile" }));
    await waitFor(() => expect(api.resolveProfile).toHaveBeenCalledTimes(1));
    expect(api.resolveProfile).toHaveBeenCalledWith(link);
  });
});
