/**
 * The preview pictures on the Dashboard cards.
 *
 * Three behaviours are worth pinning, and they are all about restraint rather
 * than about drawing: asking for a picture only when a card is on screen, asking
 * once for a file however many cards want it, and treating "there is no picture"
 * as an ordinary answer instead of a failure.
 */

import { render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

// The real one asks Tauri's runtime for the asset host; there is no runtime here.
vi.mock("@tauri-apps/api/core", () => ({
  convertFileSrc: (path: string) => `asset://localhost/${encodeURIComponent(path)}`,
}));

const mediaThumbnail = vi.fn();
vi.mock("@/services/api", () => ({
  api: { mediaThumbnail: (path: string) => mediaThumbnail(path) },
}));

import { Thumbnail, resetThumbnailCache, thumbnailSource } from "@/features/media/Thumbnail";

beforeEach(() => {
  mediaThumbnail.mockReset();
  resetThumbnailCache();
});

afterEach(() => {
  resetThumbnailCache();
});

it("draws the picture the application made", async () => {
  mediaThumbnail.mockResolvedValue("C:/cache/abc.jpg");

  render(<Thumbnail path="D:/Downloads/Douyin/Videos/clip.mp4" kind="video" alt="clip.mp4" />);

  const picture = await screen.findByRole("img");
  expect(picture).toHaveAttribute("src", expect.stringContaining("abc.jpg"));
  expect(picture).toHaveAttribute("alt", "clip.mp4");
});

it("falls back to a placeholder when there is no picture to be had", async () => {
  // No FFmpeg, an undecodable format, an audio file: the backend answers `null`,
  // and that is an ordinary answer rather than something to report. The
  // placeholder is the kind's own icon, decorative and drawn as an svg.
  mediaThumbnail.mockResolvedValue(null);

  render(<Thumbnail path="D:/Downloads/Douyin/Videos/song.m4a" kind="audio" />);

  await waitFor(() => expect(mediaThumbnail).toHaveBeenCalledTimes(1));
  expect(screen.queryByRole("img")).not.toBeInTheDocument();
  expect(document.querySelector(".lucide-file-audio")).not.toBeNull();
});

it("marks a video as a video even when no frame could be taken", async () => {
  mediaThumbnail.mockResolvedValue(null);
  render(<Thumbnail path="D:/Downloads/Douyin/Videos/clip.mp4" kind="video" />);

  await waitFor(() => expect(mediaThumbnail).toHaveBeenCalledTimes(1));
  expect(document.querySelector(".lucide-film")).not.toBeNull();
});

it("says a file is unavailable rather than pretending it has no picture", async () => {
  // The difference that matters: "nothing to show you" and "this file is not
  // there any more" are different facts, and only the second needs acting on.
  mediaThumbnail.mockRejectedValue({ kind: "validation_error", message: "not inside the folder" });

  render(<Thumbnail path="D:/Downloads/Douyin/Videos/gone.mp4" kind="video" />);

  expect(await screen.findByText("Unavailable")).toBeInTheDocument();
});

it("a broken picture does not break the card", async () => {
  mediaThumbnail.mockRejectedValue(new Error("the bridge said no"));

  expect(() =>
    render(<Thumbnail path="D:/Downloads/Douyin/Videos/clip.mp4" kind="video" />),
  ).not.toThrow();
  expect(await screen.findByText("Unavailable")).toBeInTheDocument();
});

it("asks for one file once, however many cards want it", async () => {
  // A folder card and a file card can name the same file, and a grid re-renders
  // constantly. Extracting a frame is the expensive thing; asking twice is not.
  mediaThumbnail.mockResolvedValue("C:/cache/one.jpg");

  const [a, b] = await Promise.all([
    thumbnailSource("D:/Downloads/Douyin/Videos/clip.mp4"),
    thumbnailSource("D:/Downloads/Douyin/Videos/clip.mp4"),
  ]);

  expect(mediaThumbnail).toHaveBeenCalledTimes(1);
  expect(a).toEqual(b);
});

it("answers from memory the second time, without asking again", async () => {
  mediaThumbnail.mockResolvedValue("C:/cache/one.jpg");

  await thumbnailSource("D:/Downloads/Douyin/Videos/clip.mp4");
  await thumbnailSource("D:/Downloads/Douyin/Videos/clip.mp4");

  expect(mediaThumbnail).toHaveBeenCalledTimes(1);
});

it("remembers that a file has no preview, rather than asking on every render", async () => {
  mediaThumbnail.mockResolvedValue(null);

  await thumbnailSource("D:/Downloads/Douyin/Videos/song.m4a");
  await thumbnailSource("D:/Downloads/Douyin/Videos/song.m4a");

  expect(mediaThumbnail).toHaveBeenCalledTimes(1);
});

it("does not ask about a card that has no file", () => {
  render(<Thumbnail kind="other" />);
  expect(mediaThumbnail).not.toHaveBeenCalled();
  expect(document.querySelector(".lucide-external-link")).not.toBeNull();
});

it("asks about a different file separately", async () => {
  mediaThumbnail.mockResolvedValue("C:/cache/x.jpg");

  await thumbnailSource("D:/Downloads/Douyin/Videos/one.mp4");
  await thumbnailSource("D:/Downloads/Douyin/Videos/two.mp4");

  expect(mediaThumbnail).toHaveBeenCalledTimes(2);
});

it("keeps going after the bridge fails, and asks again for a file it never got", async () => {
  // A failure is remembered for that file — asking again on every render would
  // be a retry storm — but it must not poison the connection for other files.
  mediaThumbnail.mockRejectedValueOnce(new Error("boom")).mockResolvedValue("C:/cache/ok.jpg");

  const first = await thumbnailSource("D:/Downloads/Douyin/Videos/bad.mp4");
  expect(first.unreadable).toBe(true);

  const second = await thumbnailSource("D:/Downloads/Douyin/Videos/good.mp4");
  expect(second.src).toBe("C:/cache/ok.jpg");
  expect(second.unreadable).toBe(false);
  expect(mediaThumbnail).toHaveBeenCalledTimes(2);
});
