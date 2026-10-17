/**
 * The picture gallery on the Single URL screen.
 *
 * A carousel used to be fifteen separate errands: one item row per picture, one
 * Download press each, and every slide named after the post so the download
 * layer's `(1)` collision suffix was the only thing telling them apart. What is
 * pinned here is that a carousel is one action, that a batch which half-succeeds
 * says so per slide, and that a failed slide can be retried without touching the
 * ones that worked.
 */

import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { act } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { SingleUrlPage } from "@/pages/SingleUrlPage";
import { useDownloadStore } from "@/stores/useDownloadStore";
import type { AnalyzedUrl, DownloadTask, MediaItem, TestFixture } from "@/types";

const analyzeUrl = vi.fn();
const createDownloadTask = vi.fn();
const retryDownload = vi.fn();
const savePostText = vi.fn();
/**
 * The project's fixture links.
 *
 * Kept as a named mock so a test can assert this screen never asks for them:
 * the field used to be prefilled from here on mount, which put one of the
 * project's own test links in the box before the user had done anything.
 */
const getTestFixtures = vi.fn(async (): Promise<TestFixture[]> => []);

vi.mock("@/services/api", () => ({
  api: {
    getTestFixtures: () => getTestFixtures(),
    analyzeUrl: (url: string) => analyzeUrl(url),
    createDownloadTask: (input: unknown) => createDownloadTask(input),
    retryDownload: (id: string) => retryDownload(id),
    cancelDownload: vi.fn(async () => null),
    savePostText: (id: string) => savePostText(id),
  },
}));

/** One picture of a post, as the resolver reports one. */
function image(index: number, extension = "webp"): MediaItem {
  return {
    id: `item-${index}`,
    postId: "post-1",
    mediaType: "image",
    sourceUrl: `https://p3-cdn.douyinpic.com/slide-${index}.${extension}`,
    thumbnailUrl: `https://p3-cdn.douyinpic.com/slide-${index}.${extension}`,
    mimeType: "image/webp",
    extension,
    width: 1080,
    height: 1440,
    durationSeconds: null,
    fileSize: null,
    variants: [],
    metadata: { slide: index },
  };
}

function carousel(count: number): AnalyzedUrl {
  return {
    intent: "post",
    classification: { label: "Post", kind: "post", original: "u", canonical: "u" },
    post: {
      id: "post-1",
      platform: "douyin",
      platformPostId: "7629996475679029850",
      mediaType: "carousel",
    },
    creator: null,
    media: Array.from({ length: count }, (_, index) => image(index + 1)),
    resolutionStage: "resolved",
    resolvedVia: "browser",
  } as unknown as AnalyzedUrl;
}

function task(id: string, mediaItemId: string, state: DownloadTask["state"]): DownloadTask {
  return {
    id,
    postId: "post-1",
    mediaItemId,
    variantId: null,
    sourceUrl: "https://cdn/a.webp",
    destinationPath: null,
    fileName: null,
    extension: "webp",
    mediaType: "image",
    state,
    attempts: 1,
    maxAttempts: 3,
    progress: { downloadedBytes: 0, totalBytes: 0 },
    error: null,
    createdAt: "",
    updatedAt: "",
    startedAt: null,
    completedAt: null,
  } as unknown as DownloadTask;
}

async function analyzeCarousel(user: ReturnType<typeof userEvent.setup>, count: number) {
  analyzeUrl.mockResolvedValueOnce(carousel(count));
  await user.type(screen.getByLabelText(/Douyin link/i), "https://v.douyin.com/abc/");
  await user.click(screen.getByRole("button", { name: "Analyze" }));
  await screen.findByText(`Image 1`);
}

beforeEach(() => {
  analyzeUrl.mockReset();
  getTestFixtures.mockClear();
  createDownloadTask.mockReset().mockImplementation(async (input: { mediaItemId: string }) =>
    task(`task-${input.mediaItemId}`, input.mediaItemId, "queued"),
  );
  retryDownload.mockReset();
  savePostText.mockReset();
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
});

describe("a post whose media is pictures", () => {
  it("shows every picture once, in the source's order", async () => {
    const user = userEvent.setup();
    render(<SingleUrlPage />);
    await analyzeCarousel(user, 3);

    const gallery = screen.getByRole("list");
    const labels = within(gallery)
      .getAllByText(/^Image \d+$/)
      .map((node) => node.textContent);
    expect(labels).toEqual(["Image 1", "Image 2", "Image 3"]);

    // Source order, not sorted or shuffled.
    const sources = within(gallery)
      .getAllByRole("img")
      .map((node) => node.getAttribute("src"));
    expect(sources).toEqual([
      "https://p3-cdn.douyinpic.com/slide-1.webp",
      "https://p3-cdn.douyinpic.com/slide-2.webp",
      "https://p3-cdn.douyinpic.com/slide-3.webp",
    ]);
  });

  it("starts with every picture chosen, and can clear and restore them", async () => {
    const user = userEvent.setup();
    render(<SingleUrlPage />);
    await analyzeCarousel(user, 3);

    expect(screen.getByRole("button", { name: /Download selected \(3\)/ })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Deselect all" }));
    expect(screen.getByRole("button", { name: /Download selected \(0\)/ })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Select all" }));
    expect(screen.getByRole("button", { name: /Download selected \(3\)/ })).toBeInTheDocument();
  });

  it("queues one task per chosen picture", async () => {
    const user = userEvent.setup();
    render(<SingleUrlPage />);
    await analyzeCarousel(user, 3);

    // Unchoose the middle one, so the batch is not simply "everything".
    await user.click(screen.getByRole("checkbox", { name: /^Image 2,/ }));
    await user.click(screen.getByRole("button", { name: /Download selected \(2\)/ }));

    await waitFor(() => expect(createDownloadTask).toHaveBeenCalledTimes(2));
    const asked = createDownloadTask.mock.calls.map(
      (call) => (call[0] as { mediaItemId: string }).mediaItemId,
    );
    expect(asked).toEqual(["item-1", "item-3"]);
  });

  it("says which slides failed without hiding the ones that worked", async () => {
    const user = userEvent.setup();
    render(<SingleUrlPage />);
    await analyzeCarousel(user, 3);

    // The store already knows about two of them: one downloaded, one failed.
    act(() => {
      useDownloadStore.setState({
        tasks: {
          "task-item-1": task("task-item-1", "item-1", "completed"),
          "task-item-2": task("task-item-2", "item-2", "failed"),
        },
        order: ["task-item-1", "task-item-2"],
        stateRevision: 1,
      });
    });

    expect(await screen.findByText("Downloaded")).toBeInTheDocument();
    expect(await screen.findByText("Failed")).toBeInTheDocument();
    // The third picture has no task at all, and says nothing rather than
    // borrowing a neighbour's state.
    const gallery = screen.getByRole("list");
    expect(within(gallery).getByRole("button", { name: /Retry image 2/i })).toBeInTheDocument();
  });

  it("retries only the slide that failed", async () => {
    const user = userEvent.setup();
    retryDownload.mockResolvedValueOnce(task("task-item-2", "item-2", "queued"));
    render(<SingleUrlPage />);
    await analyzeCarousel(user, 3);

    act(() => {
      useDownloadStore.setState({
        tasks: {
          "task-item-1": task("task-item-1", "item-1", "completed"),
          "task-item-2": task("task-item-2", "item-2", "failed"),
        },
        order: ["task-item-1", "task-item-2"],
        stateRevision: 1,
      });
    });

    await user.click(await screen.findByRole("button", { name: /Retry image 2/i }));

    expect(retryDownload).toHaveBeenCalledTimes(1);
    expect(retryDownload).toHaveBeenCalledWith("task-item-2");
  });

  it("does not rebuild the gallery for a progress frame", async () => {
    const user = userEvent.setup();
    render(<SingleUrlPage />);
    await analyzeCarousel(user, 2);

    const before = screen.getAllByRole("img")[0];
    act(() => {
      // A progress frame: the task map is replaced, the state is not.
      useDownloadStore.setState({
        tasks: { "task-item-1": task("task-item-1", "item-1", "downloading") },
        order: ["task-item-1"],
      });
    });

    // Same element instance: the grid was not thrown away and rebuilt.
    expect(screen.getAllByRole("img")[0]).toBe(before);
  });
});

/**
 * A post whose content is its words.
 *
 * Douyin publishes caption-only posts. They used to arrive as "this post does
 * not contain downloadable media" — true, and useless: the user sees a post the
 * application will not do anything with and no reason why. They are text posts,
 * and the thing to do with one is keep the words.
 */
describe("a text post", () => {
  function textPost(overrides: Record<string, unknown> = {}): AnalyzedUrl {
    return {
      intent: "post",
      classification: { label: "Post", kind: "post", original: "u", canonical: "u" },
      post: {
        id: "post-1",
        platform: "douyin",
        platformPostId: "7629996475679029855",
        mediaType: "text",
        caption: "开机大吉！顺顺利利！",
        canonicalUrl: "https://www.douyin.com/note/7629996475679029855",
        metadata: { text: "开机大吉！顺顺利利！", publishedAt: "2026-04-18T15:23:11" },
        ...overrides,
      },
      creator: null,
      media: [],
      resolutionStage: "resolved",
      resolvedVia: "browser",
    } as unknown as AnalyzedUrl;
  }

  async function analyzeText(user: ReturnType<typeof userEvent.setup>, result: AnalyzedUrl) {
    analyzeUrl.mockResolvedValueOnce(result);
    await user.type(screen.getByLabelText(/Douyin link/i), "https://v.douyin.com/abc/");
    await user.click(screen.getByRole("button", { name: "Analyze" }));
    await screen.findByText("Text post");
  }

  it("shows the words rather than reporting a post nothing can be done with", async () => {
    const user = userEvent.setup();
    render(<SingleUrlPage />);
    await analyzeText(user, textPost());

    // The card, not the preview above it: the preview shows the caption too, and
    // this is about what the text card itself does with the post.
    const card = screen.getByText("Text post").closest("section")!;
    expect(within(card).getByText("开机大吉！顺顺利利！")).toBeInTheDocument();
    // The publish time exactly as the source stated it, and no timezone invented.
    expect(within(card).getByText(/2026-04-18T15:23:11/)).toBeInTheDocument();

    // Nothing to download, so nothing offers to download it.
    expect(within(card).queryByRole("button", { name: /Download/ })).not.toBeInTheDocument();
    expect(within(card).getByRole("button", { name: "Save text" })).toBeEnabled();
  });

  it("saves the text and says where it went", async () => {
    const user = userEvent.setup();
    savePostText.mockResolvedValueOnce({
      postId: "post-1",
      path: "D:/Downloads/Text/7629996475679029855.txt",
      bytes: 128,
      renamed: false,
    });
    render(<SingleUrlPage />);
    await analyzeText(user, textPost());

    await user.click(screen.getByRole("button", { name: "Save text" }));

    await waitFor(() => expect(savePostText).toHaveBeenCalledWith("post-1"));
    expect(await screen.findByText(/7629996475679029855\.txt/)).toBeInTheDocument();
  });

  it("says when an existing file was in the way rather than reporting a plain save", async () => {
    const user = userEvent.setup();
    savePostText.mockResolvedValueOnce({
      postId: "post-1",
      path: "D:/Downloads/Text/7629996475679029855 (1).txt",
      bytes: 128,
      renamed: true,
    });
    render(<SingleUrlPage />);
    await analyzeText(user, textPost());

    await user.click(screen.getByRole("button", { name: "Save text" }));

    expect(await screen.findByText(/named beside it/)).toBeInTheDocument();
  });

  it("reports a failed save instead of claiming one", async () => {
    const user = userEvent.setup();
    savePostText.mockRejectedValueOnce({
      kind: "storage",
      code: "storage",
      message: "the post's text could not be written",
    });
    render(<SingleUrlPage />);
    await analyzeText(user, textPost());

    await user.click(screen.getByRole("button", { name: "Save text" }));

    expect(await screen.findByText(/could not be written/)).toBeInTheDocument();
    expect(screen.queryByText(/Saved to/)).not.toBeInTheDocument();
  });

  it("does not offer to save nothing at all", async () => {
    const user = userEvent.setup();
    render(<SingleUrlPage />);
    await analyzeText(
      user,
      textPost({ caption: null, metadata: {}, }),
    );

    expect(screen.getByText(/no text could be read from it/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save text" })).toBeDisabled();
  });
});

/**
 * A post with music behind its pictures.
 *
 * The credit and the file are two different facts. The source names the track
 * far more often than it hands over one, and a screen that treats the credit as
 * a download offers the user a file that does not exist; a screen that treats a
 * missing URL as "this post has no music" throws away something the source said.
 */
describe("a post with a soundtrack", () => {
  function postWithSoundtrack(soundtrack: Record<string, unknown> | null): AnalyzedUrl {
    return {
      intent: "post",
      classification: { label: "Post", kind: "post", original: "u", canonical: "u" },
      post: {
        id: "post-1",
        platform: "douyin",
        platformPostId: "7629996475679029860",
        mediaType: "carousel",
        caption: "两张照片",
        metadata: soundtrack ? { soundtrack } : {},
      },
      creator: null,
      media: [image(1), image(2)],
      resolutionStage: "resolved",
      resolvedVia: "browser",
    } as unknown as AnalyzedUrl;
  }

  async function analyze(user: ReturnType<typeof userEvent.setup>, result: AnalyzedUrl) {
    analyzeUrl.mockResolvedValueOnce(result);
    await user.type(screen.getByLabelText(/Douyin link/i), "https://v.douyin.com/abc/");
    await user.click(screen.getByRole("button", { name: "Analyze" }));
    await screen.findByText("Image 1");
  }

  it("credits a track that cannot be downloaded, without offering a file", async () => {
    const user = userEvent.setup();
    render(<SingleUrlPage />);
    await analyze(
      user,
      postWithSoundtrack({
        title: "背景音乐",
        author: "某歌手",
        durationSeconds: 30,
        downloadable: false,
      }),
    );

    expect(screen.getByText(/背景音乐 — 某歌手/)).toBeInTheDocument();
    expect(screen.getByText(/no downloadable audio/)).toBeInTheDocument();
    // Only the two pictures are offered: no audio task was invented for a track
    // the source did not provide.
    expect(screen.getByRole("button", { name: "Download selected (2)" })).toBeInTheDocument();
    expect(screen.getAllByRole("checkbox")).toHaveLength(2);
  });

  it("does not claim there is no audio when the source did hand one over", async () => {
    const user = userEvent.setup();
    render(<SingleUrlPage />);
    await analyze(
      user,
      postWithSoundtrack({
        title: "Original sound",
        author: "郭虎",
        durationSeconds: 15,
        downloadable: true,
      }),
    );

    expect(screen.getByText(/Original sound — 郭虎/)).toBeInTheDocument();
    expect(screen.queryByText(/no downloadable audio/)).not.toBeInTheDocument();
  });

  it("says nothing at all when the source said nothing", async () => {
    const user = userEvent.setup();
    render(<SingleUrlPage />);
    await analyze(user, postWithSoundtrack(null));

    expect(screen.queryByText(/Soundtrack/)).not.toBeInTheDocument();
  });
});

/**
 * The link field on a fresh visit.
 *
 * The field used to arrive holding one of the project's own test links, fetched
 * from the backend on mount: the user was shown a stranger's video before doing
 * anything, every visit woke the resolver, and a fresh page was never actually
 * fresh. What is pinned here is the whole of the replacement behaviour — empty
 * on arrival, nothing requested without the user asking, and the request that
 * does run carrying exactly the link the user supplied.
 */
describe("the link field on a fresh visit", () => {
  it("starts empty, and says what to paste", async () => {
    // The fixtures are offered rather than empty, so "the field is empty" is a
    // statement about this screen and not an accident of the mock having
    // nothing to prefill it with.
    getTestFixtures.mockResolvedValueOnce([
      {
        url: "https://v.douyin.com/7yyhouNSzvA/",
        intent: "post",
        label: "Video post",
        expectedKind: "post",
      },
    ]);

    render(<SingleUrlPage />);

    // Let any mount-time work settle before reading the field. A prefill
    // arrives on a later tick than the first paint, so reading the value
    // immediately after render would pass whether or not one was coming.
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });

    const input = screen.getByLabelText(/Douyin link/i) as HTMLInputElement;
    expect(input.value).toBe("");
    expect(input).toHaveAttribute("placeholder", "Paste a Douyin video or post link");
  });

  it("asks for nothing at all until the user submits a link", async () => {
    const user = userEvent.setup();
    render(<SingleUrlPage />);

    // Rendering the screen starts no resolution, and does not even go looking
    // for the project's fixture links.
    expect(analyzeUrl).not.toHaveBeenCalled();
    expect(getTestFixtures).not.toHaveBeenCalled();

    // Submitting an empty box is not a request either.
    await user.click(screen.getByRole("button", { name: "Analyze" }));
    expect(analyzeUrl).not.toHaveBeenCalled();

    // Typing alone still starts nothing: the user has not asked yet.
    await user.type(
      screen.getByLabelText(/Douyin link/i),
      "https://www.douyin.com/video/7281234567890123456",
    );
    expect(analyzeUrl).not.toHaveBeenCalled();
  });

  it("resolves exactly the link the user supplied, once they ask", async () => {
    const user = userEvent.setup();
    analyzeUrl.mockResolvedValueOnce(carousel(2));
    render(<SingleUrlPage />);

    const link = "https://www.douyin.com/video/7281234567890123456";
    await user.type(screen.getByLabelText(/Douyin link/i), link);
    await user.click(screen.getByRole("button", { name: "Analyze" }));

    await waitFor(() => expect(analyzeUrl).toHaveBeenCalledTimes(1));
    // The user's own link, not a fixture and not something prefilled.
    expect(analyzeUrl).toHaveBeenCalledWith(link);
    expect(await screen.findByText("Image 1")).toBeInTheDocument();
  });

  it("still reports a link the resolver will not accept", async () => {
    const user = userEvent.setup();
    analyzeUrl.mockRejectedValueOnce({
      kind: "validation",
      code: "validation",
      message: "That is not a Douyin link.",
    });
    render(<SingleUrlPage />);

    await user.type(screen.getByLabelText(/Douyin link/i), "not a link at all");
    await user.click(screen.getByRole("button", { name: "Analyze" }));

    // The field marks itself invalid and the message is shown; both the field
    // error and the notice carry it, which is why this counts rather than
    // looks for exactly one.
    expect(await screen.findAllByText("That is not a Douyin link.")).not.toHaveLength(0);
    expect(screen.getByLabelText(/Douyin link/i)).toHaveAttribute("aria-invalid", "true");
  });
});
