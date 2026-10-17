/**
 * The Library's rows, and what they say about the files behind them.
 *
 * The three answers a filesystem check can give — the file is there, the file is
 * gone, the file could not be looked at — are the reason this file exists. The
 * last two used to be one, and telling a user their download had been deleted
 * because a drive was unplugged is the kind of mistake that costs trust in the
 * whole screen.
 */

import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";

const queryLibrary = vi.fn();
const listLibraryCreators = vi.fn();

vi.mock("@/services/api", () => ({
  api: {
    queryLibrary: (query: unknown) => queryLibrary(query),
    listLibraryCreators: () => listLibraryCreators(),
    exportLibrary: vi.fn(async () => ({ path: "", recordCount: 0 })),
    revealExport: vi.fn(async () => undefined),
    submitProfileDownloads: vi.fn(async () => ({})),
  },
}));

import { LibraryPage } from "@/pages/LibraryPage";
import type { LibraryEntry, LibraryPage as LibraryPageDTO } from "@/types";

/**
 * Switch the page to the list layout, the way a user does.
 *
 * Several tests below pin the *row's* semantics — the closest `tr`, the
 * columns. The page now opens on the card grid, so these tests press the list
 * toggle first; the cards' own behaviour is pinned separately.
 */
async function useListLayout(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getByRole("button", { name: "Show as a detailed list" }));
}

const PATH = "D:\\Downloads\\Douyin\\郭虎\\Videos\\clip.mp4";

function entry(overrides: Partial<LibraryEntry> = {}): LibraryEntry {
  return {
    post: {
      id: "post-1",
      platform: "douyin",
      platformPostId: "7300000000000000001",
      creatorId: null,
      creatorHandle: "guohu",
      url: "https://www.douyin.com/video/7300000000000000001",
      canonicalUrl: null,
      caption: "Sunset over the sea",
      mediaType: "video",
      thumbnailUrl: null,
      publishedAt: null,
      status: "downloaded",
      mediaItems: [],
      metadata: {},
      createdAt: null,
      updatedAt: null,
    },
    creatorName: "郭虎",
    creatorHandle: "guohu",
    status: "downloaded",
    downloadState: "completed",
    fileState: "present",
    fileStateReason: null,
    localPath: PATH,
    downloadedAt: "2026-10-15T10:00:00Z",
    error: null,
    mediaItemCount: 1,
    downloadedItemCount: 1,
    durationSeconds: 12,
    width: 1920,
    height: 1080,
    ...overrides,
  };
}

function page(entries: LibraryEntry[], overrides: Partial<LibraryPageDTO> = {}) {
  return {
    entries,
    total: entries.length,
    offset: 0,
    limit: 50,
    scanned: entries.length,
    truncated: false,
    counts: {
      all: entries.length,
      videos: entries.length,
      images: 0,
      audio: 0,
      downloaded: entries.length,
      failed: 0,
      new: 0,
    },
    ...overrides,
  };
}

beforeEach(() => {
  queryLibrary.mockReset().mockResolvedValue(page([entry()]));
  listLibraryCreators.mockReset().mockResolvedValue([{ id: "c1", name: "郭虎" }]);
});

it("shows a file that is there without a presence warning", async () => {
  const user = userEvent.setup();
  render(<LibraryPage />);

  await useListLayout(user);
  const heading = await screen.findByText("Sunset over the sea");
  // Scoped to the row: "Downloaded" is also the name of one of the view tabs.
  const row = heading.closest("tr") as HTMLElement;
  expect(within(row).getByText("Downloaded")).toBeInTheDocument();

  // A file that is simply there needs no badge: a column of "Present" would bury
  // the one row worth looking at.
  expect(screen.queryByText("File missing")).not.toBeInTheDocument();
  expect(screen.queryByText("Could not be checked")).not.toBeInTheDocument();
});

it("says a file that is confirmed gone is missing", async () => {
  queryLibrary.mockResolvedValue(
    page([
      entry({
        status: "ready",
        fileState: "missing",
        fileStateReason: null,
        error: "The downloaded file is no longer on disk, so this can be downloaded again.",
      }),
    ]),
  );

  render(<LibraryPage />);

  expect(await screen.findByText("File missing")).toBeInTheDocument();
  expect(screen.getByText("Ready")).toBeInTheDocument();
  expect(
    screen.getByText(/no longer on disk/),
  ).toBeInTheDocument();
});

it("says a file it could not check is not gone, and why", async () => {
  // The whole point of the change. Nothing was deleted; the folder this file was
  // saved under is no longer the one configured, so nobody looked.
  queryLibrary.mockResolvedValue(
    page([
      entry({
        fileState: "unavailable",
        fileStateReason: "outside-configured-root",
        error:
          "Whether the downloaded file is still on disk could not be checked, so nothing has been concluded about it.",
      }),
    ]),
  );

  render(<LibraryPage />);

  expect(await screen.findByText("Could not be checked")).toBeInTheDocument();

  // Never the missing wording, in the badge or anywhere else in the row.
  expect(screen.queryByText("File missing")).not.toBeInTheDocument();
  expect(screen.queryByText(/no longer on disk/)).not.toBeInTheDocument();

  // And the reason reaches the user, in the same words the Downloads screen uses
  // for the same condition.
  expect(
    screen.getByTitle("This was saved under a different download folder than the one configured now, so it cannot be checked."),
  ).toBeInTheDocument();
});

it("keeps a file it could not check in the downloaded state", async () => {
  // A completed record with no evidence against it stays downloaded. Moving it
  // out would empty a user's library because they unplugged a disk.
  queryLibrary.mockResolvedValue(
    page([
      entry({
        status: "downloaded",
        fileState: "unavailable",
        fileStateReason: "directory-unavailable",
      }),
    ]),
  );

  const user = userEvent.setup();
  render(<LibraryPage />);
  await useListLayout(user);

  const row = (await screen.findByText("Sunset over the sea")).closest("tr") as HTMLElement;
  expect(within(row).getByText("Downloaded")).toBeInTheDocument();
  expect(within(row).getByText("Could not be checked")).toBeInTheDocument();
});

it("says nothing about a file that was never produced", async () => {
  // A post discovered and never downloaded has no answer to give, which is not
  // the same as being missing.
  queryLibrary.mockResolvedValue(
    page([entry({ status: "new", downloadState: null, fileState: null, localPath: null })]),
  );

  render(<LibraryPage />);

  expect(await screen.findByText("Sunset over the sea")).toBeInTheDocument();
  expect(screen.queryByText("File missing")).not.toBeInTheDocument();
  expect(screen.queryByText("Could not be checked")).not.toBeInTheDocument();
});

it("still shows the row the way it did before: name, type, size, duration, file", async () => {
  const user = userEvent.setup();
  render(<LibraryPage />);
  await useListLayout(user);

  const row = (await screen.findByText("Sunset over the sea")).closest("tr") as HTMLElement;
  expect(within(row).getByText("7300000000000000001")).toBeInTheDocument();
  expect(within(row).getByText("郭虎")).toBeInTheDocument();
  expect(within(row).getByText(/video/i)).toBeInTheDocument();
  expect(within(row).getByText(/1920×1080/)).toBeInTheDocument();
  expect(within(row).getByText("clip.mp4")).toBeInTheDocument();
  expect(within(row).getByText("1 of 1 items on disk")).toBeInTheDocument();
});

it("still lets a row be selected, and only when it can be downloaded", async () => {
  queryLibrary.mockResolvedValue(
    page([
      entry({ post: { ...entry().post, id: "a", caption: "Downloadable" }, status: "new", fileState: null }),
      entry({
        post: { ...entry().post, id: "b", caption: "Already here" },
        status: "downloaded",
        fileState: "present",
      }),
    ]),
  );

  render(<LibraryPage />);
  await screen.findByText("Downloadable");

  const boxes = screen.getAllByRole("checkbox") as HTMLInputElement[];
  expect(boxes).toHaveLength(2);
  expect(boxes[0]).toBeEnabled();
  expect(boxes[1]).toBeDisabled();
});

it("still distinguishes an empty library from a filter that matches nothing", async () => {
  queryLibrary.mockResolvedValue(page([]));

  render(<LibraryPage />);

  expect(await screen.findByText("The library is empty")).toBeInTheDocument();
  expect(screen.queryByText("Nothing matches")).not.toBeInTheDocument();
});

it("still reports a failed query instead of showing an empty library", async () => {
  queryLibrary.mockRejectedValue({ kind: "database_error", code: "database_error", message: "The library could not be read." });

  render(<LibraryPage />);

  expect(await screen.findByText(/could not be read/)).toBeInTheDocument();
  expect(screen.queryByText("The library is empty")).not.toBeInTheDocument();
});

it("still asks the backend again when a view is chosen", async () => {
  const user = userEvent.setup();
  render(<LibraryPage />);
  await screen.findByText("Sunset over the sea");

  await user.click(screen.getByRole("button", { name: /Failed/ }));

  await waitFor(() =>
    expect(queryLibrary.mock.calls.some(([query]) => (query as { view: string }).view === "failed")).toBe(true),
  );
});

// --- partial results, refresh, and export ------------------------------------
//
// The audit found three things missing from this screen's states: a scan that
// stopped at its limit looked like a complete answer, a refresh left the old
// rows on screen with no sign anything was happening, and the export flow had no
// test at all. Each test here pins the state the user is supposed to see.

it("says so when the query stopped at its scan limit", async () => {
  // A bound the user cannot see is a silent lie: past it, "of N" reads as
  // complete when it is a floor.
  queryLibrary.mockResolvedValue(
    page([entry()], {
      total: 23_456,
      scanned: 20_000,
      truncated: true,
      counts: {
        all: 23_456,
        videos: 11_000,
        images: 12_000,
        audio: 456,
        downloaded: 20_000,
        failed: 0,
        new: 3_456,
      },
    }),
  );

  render(<LibraryPage />);

  expect(await screen.findByText("Sunset over the sea")).toBeInTheDocument();
  const notice = screen.getByRole("status");
  expect(notice.textContent).toContain("first 20000");
  expect(notice.textContent).toContain("rest is not included");
});

it("does not claim truncation when the read finished", async () => {
  queryLibrary.mockResolvedValue(page([entry()], { truncated: false, scanned: 1 }));

  render(<LibraryPage />);

  expect(await screen.findByText("Sunset over the sea")).toBeInTheDocument();
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});

it("shows a busy notice when a refresh runs over rows already on screen", async () => {
  // The first load replaces the rows; a later one leaves them standing, and
  // without a notice a large-library query looks stalled.
  let release!: (value: unknown) => void;
  queryLibrary.mockImplementationOnce(() => page([entry()]));
  queryLibrary.mockImplementation(
    () =>
      new Promise((resolve) => {
        release = resolve;
      }),
  );

  const user = userEvent.setup();
  render(<LibraryPage />);
  await screen.findByText("Sunset over the sea");

  // Change the sort to trigger a second load. The control is a native select,
  // so the value is chosen directly rather than through an options popup.
  await user.selectOptions(screen.getByRole("combobox", { name: "Sort by" }), "oldest");

  // The notice appears while the second query is in flight, beside the rows.
  expect(await screen.findByText("Updating the library…")).toBeInTheDocument();

  release(page([entry()]));
  await waitFor(() =>
    expect(screen.queryByText("Updating the library…")).not.toBeInTheDocument(),
  );
});

it("does not show a busy notice for a load that finished", async () => {
  render(<LibraryPage />);
  await screen.findByText("Sunset over the sea");

  expect(screen.queryByText("Updating the library…")).not.toBeInTheDocument();
  expect(screen.queryByText("Loading the library…")).not.toBeInTheDocument();
});

// --- the card grid ------------------------------------------------------------
//
// The grid is the layout the page opens on, so its reading of a file's state
// has to match the row's. A card that says "missing" where the row says
// "could not be checked" would have the two layouts telling different stories
// about one file — the exact failure this file's first tests exist to prevent.

it("opens on the card grid and offers the list beside it", async () => {
  render(<LibraryPage />);

  // The card is there without anyone asking for it, and so is the way back.
  expect(await screen.findByText("Sunset over the sea")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Show as picture cards" })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
  expect(screen.getByRole("button", { name: "Show as a detailed list" })).toHaveAttribute(
    "aria-pressed",
    "false",
  );
});

it("shows a placeholder, not a broken picture, for a post without a thumbnail", async () => {
  queryLibrary.mockResolvedValue(
    page([entry({ post: { ...entry().post, thumbnailUrl: null } })]),
  );

  render(<LibraryPage />);

  // No <img> is rendered, so nothing can fail to load; the type's own glyph
  // stands in.
  await screen.findByText("Sunset over the sea");
  expect(screen.queryByRole("img")).not.toBeInTheDocument();
});

it("carries the same presence wording and hint the list carries", async () => {
  queryLibrary.mockResolvedValue(
    page([
      entry({
        status: "downloaded",
        fileState: "unavailable",
        fileStateReason: "outside-configured-root",
      }),
    ]),
  );

  render(<LibraryPage />);

  expect(await screen.findByText("Could not be checked")).toBeInTheDocument();
  expect(
    screen.getByTitle(
      "This was saved under a different download folder than the one configured now, so it cannot be checked.",
    ),
  ).toBeInTheDocument();
  expect(screen.queryByText("File missing")).not.toBeInTheDocument();
});

it("still selects from the grid, and only what can be downloaded", async () => {
  const user = userEvent.setup();
  queryLibrary.mockResolvedValue(
    page([
      entry({ post: { ...entry().post, id: "a", caption: "Downloadable" }, status: "new", fileState: null }),
      entry({
        post: { ...entry().post, id: "b", caption: "Already here" },
        status: "downloaded",
        fileState: "present",
      }),
    ]),
  );

  render(<LibraryPage />);
  await screen.findByText("Downloadable");

  const boxes = screen.getAllByRole("checkbox") as HTMLInputElement[];
  expect(boxes).toHaveLength(2);
  expect(boxes[0]).toBeEnabled();
  expect(boxes[1]).toBeDisabled();

  await user.click(boxes[0]);
  expect(screen.getByText("Download selected (1)")).toBeInTheDocument();
});

it("explains itself in the grid the same way it does in the list", async () => {
  // The sentence a row shows for a failed state is the user's only path to
  // understanding it; the card may not drop it for the sake of the picture.
  queryLibrary.mockResolvedValue(
    page([
      entry({
        status: "ready",
        fileState: "missing",
        error: "The downloaded file is no longer on disk, so this can be downloaded again.",
      }),
    ]),
  );

  render(<LibraryPage />);

  expect(await screen.findByText(/no longer on disk/)).toBeInTheDocument();
});

/**
 * The layout control says what it does.
 *
 * Found by looking at the running application, not by a test: the two buttons
 * were drawn with a folder and a checkbox, so the control read as "open a
 * folder" and "tick something" rather than "cards" and "list". The accessible
 * names were right the whole time, which is why only looking could find it.
 */
it("draws the layout control as a grid and a list, not as something else", async () => {
  render(<LibraryPage />);
  await screen.findByText("Sunset over the sea");

  const cards = screen.getByRole("button", { name: "Show as picture cards" });
  const list = screen.getByRole("button", { name: "Show as a detailed list" });

  expect(cards.querySelector(".lucide-layout-grid")).not.toBeNull();
  expect(list.querySelector(".lucide-list")).not.toBeNull();
  // The shapes that used to stand in for them, and did not mean either thing.
  expect(cards.querySelector(".lucide-folder-open")).toBeNull();
  expect(list.querySelector(".lucide-square-check")).toBeNull();
});

/**
 * The detail table can be reached to its last column.
 *
 * Found by measuring the running application: the table was 1038px wide inside
 * a 974px container whose `overflow-hidden` clipped the difference, so the
 * Published column sat past the right edge of the window with no way to scroll
 * to it. jsdom computes no layout, so what is pinned here is the container's
 * decision to scroll rather than clip; the reachability itself was measured in
 * the real window.
 */
it("does not clip the detail table's last column away", async () => {
  const user = userEvent.setup();
  render(<LibraryPage />);
  await useListLayout(user);

  const table = await screen.findByRole("table");
  const container = table.closest("section") as HTMLElement;
  expect(container.className).toContain("overflow-x-auto");
  expect(container.className).not.toContain("overflow-hidden");
});

/**
 * A card names its creator in full, however narrow the card is.
 *
 * Found by sweeping the running application for elements that clamp their text
 * with no way to read the whole of it: the card's creator line was one of them,
 * so a long creator name was cut off with nothing to hover. Cards here are a
 * fifth of the window wide, and creator names are not bounded.
 */
it("keeps a clamped creator name readable on the card", async () => {
  const long = "LongNameCreatorWithAnExcessivelyLongDisplayNameForLayoutTesting";
  queryLibrary.mockResolvedValue(page([entry({ creatorName: long })]));

  render(<LibraryPage />);

  const name = await screen.findByText(long);
  expect(name).toHaveAttribute("title", long);
});
