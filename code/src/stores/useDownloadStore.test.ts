/**
 * The shared download store.
 *
 * These are the properties that three separate copies of task state could not
 * have: that one task has one state however many screens are looking at it,
 * that a finished task stays finished, and that a gap in the event stream heals
 * itself rather than leaving a row showing "downloading" forever.
 *
 * The backend is mocked, because what is under test is how this store folds
 * what the backend says — not the backend.
 */

import { beforeEach, describe, expect, it, vi } from "vitest";

import { useDownloadStore } from "@/stores/useDownloadStore";
import type { DownloadEvent, DownloadState, DownloadTask } from "@/types";

const getDownloadQueue = vi.fn();
vi.mock("@/services/api", () => ({
  api: {
    getDownloadQueue: () => getDownloadQueue(),
  },
}));

/** A task as the backend would describe it. */
function task(id: string, state: DownloadState = "downloading"): DownloadTask {
  return {
    id,
    postId: "post-1",
    mediaItemId: null,
    variantId: null,
    sourceUrl: "https://cdn.test/a.mp4",
    destinationPath: null,
    fileName: null,
    extension: null,
    mediaType: "video",
    state,
    attempts: 1,
    maxAttempts: 3,
    progress: { downloadedBytes: 0, totalBytes: 1000 },
    error: null,
    createdAt: "2026-01-01T00:00:00Z",
    updatedAt: "2026-01-01T00:00:00Z",
    startedAt: null,
    completedAt: null,
  };
}

function queue(tasks: DownloadTask[]) {
  return {
    tasks,
    summary: {
      total: tasks.length,
      queued: 0,
      downloading: tasks.length,
      paused: 0,
      completed: 0,
      failed: 0,
      cancelled: 0,
      activeDownloadedBytes: 0,
      activeTotalBytes: null,
      activeWithoutSize: 0,
      completedBytes: 0,
    },
  };
}

function apply(event: DownloadEvent) {
  useDownloadStore.getState().applyEvent(event);
}

function the(taskId: string) {
  return useDownloadStore.getState().tasks[taskId];
}

beforeEach(() => {
  getDownloadQueue.mockReset();
  getDownloadQueue.mockResolvedValue(queue([]));
  // The store is a module singleton; each test starts from nothing rather than
  // from whatever the previous one left behind.
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

describe("one task, one state", () => {
  it("shows the same row to every screen looking at it", async () => {
    useDownloadStore.getState().putTasks([task("t1")]);

    apply({
      type: "progress",
      taskId: "t1",
      downloadedBytes: 400,
      totalBytes: 1000,
      speedBytesPerSecond: 200,
      etaSeconds: 3,
    });

    // Whatever a screen reads — by id, or by walking the queue — it is the same
    // row. Two copies is how the Single URL screen and the Downloads screen
    // came to disagree about one task.
    const byId = the("t1");
    const byQueue = useDownloadStore
      .getState()
      .order.map((id) => useDownloadStore.getState().tasks[id])[0];

    expect(byQueue).toBe(byId);
    expect(byId.progress.downloadedBytes).toBe(400);
    expect(useDownloadStore.getState().rates["t1"]?.etaSeconds).toBe(3);
  });

  it("does not reopen a finished task when a late progress frame arrives", async () => {
    useDownloadStore.getState().putTasks([task("t1")]);
    apply({ type: "completed", taskId: "t1", destinationPath: "D:/out/a.mp4" });
    expect(the("t1").state).toBe("completed");

    // Coalesced progress is emitted from a worker that may still be finishing;
    // this frame is older than the completion that overtook it.
    apply({
      type: "progress",
      taskId: "t1",
      downloadedBytes: 900,
      totalBytes: 1000,
      speedBytesPerSecond: 100,
      etaSeconds: 1,
    });

    const row = the("t1");
    expect(row.state).toBe("completed");
    expect(row.destinationPath).toBe("D:/out/a.mp4");
    // And the bar is not left at 900 of 1000 on a file that is finished.
    expect(row.progress.downloadedBytes).toBe(1000);
  });

  it("keeps a failed task failed when progress arrives after it", () => {
    useDownloadStore.getState().putTasks([task("t1")]);
    apply({ type: "failed", taskId: "t1", message: "the source refused the resource" });

    apply({
      type: "progress",
      taskId: "t1",
      downloadedBytes: 10,
      totalBytes: 1000,
      speedBytesPerSecond: null,
      etaSeconds: null,
    });

    expect(the("t1").state).toBe("failed");
    expect(the("t1").error).toBe("the source refused the resource");
  });

  it("keeps a cancelled task cancelled", () => {
    useDownloadStore.getState().putTasks([task("t1")]);
    apply({ type: "stateChanged", taskId: "t1", state: "cancelled" });

    apply({
      type: "progress",
      taskId: "t1",
      downloadedBytes: 10,
      totalBytes: 1000,
      speedBytesPerSecond: null,
      etaSeconds: null,
    });

    expect(the("t1").state).toBe("cancelled");
  });

  it("lets an explicit state change take a task out of a terminal state", () => {
    // Retrying a failed task is a real transition, and only the backend saying
    // so moves it — the guard above is about late *progress*, not about this.
    useDownloadStore.getState().putTasks([task("t1")]);
    apply({ type: "failed", taskId: "t1", message: "boom" });
    apply({ type: "stateChanged", taskId: "t1", state: "queued" });

    expect(the("t1").state).toBe("queued");
  });

  it("drops a task the backend says is gone", () => {
    useDownloadStore.getState().putTasks([task("t1"), task("t2")]);
    apply({ type: "removed", taskId: "t1" });

    expect(the("t1")).toBeUndefined();
    expect(useDownloadStore.getState().order).toEqual(["t2"]);
  });
});

describe("reconciliation", () => {
  it("heals an event that never arrived", async () => {
    // The task finished while the listener was not there to hear it — a frame
    // dropped, or a window that had not subscribed yet. Nothing in the event
    // stream will ever say so again, so the snapshot is the only way back.
    useDownloadStore.getState().putTasks([task("t1")]);
    expect(the("t1").state).toBe("downloading");

    getDownloadQueue.mockResolvedValue(queue([task("t1", "completed")]));
    await useDownloadStore.getState().reconcile();

    expect(the("t1").state).toBe("completed");
    expect(useDownloadStore.getState().hydrated).toBe(true);
  });

  it("does not let a snapshot overwrite an event that arrived after it was asked for", async () => {
    useDownloadStore.getState().putTasks([task("t1")]);

    // The snapshot is requested while the task is still downloading...
    let release: (value: unknown) => void = () => {};
    getDownloadQueue.mockReturnValue(
      new Promise((resolve) => {
        release = resolve;
      }),
    );
    const pending = useDownloadStore.getState().reconcile();

    // ...and the completion lands before the reply does.
    apply({ type: "completed", taskId: "t1", destinationPath: "D:/out/a.mp4" });

    release(queue([task("t1", "downloading")]));
    await pending;

    expect(the("t1").state).toBe("completed");
    expect(the("t1").destinationPath).toBe("D:/out/a.mp4");
  });

  it("asks the backend for a task it has never seen", async () => {
    getDownloadQueue.mockResolvedValue(queue([task("t1")]));

    // The event names a task the store knows nothing about, and an event
    // carries an id and nothing else — so it cannot be assembled from one.
    apply({ type: "queued", taskId: "t1" });
    await vi.waitFor(() => expect(getDownloadQueue).toHaveBeenCalled());

    await vi.waitFor(() => expect(the("t1")).toBeDefined());
  });

  it("takes the snapshot's view of a task that no event has touched", async () => {
    useDownloadStore.getState().putTasks([task("t1")]);

    getDownloadQueue.mockResolvedValue(queue([task("t1", "paused")]));
    await useDownloadStore.getState().reconcile();

    expect(the("t1").state).toBe("paused");
  });

  it("keeps a failure visible rather than swallowing it", async () => {
    getDownloadQueue.mockRejectedValue(new Error("the backend is unreachable"));
    await useDownloadStore.getState().reconcile();

    expect(useDownloadStore.getState().error).not.toBeNull();
    expect(useDownloadStore.getState().hydrated).toBe(false);
  });
});

describe("what wakes a screen up", () => {
  it("moves the state counter for a state change and not for progress", () => {
    useDownloadStore.getState().putTasks([task("t1")]);
    const afterPut = useDownloadStore.getState().stateRevision;

    apply({
      type: "progress",
      taskId: "t1",
      downloadedBytes: 1,
      totalBytes: 1000,
      speedBytesPerSecond: null,
      etaSeconds: null,
    });
    expect(useDownloadStore.getState().stateRevision).toBe(afterPut);

    apply({ type: "stateChanged", taskId: "t1", state: "paused" });
    expect(useDownloadStore.getState().stateRevision).toBe(afterPut + 1);
  });

  it("counts every progress frame in the general revision", () => {
    // The general counter still moves, so the Downloads screen's progress bars
    // update; it is only the screens that show a status that stay asleep.
    useDownloadStore.getState().putTasks([task("t1")]);
    const before = useDownloadStore.getState().revision;

    apply({
      type: "progress",
      taskId: "t1",
      downloadedBytes: 1,
      totalBytes: 1000,
      speedBytesPerSecond: null,
      etaSeconds: null,
    });

    expect(useDownloadStore.getState().revision).toBe(before + 1);
  });
});
