/**
 * The Dashboard: what is actually in the download folder.
 *
 * The point of the screen is that it reads the disk rather than the database, so
 * what these check is that it shows what the filesystem said — including a file
 * nothing has ever heard of and a format it cannot display — and that it never
 * reports success it cannot support.
 */

import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";

// The real one asks Tauri's runtime for the asset host; there is no runtime here.
vi.mock("@tauri-apps/api/core", () => ({
  convertFileSrc: (path: string) => `asset://localhost/${encodeURIComponent(path)}`,
}));

const browseMediaFolders = vi.fn();
const browseMediaFolder = vi.fn();
const openMediaFile = vi.fn();
const mediaThumbnail = vi.fn();

vi.mock("@/services/api", () => ({
  api: {
    browseMediaFolders: () => browseMediaFolders(),
    browseMediaFolder: (path: string) => browseMediaFolder(path),
    openMediaFile: (path: string) => openMediaFile(path),
    mediaThumbnail: (path: string) => mediaThumbnail(path),
  },
}));

import { DashboardPage } from "@/pages/DashboardPage";
import { resetThumbnailCache } from "@/features/media/Thumbnail";

const ROOT = "D:/Downloads/Douyin";

function folder(overrides: Partial<Record<string, unknown>> = {}) {
  return {
    name: "Videos",
    path: `${ROOT}/Videos`,
    mediaCount: 2,
    imageCount: 0,
    videoCount: 2,
    audioCount: 0,
    totalBytes: 6144,
    state: "ok",
    ...overrides,
  };
}

function file(overrides: Partial<Record<string, unknown>> = {}) {
  return {
    name: "clip.mp4",
    path: `${ROOT}/Videos/clip.mp4`,
    kind: "video",
    extension: "mp4",
    sizeBytes: 2048,
    playable: true,
    ...overrides,
  };
}

function listing() {
  return {
    root: ROOT,
    folders: [
      folder(),
      folder({ name: "郭虎", path: `${ROOT}/郭虎`, mediaCount: 0, videoCount: 0, state: "empty" }),
    ],
    looseMediaCount: 0,
  };
}

function contents() {
  return {
    path: `${ROOT}/Videos`,
    parent: ROOT,
    folders: [],
    files: [
      file(),
      file({
        name: "put-here-by-hand.mp4",
        path: `${ROOT}/Videos/put-here-by-hand.mp4`,
        sizeBytes: 256,
      }),
      file({
        name: "notes.txt",
        path: `${ROOT}/Videos/notes.txt`,
        kind: "other",
        extension: "txt",
        sizeBytes: 12,
        playable: false,
        caption: "a caption only the matched file has",
        creator: "郭虎",
      }),
    ],
    truncated: false,
  };
}

beforeEach(() => {
  browseMediaFolders.mockReset().mockResolvedValue(listing());
  browseMediaFolder.mockReset().mockResolvedValue(contents());
  openMediaFile.mockReset().mockResolvedValue(null);
  // One picture per file, unless a test says otherwise.
  mediaThumbnail.mockReset().mockImplementation(async (path: string) => `C:/cache/${path.split("/").pop()}.jpg`);
  resetThumbnailCache();
});

it("lists the folders that are really there, with what they hold", async () => {
  render(<DashboardPage />);

  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  expect(screen.getByText("郭虎")).toBeInTheDocument();
  // The count and the size, from the filesystem.
  expect(screen.getByText(/2 files · 6.0 KB|2 files · 6144/)).toBeInTheDocument();
  // A folder with nothing in it says so rather than showing a bare zero.
  expect(screen.getByText("No media in this folder")).toBeInTheDocument();
  // And the screen says which tree it looked in, so a changed setting is visible.
  expect(screen.getByText(ROOT)).toBeInTheDocument();
});

it("opens a folder, showing its files, and offers the way back", async () => {
  const user = userEvent.setup();
  render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());

  await user.click(screen.getByRole("button", { name: /Videos/ }));

  await waitFor(() => expect(browseMediaFolder).toHaveBeenCalledWith(`${ROOT}/Videos`));
  expect(await screen.findByText("clip.mp4")).toBeInTheDocument();
  // A file nothing recorded, which the database view could never show.
  expect(screen.getByText("put-here-by-hand.mp4")).toBeInTheDocument();

  // Back to the top.
  await user.click(screen.getByRole("button", { name: "All folders" }));
  await waitFor(() => expect(browseMediaFolders).toHaveBeenCalledTimes(2));
});

it("describes a format it cannot show, and offers the desktop instead", async () => {
  const user = userEvent.setup();
  render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: /Videos/ }));
  await screen.findByText("notes.txt");

  await user.click(screen.getByRole("button", { name: /notes\.txt, other/ }));

  expect(await screen.findByText(/does not show \.txt/)).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Open externally" }));
  await waitFor(() => expect(openMediaFile).toHaveBeenCalledWith(`${ROOT}/Videos/notes.txt`));
});

it("shows what the application knows about a file it recorded, and nothing about one it did not", async () => {
  const user = userEvent.setup();
  render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: /Videos/ }));
  await screen.findByText("notes.txt");

  // The caption and creator that belong to the matched file, in its own card.
  const matched = screen.getByText("notes.txt").closest("div")?.parentElement as HTMLElement;
  expect(within(matched).getByText(/a caption only the matched file has/)).toBeInTheDocument();
  expect(within(matched).getByText("郭虎")).toBeInTheDocument();

  // The one nobody recorded carries no invented metadata.
  const unrecorded = screen
    .getByText("put-here-by-hand.mp4")
    .closest("div")?.parentElement as HTMLElement;
  expect(within(unrecorded).queryByText(/caption/)).not.toBeInTheDocument();
  expect(within(unrecorded).queryByText("郭虎")).not.toBeInTheDocument();
});

it("previews a picture through the application's own asset scope", async () => {
  const user = userEvent.setup();
  browseMediaFolder.mockResolvedValueOnce({
    path: `${ROOT}/郭虎/Images`,
    parent: `${ROOT}/郭虎`,
    folders: [],
    files: [
      file({
        name: "slide-1.jpg",
        path: `${ROOT}/郭虎/Images/slide-1.jpg`,
        kind: "image",
        extension: "jpg",
        sizeBytes: 1024,
      }),
    ],
    truncated: false,
  });
  render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: /郭虎/ }));

  await user.click(await screen.findByRole("button", { name: /slide-1\.jpg, image/ }));

  const dialog = await screen.findByRole("dialog", { name: "slide-1.jpg" });
  const picture = within(dialog).getByRole("img", { name: /slide-1\.jpg/ });
  expect(picture).toHaveAttribute("src", expect.stringContaining("asset://localhost/"));
  expect(picture).toHaveAttribute("src", expect.stringContaining("slide-1.jpg"));
});

it("plays a video in the window rather than opening a browser", async () => {
  const user = userEvent.setup();
  render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: /Videos/ }));

  await user.click(await screen.findByRole("button", { name: /clip\.mp4, video/ }));

  const dialog = await screen.findByRole("dialog", { name: "clip.mp4" });
  // A real media element with controls, pointed at the file.
  const player = dialog.querySelector("video");
  expect(player).not.toBeNull();
  expect(player).toHaveAttribute("controls");
  expect(player?.getAttribute("src")).toContain("clip.mp4");
  // And nothing offers to hand it to anything else.
  expect(within(dialog).queryByRole("button", { name: "Open externally" })).not.toBeInTheDocument();
});

it("offers the desktop when a video it classified as playable will not decode", async () => {
  const user = userEvent.setup();
  render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: /Videos/ }));

  await user.click(await screen.findByRole("button", { name: /clip\.mp4, video/ }));
  const dialog = await screen.findByRole("dialog", { name: "clip.mp4" });

  // The extension said video; this engine cannot decode the codec inside. The
  // failure is only knowable at play time, so the fallback cannot be decided
  // from the file type — it has to appear once the element reports an error.
  fireEvent.error(dialog.querySelector("video") as HTMLVideoElement);

  expect(await screen.findByText(/could not be played/)).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Open externally" }));
  await waitFor(() => expect(openMediaFile).toHaveBeenCalledWith(`${ROOT}/Videos/clip.mp4`));
});

it("closes a preview with Escape and keeps the folder it was opened from", async () => {
  const user = userEvent.setup();
  render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: /Videos/ }));
  await user.click(await screen.findByRole("button", { name: /clip\.mp4, video/ }));
  await screen.findByRole("dialog");

  await user.keyboard("{Escape}");

  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  // Still in the folder, not thrown back to the top.
  expect(screen.getByText("clip.mp4")).toBeInTheDocument();
  expect(browseMediaFolders).toHaveBeenCalledTimes(1);
});

it("says a folder is empty rather than showing an empty grid", async () => {
  const user = userEvent.setup();
  browseMediaFolder.mockResolvedValueOnce({
    path: `${ROOT}/郭虎`,
    parent: ROOT,
    folders: [],
    files: [],
    truncated: false,
  });
  render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: /郭虎/ }));

  expect(await screen.findByText("This folder is empty")).toBeInTheDocument();
});

it("reports a folder that could not be read instead of showing it as empty", async () => {
  const user = userEvent.setup();
  browseMediaFolder.mockRejectedValueOnce({
    kind: "unavailable_content",
    code: "unavailable_content",
    message: "The download folder could not be opened.",
  });
  render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: /Videos/ }));

  // The error appears, and the screen does not claim the folder is empty.
  expect(await screen.findByText(/could not be opened/)).toBeInTheDocument();
  expect(screen.queryByText("This folder is empty")).not.toBeInTheDocument();
});

it("refreshes the level it is on, not always the top", async () => {
  const user = userEvent.setup();
  render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: /Videos/ }));
  await screen.findByText("clip.mp4");

  await user.click(screen.getByRole("button", { name: "Refresh" }));

  await waitFor(() => expect(browseMediaFolder).toHaveBeenCalledTimes(2));
  expect(browseMediaFolders).toHaveBeenCalledTimes(1);
  // Still looking at the folder, not sent back to the list.
  expect(screen.getByText("clip.mp4")).toBeInTheDocument();
});

it("tells the user when the download folder holds nothing at all", async () => {
  browseMediaFolders.mockResolvedValueOnce({ root: ROOT, folders: [], looseMediaCount: 0 });
  render(<DashboardPage />);

  expect(await screen.findByText("Nothing downloaded yet")).toBeInTheDocument();
});

// --- the pictures the grid draws ---------------------------------------------

it("shows the picture the application made for a file", async () => {
  const user = userEvent.setup();
  const { container } = render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: /Videos/ }));
  await screen.findByText("clip.mp4");

  // The thumbnail is asked for by the file's own path, and drawn from the
  // cache directory the application owns — not from beside the user's media.
  await waitFor(() => expect(mediaThumbnail).toHaveBeenCalledWith(`${ROOT}/Videos/clip.mp4`));
  // Queried as elements rather than by role: the picture is decorative, so it
  // carries an empty `alt` and the card itself is what is read aloud.
  await waitFor(() => {
    const sources = [...container.querySelectorAll("img")].map((img) => img.getAttribute("src") ?? "");
    expect(sources.some((src) => src.includes("clip.mp4.jpg"))).toBe(true);
  });
});

it("marks a video's picture as a video", async () => {
  // A frame from a video looks like a photograph. Without the play mark the
  // grid would claim these are stills. The mark is the decorative video icon
  // the shared icon set provides — found as an svg, since it has no text.
  const user = userEvent.setup();
  render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: /Videos/ }));
  await screen.findByText("clip.mp4");

  await waitFor(() => {
    const marks = document.querySelectorAll(".lucide-film");
    expect(marks.length).toBeGreaterThan(0);
  });
});

it("gives a folder a frame from its first video when it holds no picture", async () => {
  // A folder of downloaded videos had nothing to show before this. It now shows
  // what is in it, taken the same way the file cards take theirs.
  browseMediaFolders.mockResolvedValueOnce({
    root: ROOT,
    folders: [
      folder({
        name: "Videos",
        path: `${ROOT}/Videos`,
        mediaCount: 2,
        videoCount: 2,
        previewPath: `${ROOT}/Videos/clip.mp4`,
        previewKind: "video",
      }),
    ],
    looseMediaCount: 0,
  });
  render(<DashboardPage />);

  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  await waitFor(() => expect(mediaThumbnail).toHaveBeenCalledWith(`${ROOT}/Videos/clip.mp4`));
  await waitFor(() => {
    const marks = document.querySelectorAll(".lucide-film");
    expect(marks.length).toBeGreaterThan(0);
  });
});

it("shows a placeholder rather than a broken picture when there is no thumbnail", async () => {
  // The ordinary outcome on a machine with no FFmpeg. The card must still be a
  // card, with its name and size, and the screen must not be broken by it.
  mediaThumbnail.mockResolvedValue(null);
  const user = userEvent.setup();
  render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: /Videos/ }));
  await screen.findByText("clip.mp4");

  await waitFor(() => expect(screen.queryAllByRole("img")).toHaveLength(0));
  expect(screen.getByText("clip.mp4")).toBeInTheDocument();
  expect(screen.getByText("notes.txt")).toBeInTheDocument();
});

it("keeps the card usable when the thumbnail request is refused", async () => {
  mediaThumbnail.mockRejectedValue({ kind: "validation_error", message: "not inside the folder" });
  const user = userEvent.setup();
  render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("Videos")).toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: /Videos/ }));
  await screen.findByText("clip.mp4");

  expect(await screen.findAllByText("Unavailable")).not.toHaveLength(0);
  // And the file is still openable — a preview that could not be made says
  // nothing about whether the file itself is playable.
  await user.click(screen.getByRole("button", { name: /clip\.mp4, video/ }));
  expect(await screen.findByRole("dialog", { name: "clip.mp4" })).toBeInTheDocument();
});

it("names a carousel's picture as a carousel when a record says so", async () => {
  // The filesystem cannot tell a carousel frame from any other picture; the
  // download record can, and the card says what it knows.
  browseMediaFolder.mockResolvedValueOnce({
    path: `${ROOT}/郭虎/Images`,
    parent: `${ROOT}/郭虎`,
    folders: [],
    files: [
      {
        ...file({
          name: "slide-1.jpg",
          path: `${ROOT}/郭虎/Images/slide-1.jpg`,
          kind: "image",
          extension: "jpg",
        }),
        mediaType: "carousel",
      },
    ],
    truncated: false,
  });
  const user = userEvent.setup();
  render(<DashboardPage />);
  await waitFor(() => expect(screen.getByText("郭虎")).toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: /郭虎/ }));

  await screen.findByText("slide-1.jpg");
  expect(screen.getByText("carousel")).toBeInTheDocument();
});

/**
 * A folder's full name is readable even when the card is too narrow for it.
 *
 * Found by measuring the running application with a realistic fixture: a
 * creator folder named for a long display name overflowed its card by 259px and
 * the name was ellipsised with nothing to hover, so the folder was effectively
 * unnamed. Folders here are named after creators, whose names are not bounded.
 */
it("keeps a truncated folder's full name reachable", async () => {
  const long =
    "LongNameCreatorWithAnExcessivelyLongDisplayNameForLayoutTesting";
  browseMediaFolders.mockResolvedValueOnce({
    root: ROOT,
    folders: [folder({ name: long, path: `${ROOT}/x` })],
    looseMediaCount: 0,
  });

  render(<DashboardPage />);

  const name = await screen.findByText(long);
  // The name is clamped by CSS on a narrow card, so the whole of it has to
  // travel somewhere a person can get at it.
  expect(name).toHaveAttribute("title", long);
});
