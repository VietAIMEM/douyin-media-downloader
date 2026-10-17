/**
 * The renditions a post offers, as the user is asked to choose between them.
 *
 * The failure this pins: a live post whose video item carried five variants —
 * three URLs for one "Original / Clean" rendition on three different hosts, and
 * two for one "Play endpoint" — was shown as five options, three of them
 * identical to each other and two identical to each other. A mirror is not a
 * choice; which host happens to serve the bytes is the transfer's business.
 *
 * The other half matters just as much: collapsing must happen on the
 * *rendition's* identity and not on its label, or two genuinely different
 * renditions that happen to share a label would become one option and one of
 * them would become unreachable.
 */

import { describe, expect, it } from "vitest";

import { logicalVariantGroups } from "@/pages/SingleUrlPage";
import type { MediaItem, MediaVariant } from "@/types";

function variant(overrides: Partial<MediaVariant> = {}): MediaVariant {
  return {
    id: "v1",
    mediaItemId: "item-1",
    kind: "original",
    label: "Original / Clean",
    sourceUrl: "https://v5-dy-ov-experiment.zjcdn.com/a.mp4",
    mimeType: "video/mp4",
    extension: "mp4",
    width: null,
    height: null,
    fileSize: null,
    bitrateKbps: null,
    isOriginal: true,
    ...overrides,
  };
}

function item(variants: MediaVariant[], overrides: Partial<MediaItem> = {}): MediaItem {
  return {
    id: "item-1",
    postId: "post-1",
    mediaType: "video",
    sourceUrl: "https://v5-dy-ov-experiment.zjcdn.com/a.mp4",
    thumbnailUrl: null,
    mimeType: "video/mp4",
    extension: "mp4",
    variants,
    ...overrides,
  } as MediaItem;
}

describe("logical variants", () => {
  it("shows one option for one rendition served from several hosts", () => {
    // The live shape, verbatim: three hosts, one rendition.
    const groups = logicalVariantGroups(
      item([
        variant({ id: "a", sourceUrl: "https://v5-dy-ov-experiment.zjcdn.com/a.mp4" }),
        variant({ id: "b", sourceUrl: "https://v5-hl-mly-ov.zjcdn.com/a.mp4" }),
        variant({ id: "c", sourceUrl: "https://v5-hl-mly-ov.zjcdn.com/b.mp4" }),
        variant({ id: "d", kind: "standard", label: "Play endpoint (video only)", isOriginal: false }),
        variant({ id: "e", kind: "standard", label: "Play endpoint (video only)", isOriginal: false }),
      ]),
    );

    expect(groups).toHaveLength(2);
    expect(groups[0].members.map((member) => member.id)).toEqual(["a", "b", "c"]);
    expect(groups[1].members.map((member) => member.id)).toEqual(["d", "e"]);
  });

  it("keeps the resolver's own order, so the first candidate stays the default", () => {
    // The representative is what gets submitted, and the resolver ranked these:
    // reordering them would quietly change which host a download starts on.
    const groups = logicalVariantGroups(
      item([variant({ id: "first" }), variant({ id: "second" }), variant({ id: "third" })]),
    );
    expect(groups).toHaveLength(1);
    expect(groups[0].representative.id).toBe("first");
  });

  it("keeps genuinely different resolutions apart", () => {
    const groups = logicalVariantGroups(
      item([
        variant({ id: "720", width: 720, height: 1280 }),
        variant({ id: "1080", width: 1080, height: 1920 }),
      ]),
    );
    expect(groups).toHaveLength(2);
    expect(groups.map((group) => group.representative.id)).toEqual(["720", "1080"]);
  });

  it("keeps genuinely different bitrates apart", () => {
    // Same geometry, different bitrate is a different rendition and must stay
    // selectable — this is the case a label-based collapse would destroy.
    const groups = logicalVariantGroups(
      item([
        variant({ id: "low", bitrateKbps: 800 }),
        variant({ id: "high", bitrateKbps: 2400 }),
      ]),
    );
    expect(groups).toHaveLength(2);
  });

  it("keeps different containers and codecs apart", () => {
    const groups = logicalVariantGroups(
      item([
        variant({ id: "mp4", mimeType: "video/mp4", extension: "mp4" }),
        variant({ id: "webm", mimeType: "video/webm", extension: "webm" }),
      ]),
    );
    expect(groups).toHaveLength(2);
  });

  it("collapses on identity even when the labels disagree", () => {
    // Two entries for the same rendition that were labelled differently: still
    // one rendition, because the label is not what makes it one.
    const groups = logicalVariantGroups(
      item([
        variant({ id: "a", label: "Original / Clean" }),
        variant({ id: "b", label: "Clean" }),
      ]),
    );
    expect(groups).toHaveLength(1);
  });

  it("leaves an audio item's own renditions to itself", () => {
    // A separate audio track is a separate download, not a rendition of the
    // video — grouping happens within an item and can never merge across them.
    const audio = item(
      [variant({ id: "au", kind: "audio_only", label: "Audio only", mimeType: "video/mp4", extension: "m4a" })],
      { id: "item-2", mediaType: "audio" },
    );
    const groups = logicalVariantGroups(audio);
    expect(groups).toHaveLength(1);
    expect(groups[0].representative.id).toBe("au");
  });

  it("keeps a carousel's images distinct from one another", () => {
    // Distinct images share a label by default and differ in nothing recorded.
    // They are separate variants of separate items in the real payload, so the
    // grouping is asserted per item — collapsing across items is the mistake
    // that would lose images.
    const first = logicalVariantGroups(item([variant({ id: "img-1" })], { id: "i1" }));
    const second = logicalVariantGroups(item([variant({ id: "img-2" })], { id: "i2" }));
    expect(first[0].representative.id).toBe("img-1");
    expect(second[0].representative.id).toBe("img-2");
  });

  it("has nothing to show for an item with no variants", () => {
    expect(logicalVariantGroups(item([]))).toEqual([]);
  });
});
