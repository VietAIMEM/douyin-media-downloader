/**
 * History.
 *
 * The screen's job is the one thing the Downloads screen does not do: it is a
 * record of outcomes. Everything here has already happened, so nothing on it is
 * a control over work in flight — no cancel, no pause, no progress. What these
 * tests pin is that it stays that way, and that a failure is explained rather
 * than merely counted.
 */

import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";

const listHistory = vi.fn();
const getHistoryCount = vi.fn();
const openDownloadPath = vi.fn();

vi.mock("@/services/api", () => ({
  api: {
    listHistory: (limit?: number) => listHistory(limit),
    getHistoryCount: () => getHistoryCount(),
    openDownloadPath: (taskId: string, kind: "file" | "folder") =>
      openDownloadPath(taskId, kind),
    scanDownloadedFiles: vi.fn(async () => ({
      root: "D:/Downloads/Douyin",
      scannedAt: "2026-01-01T00:00:00Z",
      entries: [],
    })),
  },
}));

vi.mock("react-router-dom", async () => {
  const actual = await vi.importActual<typeof import("react-router-dom")>("react-router-dom");
  return { ...actual, useLocation: () => ({ pathname: "/history" }) };
});

import { HistoryPage } from "@/pages/HistoryPage";
import type { DownloadHistoryEntry } from "@/types";

function entry(overrides: Partial<DownloadHistoryEntry> = {}): DownloadHistoryEntry {
  return {
    id: "h1",
    taskId: "task-1",
    postId: "post-1",
    postUrl: "https://www.douyin.com/video/7300000000000000001",
    creatorId: null,
    creatorName: "郭虎",
    mediaType: "video",
    sourceUrl: "https://cdn.test/a.mp4",
    localPath: "D:\\Downloads\\Douyin\\郭虎\\Videos\\clip.mp4",
    fileSize: 4096,
    status: "completed",
    error: null,
    downloadedAt: "2026-10-15T10:00:00Z",
    ...overrides,
  };
}

beforeEach(() => {
  listHistory.mockReset().mockResolvedValue([entry()]);
  getHistoryCount.mockReset().mockResolvedValue(1);
  openDownloadPath.mockReset().mockResolvedValue(undefined);
});

it("leads a row with the file the download produced, not the URL", async () => {
  render(<HistoryPage />);

  // The name a person recognizes, out of the path.
  expect(await screen.findByText("clip.mp4")).toBeInTheDocument();
  // The source stays, as context rather than as the headline.
  expect(screen.getByText(/www\.douyin\.com\/video/)).toBeInTheDocument();
});

it("offers no control over work in flight, because none of it is in flight", async () => {
  render(<HistoryPage />);
  await screen.findByText("clip.mp4");

  // History is a record. The queue controls belong on the screen that owns the
  // queue, and offering them here would be offering them over a finished thing.
  for (const action of ["Cancel", "Pause", "Resume", "Retry"]) {
    expect(screen.queryByRole("button", { name: action })).not.toBeInTheDocument();
  }
});

it("makes a failure understandable rather than merely present", async () => {
  listHistory.mockResolvedValue([
    entry({
      id: "h2",
      status: "failed",
      localPath: null,
      error: "the connection dropped after 3 attempts",
    }),
  ]);
  getHistoryCount.mockResolvedValue(1);

  render(<HistoryPage />);

  expect(await screen.findByText("Failed")).toBeInTheDocument();
  expect(screen.getByText(/connection dropped after 3 attempts/)).toBeInTheDocument();
  // A failure produced no file, so nothing offers to open one.
  expect(screen.queryByRole("button", { name: "Open file" })).not.toBeInTheDocument();
});

it("opens a finished download's file and its folder", async () => {
  const user = userEvent.setup();
  render(<HistoryPage />);
  await screen.findByText("clip.mp4");

  await user.click(screen.getByRole("button", { name: "Open file" }));
  await waitFor(() => expect(openDownloadPath).toHaveBeenCalledWith("task-1", "file"));

  await user.click(screen.getByRole("button", { name: "Open folder" }));
  await waitFor(() => expect(openDownloadPath).toHaveBeenCalledWith("task-1", "folder"));
});

it("reports how many succeeded and how many did not", async () => {
  listHistory.mockResolvedValue([
    entry({ id: "h1", status: "completed" }),
    entry({ id: "h2", status: "failed", taskId: "task-2", localPath: null }),
    entry({ id: "h3", status: "cancelled", taskId: "task-3", localPath: null }),
  ]);
  getHistoryCount.mockResolvedValue(3);

  render(<HistoryPage />);

  const description = await screen.findByText(/succeeded · .* failed or cancelled/);
  expect(description.textContent).toContain("1 succeeded");
  expect(description.textContent).toContain("2 failed or cancelled");
});

it("says nothing has been recorded rather than showing an empty list", async () => {
  listHistory.mockResolvedValue([]);
  getHistoryCount.mockResolvedValue(0);

  render(<HistoryPage />);

  expect(await screen.findByText("Nothing has been downloaded yet")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Open file" })).not.toBeInTheDocument();
});

it("reports a failed read instead of reporting an empty history", async () => {
  listHistory.mockRejectedValue({
    kind: "database_error",
    code: "database_error",
    message: "The history could not be read.",
  });

  render(<HistoryPage />);

  expect(await screen.findByText(/could not be read/)).toBeInTheDocument();
  expect(screen.queryByText("Nothing has been downloaded yet")).not.toBeInTheDocument();
});

it("lets a row without an originating task be read but not opened", async () => {
  // Older records predate the task link. The row still describes what happened;
  // it simply cannot act on it, and says so by offering nothing.
  listHistory.mockResolvedValue([entry({ taskId: null })]);
  getHistoryCount.mockResolvedValue(1);

  render(<HistoryPage />);

  const row = (await screen.findByText("clip.mp4")).closest("li") as HTMLElement;
  expect(within(row).getByText("Completed")).toBeInTheDocument();
  expect(within(row).queryByRole("button", { name: "Open file" })).not.toBeInTheDocument();
});

/**
 * A row says its address once.
 *
 * Found by looking at the running application: a failed download has no file to
 * name, so the row falls back to the post's URL as its headline — and the
 * context line under it printed the same URL again. Completed rows, whose
 * headline is the file, were unaffected.
 */
it("does not print a post's URL twice when it is already the row's headline", async () => {
  listHistory.mockResolvedValue([
    entry({ id: "h9", taskId: "task-9", status: "failed", localPath: null, error: "it failed" }),
  ]);
  getHistoryCount.mockResolvedValue(1);

  render(<HistoryPage />);

  const row = (await screen.findByText("Failed")).closest("li") as HTMLElement;
  const shown = row.textContent?.match(/douyin\.com\/video/g) ?? [];
  // Once. The headline is the address; repeating it underneath says nothing new.
  expect(shown).toHaveLength(1);
});

it("keeps naming the creator and source when the headline is a file", async () => {
  render(<HistoryPage />);
  await screen.findByText("clip.mp4");

  const row = screen.getByText("clip.mp4").closest("li") as HTMLElement;
  // A file's name says nothing about where it came from, so the context stays.
  expect(row.textContent).toContain("@郭虎");
  expect(row.textContent).toMatch(/douyin\.com\/video/);
});
