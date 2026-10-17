/**
 * The Downloads screen with a queue in it.
 *
 * Found by using the installed application, not by a test: this screen stopped
 * rendering as soon as it had a task to show, and the error boundary caught it
 * with "Maximum update depth exceeded". The cause was a store selector that
 * built a new array on every call — React's external-store subscription treats
 * a changed value as a reason to render again, so it rendered for ever.
 *
 * The existing application test visited this screen with an empty queue and
 * therefore never saw it, which is why the queue here is populated.
 */

import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@/services/api", () => ({
  api: {
    getDownloadQueue: vi.fn(async () => ({ tasks: [], summary: null })),
    pauseDownload: vi.fn(),
    resumeDownload: vi.fn(),
    cancelDownload: vi.fn(),
    retryDownload: vi.fn(),
    removeDownload: vi.fn(),
    retryFailedDownloads: vi.fn(),
    clearCompletedDownloads: vi.fn(),
    openDownloadPath: vi.fn(),
    // The screen asks the filesystem which recorded files are still there when
    // it opens. Nothing is on disk in this test, and no file state is the honest
    // answer for a queue that holds none.
    scanDownloadedFiles: vi.fn(async () => ({
      root: "D:\Downloads\Douyin",
      scannedAt: "2026-01-01T00:00:00Z",
      entries: [],
    })),
  },
}));

import { DownloadsPage } from "@/pages/DownloadsPage";
import { useDownloadStore } from "@/stores/useDownloadStore";
import type { DownloadTask } from "@/types";

function task(overrides: Partial<DownloadTask> = {}): DownloadTask {
  return {
    id: "task-1",
    postId: "post-1",
    mediaItemId: null,
    variantId: null,
    sourceUrl: "https://cdn.test/a.mp4",
    destinationPath: "D:\Downloads\Douyin\Videos\clip.mp4",
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
    startedAt: "2026-01-01T00:00:00Z",
    completedAt: "2026-01-01T00:00:01Z",
    ...overrides,
  };
}

beforeEach(() => {
  useDownloadStore.setState({
    tasks: {},
    order: [],
    summary: null,
    rates: {},
    hydrated: true,
    error: null,
    revision: 0,
    taskRevision: {},
    stateRevision: 0,
  });
});

afterEach(() => {
  useDownloadStore.setState({ tasks: {}, order: [] });
});

it("shows a task the store already holds", () => {
  useDownloadStore.getState().putTasks([task()]);

  render(
    <MemoryRouter>
      <DownloadsPage />
    </MemoryRouter>,
  );

  // The row itself: the file the task produced, and its state badge.
  expect(screen.getByText("clip.mp4")).toBeInTheDocument();
  expect(screen.getAllByText("Completed").length).toBeGreaterThan(0);
});

it("keeps rendering as the queue changes, rather than looping for ever", () => {
  useDownloadStore.getState().putTasks([task()]);
  render(
    <MemoryRouter>
      <DownloadsPage />
    </MemoryRouter>,
  );

  // Each of these is a store change, which is what re-runs the selector and
  // what used to make the screen render without end.
  useDownloadStore.getState().putTasks([task({ state: "downloading" })]);
  useDownloadStore.getState().putTasks([task({ state: "downloading" }), task({ id: "task-2" })]);
  useDownloadStore.getState().applyEvent({ type: "removed", taskId: "task-2" });

  expect(screen.getByText("clip.mp4")).toBeInTheDocument();
  expect(screen.getAllByText("Downloading").length).toBeGreaterThan(0);
  expect(screen.getByText("1/3 attempts")).toBeInTheDocument();
});

/**
 * A row states its size once.
 *
 * Found by looking at the running application: the progress bar printed
 * "4.0 KB / 4.0 KB · 100%" and the line under it printed "4.0 KB of 4.0 KB",
 * so every row said the same two numbers twice in two formats and read as if it
 * were reporting two different measurements.
 */
it("states a task's size once, not twice", () => {
  useDownloadStore.getState().putTasks([task()]);

  render(
    <MemoryRouter>
      <DownloadsPage />
    </MemoryRouter>,
  );

  const row = screen.getByText("clip.mp4").closest("li") as HTMLElement;
  // The bar's own "4.0 KB / 4.0 KB" is the one statement. The duplicated
  // "X of Y" phrasing must not also be present.
  expect(row.textContent).toMatch(/4\.0 KB \/ 4\.0 KB/);
  expect(row.textContent).not.toMatch(/of 4\.0 KB/);
});

it("still states the size for a queued task, which has no bar to state it", () => {
  useDownloadStore.getState().putTasks([
    task({ state: "queued", progress: { downloadedBytes: 0, totalBytes: 8192 } }),
  ]);

  render(
    <MemoryRouter>
      <DownloadsPage />
    </MemoryRouter>,
  );

  const row = screen.getByText("clip.mp4").closest("li") as HTMLElement;
  // No progress bar is drawn for a queued task, so the row itself must say how
  // much is waiting to be fetched.
  expect(row.textContent).toContain("0 B of 8.0 KB");
});
