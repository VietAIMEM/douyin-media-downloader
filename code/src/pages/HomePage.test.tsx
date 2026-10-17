/**
 * Home.
 *
 * The page it replaced answered "what did the developers build": an implemented
 * checklist, a not-built-yet list, a phase map. Those were accurate, and they
 * were not what the screen is for. What is pinned here is that the developer
 * content is gone and the three questions a user arrives with are answered —
 * how do I start, what is happening, and where is my media.
 */

import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";

const listen = vi.fn(async () => () => {});
vi.mock("@tauri-apps/api/event", () => ({
  listen: (...args: unknown[]) => listen(...(args as [])),
}));

/**
 * The queue command reads the store, the way the real command reads the
 * backend.
 *
 * The application root reconciles on mount, and a reconciliation against a
 * queue that does not know a task is a statement that the task is gone — that
 * is correct behaviour, and it means this mock has to stand in for the backend
 * honestly: whatever the test put in the store is what the backend holds.
 */
vi.mock("@/services/api", () => ({
  api: {
    getAppInfo: vi.fn(async () => null),
    getDatabaseStatus: vi.fn(async () => null),
    getSettings: vi.fn(async () => null),
    getSettingsOptions: vi.fn(async () => null),
    getTestFixtures: vi.fn(async () => []),
    getDownloadQueue: vi.fn(async () => {
      const state = useDownloadStore.getState();
      return { tasks: Object.values(state.tasks), summary: state.summary };
    }),
  },
}));

import { App } from "@/App";
import { useDownloadStore } from "@/stores/useDownloadStore";
import type { DownloadTask } from "@/types";

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

/** The sidebar link for a screen, scoped to the navigation landmark. */
function tab(name: string) {
  return within(screen.getByRole("navigation", { name: "Main" })).getByRole("link", {
    name: new RegExp(name, "i"),
  });
}

it("no longer displays developer-facing implementation checklists", async () => {
  render(<App />);

  // Every phrase the old page carried. The build's phase map, its checklist,
  // and its IPC narration are all implementation talk, and none of it belongs
  // in front of a user.
  await screen.findByText("Download from a link");
  for (const phrase of [
    /what works in this build/i,
    /not built yet/i,
    /deliberately not built/i,
    /application shell with navigation/i,
    /phase 0\d/i,
    /tauri command/i,
  ]) {
    expect(screen.queryByText(phrase)).not.toBeInTheDocument();
  }
});

it("offers the two download workflows as its first actions", async () => {
  const user = userEvent.setup();
  render(<App />);

  await user.click(await screen.findByText("Download from a link"));
  expect(await screen.findByLabelText(/douyin link/i)).toBeInTheDocument();

  await user.click(tab("Home"));
  await user.click(await screen.findByText("Download from a profile"));
  expect(await screen.findByLabelText(/douyin profile link/i)).toBeInTheDocument();
});

it("does not expose duplicate Home and Dashboard overview destinations", async () => {
  const { NAV_ITEMS } = await import("@/navigation");
  const paths = NAV_ITEMS.map((item) => item.path);
  // One entry per path — the old sidebar had two screens competing to be the
  // overview, which read as two different applications.
  expect(new Set(paths).size).toBe(paths.length);
  // The disk browser is named for what it is, not for a second overview.
  const dashboard = NAV_ITEMS.find((item) => item.path === "/dashboard");
  expect(dashboard?.label).toBe("Download Folder");
});

it("shows what the queue is doing when there is something to show", async () => {
  function task(overrides: Partial<DownloadTask> = {}): DownloadTask {
    return {
      id: "task-1",
      postId: null,
      mediaItemId: null,
      variantId: null,
      sourceUrl: "https://cdn.test/a.mp4",
      destinationPath: null,
      fileName: "clip.mp4",
      extension: "mp4",
      mediaType: "video",
      state: "downloading",
      attempts: 1,
      maxAttempts: 3,
      progress: { downloadedBytes: 2048, totalBytes: 4096 },
      error: null,
      createdAt: "2026-01-01T00:00:00Z",
      updatedAt: "2026-01-01T00:00:00Z",
      startedAt: null,
      completedAt: null,
      ...overrides,
    };
  }

  useDownloadStore.getState().putTasks([task()]);
  render(<App />);

  await screen.findByText("Download from a link");
  expect(await screen.findByText("clip.mp4")).toBeInTheDocument();
  expect(screen.getByText(/1 in progress or waiting/)).toBeInTheDocument();
  expect(screen.getByText("Downloading")).toBeInTheDocument();
});

it("offers a first-run state rather than invented activity", async () => {
  render(<App />);

  await screen.findByText("Download from a link");
  expect(await screen.findByText(/nothing downloaded yet/i)).toBeInTheDocument();
  // And it names the way out rather than listing phases.
  expect(
    screen.getByText(/paste a douyin link on the single url screen/i),
  ).toBeInTheDocument();
});

it("points at where media lives once downloads have completed", async () => {
  function completed(): DownloadTask {
    return {
      id: "task-1",
      postId: null,
      mediaItemId: null,
      variantId: null,
      sourceUrl: "https://cdn.test/a.mp4",
      destinationPath: "D:/Downloads/clip.mp4",
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
    };
  }

  useDownloadStore.getState().putTasks([completed()]);
  render(<App />);

  await screen.findByText("Download from a link");
  const section = (await screen.findByText("Find your media")).closest("section")!;
  expect(within(section).getByRole("link", { name: /library/i })).toHaveAttribute(
    "href",
    "#/library",
  );
  expect(within(section).getByRole("link", { name: /download folder/i })).toHaveAttribute(
    "href",
    "#/dashboard",
  );
  // The counts section needs the store's summary, which a putTasks does not
  // carry; the completed file itself is still named.
  await waitFor(() => expect(screen.getByText("clip.mp4")).toBeInTheDocument());
});

beforeEach(() => {
  // Navigation is hash-based and the hash outlives a test: a test that clicked
  // into another screen leaves the window there, and the next test's App would
  // open on that screen instead of Home. Every test starts at Home, the way the
  // application itself does.
  window.location.hash = "";
});
