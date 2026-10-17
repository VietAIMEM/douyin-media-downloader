/**
 * The application's one copy of download state.
 *
 * Every screen used to keep its own: the Single URL screen folded events into a
 * task it held in `useState`, the Downloads screen kept an array it refreshed
 * from the queue, the Profile screen mapped task ids through a ref. Three
 * copies of one fact, each updated by a different rule, is why the same task
 * could be "downloading" on one screen and "completed" on another — and why a
 * screen that was not mounted when the completion arrived never found out.
 *
 * So there is one store, and it is fed by the backend: events as they arrive,
 * and a queue snapshot whenever an event refers to a task it has not seen or
 * whenever a screen asks it to reconcile. The backend remains the authority;
 * this is a cache of it that every screen reads.
 */

import { useMemo } from "react";

import { create } from "zustand";

import { api } from "@/services/api";
import { onDownloadEvent } from "@/services/downloadEvents";
import { normalizeError, type BackendError } from "@/services/ipc";
import type { DownloadEvent, DownloadState, DownloadSummary, DownloadTask, Id } from "@/types";

/** Live transfer rate, which is not part of the stored task. */
export interface LiveRate {
  speedBytesPerSecond: number | null;
  etaSeconds: number | null;
}

/** A state the task will not leave by itself. */
const TERMINAL_STATES: ReadonlySet<DownloadState> = new Set<DownloadState>([
  "completed",
  "failed",
  "cancelled",
]);

export function isTerminal(state: DownloadState): boolean {
  return TERMINAL_STATES.has(state);
}

type Tasks = Record<Id, DownloadTask>;

interface DownloadStoreState {
  /** Every task the backend has told us about, keyed by id. */
  tasks: Tasks;
  /** Queue order, as the backend gave it. New tasks are appended. */
  order: Id[];
  summary: DownloadSummary | null;
  /** Speed and ETA per task, from the live stream. Never stored on the task. */
  rates: Record<Id, LiveRate>;
  /** True once a snapshot has been applied. */
  hydrated: boolean;
  /** Set when a reconcile failed, so a screen can show it. */
  error: BackendError | null;

  /**
   * A counter, incremented for every event applied.
   *
   * What makes a stale snapshot harmless. A snapshot is requested at one
   * revision and applied later; anything whose own revision has moved past that
   * point was written by an event that arrived *after* the request, so it is
   * newer than the snapshot and must not be overwritten by it.
   */
  revision: number;
  /** The revision at which each task was last written by an event. */
  taskRevision: Record<Id, number>;
  /**
   * Bumped only when a task's *state* changes, never for a progress frame.
   *
   * Screens that show a status per row — the profile listing, the history —
   * care about "did something finish", not about bytes, and a counter that
   * moves on every frame would have them re-render for nothing.
   */
  stateRevision: number;

  applyEvent: (event: DownloadEvent) => void;
  /** Fold in tasks returned by a command, e.g. the one a submit produced. */
  putTasks: (tasks: DownloadTask[]) => void;
  /** Ask the backend for the whole queue and merge it. */
  reconcile: () => Promise<void>;
  /** Drop a task that no longer exists. */
  forget: (taskId: Id) => void;
  /** Dismiss a reconcile failure the user has seen. */
  clearError: () => void;
}

/** Where a task goes in the queue order when it has not been seen before. */
function appendUnique(order: Id[], id: Id): Id[] {
  return order.includes(id) ? order : [...order, id];
}

/**
 * One reconciliation at a time, with at most one more waiting behind it.
 *
 * A task the store has not seen yet arrives as a burst — queued, a state
 * change, then progress frames — and each of those would otherwise ask the
 * backend for the whole queue. Coalescing them is what keeps "fetch what we are
 * missing" from becoming a request per event.
 */
let reconciling = false;
let reconcileQueued = false;

function requestReconcile(): void {
  if (reconciling) {
    reconcileQueued = true;
    return;
  }
  reconciling = true;
  void useDownloadStore
    .getState()
    .reconcile()
    .finally(() => {
      reconciling = false;
      if (reconcileQueued) {
        reconcileQueued = false;
        requestReconcile();
      }
    });
}
export const useDownloadStore = create<DownloadStoreState>()((set, get) => ({
  tasks: {},
  order: [],
  summary: null,
  rates: {},
  hydrated: false,
  error: null,
  revision: 0,
  taskRevision: {},
  stateRevision: 0,

  applyEvent: (event) => {
    const state = get();
    const revision = state.revision + 1;
    // Progress frames move bytes, not states; only the latter should wake a
    // screen that shows a status per row.
    const stateRevision =
      state.stateRevision + (event.type === "progress" ? 0 : 1);
    const previous = state.tasks[event.taskId];

    // A queued task the store has never seen cannot be assembled from the event
    // — the event carries an id and nothing else — so the backend is asked for
    // it. Coalesced, because a new task arrives as a burst of frames.
    if (!previous) {
      requestReconcile();
    }

    const stamp = (tasks: Tasks, rates = state.rates) => {
      set({
        tasks,
        rates,
        revision,
        stateRevision,
        taskRevision: { ...state.taskRevision, [event.taskId]: revision },
      });
    };

    switch (event.type) {
      case "progress": {
        // A late progress frame after the task finished is the classic way a
        // screen ends up showing "downloading" forever: it is the last frame to
        // arrive, so it is the last write. A terminal state is final until the
        // backend says otherwise, and the backend says so with a state change.
        if (previous && isTerminal(previous.state)) return;
        if (!previous) return;

        stamp(
          {
            ...state.tasks,
            [event.taskId]: {
              ...previous,
              progress: {
                downloadedBytes: event.downloadedBytes,
                totalBytes: event.totalBytes,
              },
            },
          },
          {
            ...state.rates,
            [event.taskId]: {
              speedBytesPerSecond: event.speedBytesPerSecond,
              etaSeconds: event.etaSeconds,
            },
          },
        );
        return;
      }

      case "stateChanged": {
        if (!previous) return;
        // Any change of state invalidates a measured rate: it was measured
        // against the attempt that just ended.
        const rates = { ...state.rates };
        delete rates[event.taskId];
        stamp({ ...state.tasks, [event.taskId]: { ...previous, state: event.state } }, rates);
        return;
      }

      case "completed": {
        if (!previous) return;
        const rates = { ...state.rates };
        delete rates[event.taskId];
        stamp(
          {
            ...state.tasks,
            [event.taskId]: {
              ...previous,
              state: "completed",
              destinationPath: event.destinationPath ?? previous.destinationPath,
              completedAt: previous.completedAt ?? new Date().toISOString(),
              // The file is on disk and its size is known, so the bar shows a
              // full one rather than stopping wherever the last frame landed.
              progress: previous.progress.totalBytes
                ? {
                    downloadedBytes: previous.progress.totalBytes,
                    totalBytes: previous.progress.totalBytes,
                  }
                : previous.progress,
            },
          },
          rates,
        );
        return;
      }

      case "failed": {
        if (!previous) return;
        const rates = { ...state.rates };
        delete rates[event.taskId];
        stamp(
          {
            ...state.tasks,
            [event.taskId]: { ...previous, state: "failed", error: event.message },
          },
          rates,
        );
        return;
      }

      case "removed": {
        const tasks = { ...state.tasks };
        delete tasks[event.taskId];
        const rates = { ...state.rates };
        delete rates[event.taskId];
        set({
          tasks,
          rates,
          order: state.order.filter((id) => id !== event.taskId),
          revision,
          stateRevision,
        });
        return;
      }

      case "queued": {
        if (!previous) return;
        stamp({
          ...state.tasks,
          [event.taskId]: { ...previous, state: "queued" },
        });
        return;
      }
    }
  },

  putTasks: (incoming) => {
    const state = get();
    const revision = state.revision + 1;
    const tasks = { ...state.tasks };
    let order = state.order;
    const taskRevision = { ...state.taskRevision };

    for (const task of incoming) {
      // A command's reply describes the task *after* the command, so it is the
      // newest thing known about it and it wins over everything already folded
      // in. An event that arrives later still supersedes it, because that event
      // gets a later revision.
      tasks[task.id] = task;
      order = appendUnique(order, task.id);
      taskRevision[task.id] = revision;
    }

    set({ tasks, order, revision, stateRevision: state.stateRevision + 1, taskRevision });
  },

  reconcile: async () => {
    const requestedAt = get().revision;
    try {
      const queue = await api.getDownloadQueue();
      const state = get();
      const tasks = { ...state.tasks };
      // Whether this snapshot tells a screen watching statuses anything new.
      let statesMoved = false;

      for (const task of queue.tasks) {
        // Written by an event that landed after this request went out: newer
        // than the snapshot, so the snapshot does not get to overwrite it.
        const written = state.taskRevision[task.id];
        if (written !== undefined && written > requestedAt) continue;
        const before = tasks[task.id];
        if (!before || before.state !== task.state) statesMoved = true;
        tasks[task.id] = task;
      }

      // A task the snapshot does not mention is gone — but only if nothing has
      // touched it since the request, for the same reason.
      const live = new Set(queue.tasks.map((task) => task.id));
      for (const id of Object.keys(tasks)) {
        const written = state.taskRevision[id];
        if (live.has(id)) continue;
        if (written !== undefined && written > requestedAt) continue;
        delete tasks[id];
        statesMoved = true;
      }

      set({
        tasks,
        order: queue.tasks.map((task) => task.id),
        summary: queue.summary,
        hydrated: true,
        error: null,
        // A snapshot that moved a task's state is the same news to a screen
        // watching statuses as an event that did. Leaving the counter still is
        // how a row went on saying "downloading" for ever: a download started
        // from a screen that does not put tasks into this store is learned
        // about only this way, and the snapshot arrived, and nothing told the
        // row about it.
        stateRevision: state.stateRevision + (statesMoved ? 1 : 0),
      });
    } catch (raw) {
      set({ error: normalizeError(raw) });
    }
  },

  forget: (taskId) => {
    const state = get();
    const tasks = { ...state.tasks };
    const rates = { ...state.rates };
    delete tasks[taskId];
    delete rates[taskId];
    set({ tasks, rates, order: state.order.filter((id) => id !== taskId) });
  },

  clearError: () => set({ error: null }),
}));

/**
 * Tasks in queue order, for screens that show the queue.
 *
 * The two slices are selected separately and combined in a `useMemo`, rather
 * than one selector returning `order.map(...)`. A selector that builds a new
 * array on every call hands React a different value on each render, and
 * `useSyncExternalStore` answers that by rendering again — for ever. It is the
 * classic way this hook deadlocks a screen, and it did: the Downloads screen
 * stopped rendering at all until the error boundary caught it.
 */
export function useDownloadTasks(): DownloadTask[] {
  const order = useDownloadStore((state) => state.order);
  const tasks = useDownloadStore((state) => state.tasks);
  return useMemo(
    () => order.map((id) => tasks[id]).filter(Boolean),
    [order, tasks],
  );
}

/** One task, or `undefined` until the backend has said anything about it. */
export function useDownloadTask(taskId: Id | null | undefined): DownloadTask | undefined {
  return useDownloadStore((state) => (taskId ? state.tasks[taskId] : undefined));
}

/**
 * The single subscription to the backend's download stream.
 *
 * Started once, from the application root, so a screen mounting and unmounting
 * cannot create a second listener or drop the only one. Returns the function
 * that stops it.
 */
export function startDownloadEvents(): () => void {
  let stopped = false;
  let unlisten: (() => void) | undefined;

  void onDownloadEvent((event) => useDownloadStore.getState().applyEvent(event))
    .then((stop) => {
      if (stopped) stop();
      else unlisten = stop;
    })
    .catch(() => {
      // Without a listener the screens still show what a snapshot gave them;
      // they simply stop updating live. Reconciliation on the next mount is
      // what recovers, and there is nothing actionable to put on screen for a
      // failed subscription.
    });

  void useDownloadStore.getState().reconcile();

  return () => {
    stopped = true;
    unlisten?.();
  };
}
