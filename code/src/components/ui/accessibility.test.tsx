/**
 * Interaction-quality tests for the shared controls.
 *
 * These are deliberately not "does it render" tests. Each one asserts something
 * a person needs in order to use the control without a mouse or without sight:
 * that a field's error is attached to the field, that a destructive button asks
 * first and can be backed out of with Escape, that a progress bar says what it
 * is measuring, and that the controls that are not `<Button>` still show where
 * the keyboard is.
 */

import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { Badge } from "@/components/ui/Badge";
import { Button } from "@/components/ui/Button";
import { ConfirmButton } from "@/components/ui/ConfirmButton";
import { ErrorNotice } from "@/components/ui/ErrorNotice";
import { ProgressBar } from "@/components/ui/ProgressBar";
import { Field, TextInput, Toggle } from "@/components/ui/form";
import { FOCUS_RING } from "@/lib/a11y";
import { humanize } from "@/lib/format";
import { BackendError } from "@/services/ipc";
import type { ContentStatus, DownloadState } from "@/types";

describe("Field", () => {
  it("gives its control the label as its accessible name", () => {
    render(
      <Field label="Download folder">
        <TextInput defaultValue="D:/Media" />
      </Field>,
    );

    // The name is what a screen reader announces; finding it by label is also
    // the check that a click on the text focuses the input.
    expect(screen.getByLabelText("Download folder")).toHaveValue("D:/Media");
  });

  it("attaches the hint to the control rather than leaving it nearby", () => {
    render(
      <Field label="Timeout" hint="Between 5 and 600 seconds">
        <TextInput />
      </Field>,
    );

    const input = screen.getByLabelText("Timeout");
    const hint = screen.getByText("Between 5 and 600 seconds");
    // Related by id, so it is announced with the field instead of being a
    // sentence that happens to sit underneath it.
    expect(input).toHaveAttribute("aria-describedby", hint.id);
    expect(input).not.toHaveAttribute("aria-invalid");
  });

  it("announces a rejected value and marks the control invalid", () => {
    render(
      <Field label="Concurrent downloads" error="Concurrent downloads must be at least 1.">
        <TextInput />
      </Field>,
    );

    const input = screen.getByLabelText("Concurrent downloads");
    const message = screen.getByRole("alert");
    expect(message).toHaveTextContent("must be at least 1");
    expect(input).toHaveAttribute("aria-describedby", message.id);
    expect(input).toHaveAttribute("aria-invalid", "true");
  });

  it("does not describe the control by a hint it is not showing", () => {
    // With an error present the hint is replaced, so the control must not point
    // at an id that no longer exists — that is how a screen reader ends up
    // reading nothing at all.
    render(
      <Field label="Folder" hint="Somewhere writable" error="Choose a download folder.">
        <TextInput />
      </Field>,
    );

    const input = screen.getByLabelText("Folder");
    const describedBy = input.getAttribute("aria-describedby");
    expect(describedBy).toBeTruthy();
    expect(document.getElementById(describedBy!)).toHaveTextContent("Choose a download folder.");
  });
});

describe("Toggle", () => {
  it("is operable and named from the keyboard", async () => {
    const user = userEvent.setup();
    const onCheckedChange = vi.fn();
    render(<Toggle label="Organize by creator" checked={false} onCheckedChange={onCheckedChange} />);

    await user.tab();
    expect(screen.getByRole("checkbox", { name: "Organize by creator" })).toHaveFocus();
    await user.keyboard(" ");
    expect(onCheckedChange).toHaveBeenCalledWith(true);
  });
});

describe("ConfirmButton", () => {
  it("does not act on the first click", async () => {
    const user = userEvent.setup();
    const onConfirm = vi.fn();
    render(
      <ConfirmButton
        label="Restore defaults"
        question="Restore every setting to its default value?"
        onConfirm={onConfirm}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Restore defaults" }));
    expect(onConfirm).not.toHaveBeenCalled();
    expect(screen.getByText(/Restore every setting/)).toBeInTheDocument();
  });

  it("moves focus to the confirming button so the keyboard is where the question is", async () => {
    const user = userEvent.setup();
    render(
      <ConfirmButton label="Clear" question="Remove the completed entries?" onConfirm={vi.fn()} />,
    );

    await user.click(screen.getByRole("button", { name: "Clear" }));
    expect(screen.getByRole("button", { name: "Confirm" })).toHaveFocus();
  });

  it("acts only on the confirmation", async () => {
    const user = userEvent.setup();
    const onConfirm = vi.fn();
    render(
      <ConfirmButton
        label="Clear"
        question="Remove the completed entries?"
        confirmLabel="Clear entries"
        onConfirm={onConfirm}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Clear" }));
    await user.click(screen.getByRole("button", { name: "Clear entries" }));
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it("can be backed out of with Escape", async () => {
    const user = userEvent.setup();
    const onConfirm = vi.fn();
    render(
      <ConfirmButton label="Clear" question="Remove the completed entries?" onConfirm={onConfirm} />,
    );

    await user.click(screen.getByRole("button", { name: "Clear" }));
    await user.keyboard("{Escape}");

    expect(onConfirm).not.toHaveBeenCalled();
    expect(screen.queryByText(/Remove the completed entries/)).not.toBeInTheDocument();
    // Focus comes back to where it started, so cancelling is not a jump.
    expect(screen.getByRole("button", { name: "Clear" })).toHaveFocus();
  });

  it("can be backed out of with the cancel button", async () => {
    const user = userEvent.setup();
    const onConfirm = vi.fn();
    render(
      <ConfirmButton label="Clear" question="Remove the completed entries?" onConfirm={onConfirm} />,
    );

    await user.click(screen.getByRole("button", { name: "Clear" }));
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onConfirm).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Clear" })).toBeInTheDocument();
  });

  it("names the group it is asking in", async () => {
    const user = userEvent.setup();
    render(
      <ConfirmButton label="Clear" question="Remove the completed entries?" onConfirm={vi.fn()} />,
    );
    await user.click(screen.getByRole("button", { name: "Clear" }));

    const group = screen.getByRole("group", { name: "Remove the completed entries?" });
    expect(within(group).getByRole("button", { name: "Confirm" })).toBeInTheDocument();
    expect(within(group).getByRole("button", { name: "Cancel" })).toBeInTheDocument();
  });
});

describe("ProgressBar", () => {
  it("exposes the download's progress to assistive technology", () => {
    render(<ProgressBar downloadedBytes={512} totalBytes={2048} label="clip.mp4" />);

    const bar = screen.getByRole("progressbar", { name: "clip.mp4" });
    expect(bar).toHaveAttribute("aria-valuenow", "25");
    expect(bar).toHaveAttribute("aria-valuemin", "0");
    expect(bar).toHaveAttribute("aria-valuemax", "100");
    expect(bar).toHaveAttribute("aria-valuetext", "512 B of 2.0 KB");
  });

  it("does not invent a percentage when the size is unknown", () => {
    render(<ProgressBar downloadedBytes={4096} totalBytes={null} label="clip.mp4" />);

    const bar = screen.getByRole("progressbar", { name: "clip.mp4" });
    // A screen reader announcing "0 percent" for a healthy download would be
    // worse than announcing nothing.
    expect(bar).not.toHaveAttribute("aria-valuenow");
    expect(bar).toHaveAttribute("aria-valuetext", "4.0 KB, total size unknown");
    expect(screen.getByText("size unknown")).toBeInTheDocument();
  });

  it("states the numbers as text, not only as a bar", () => {
    // Colour and width alone communicate nothing to a screen reader, and width
    // alone communicates nothing to someone who cannot see it.
    render(<ProgressBar downloadedBytes={1024} totalBytes={4096} />);
    expect(screen.getByText("1.0 KB / 4.0 KB")).toBeInTheDocument();
    expect(screen.getByText("25%")).toBeInTheDocument();
  });
});

describe("focus indicators", () => {
  it("every shared Button carries a visible focus style", () => {
    render(<Button>Save</Button>);
    expect(screen.getByRole("button", { name: "Save" }).className).toContain("focus-visible:outline");
  });

  it("the focus ring used by controls that are not Button is a keyboard-only ring", () => {
    // `focus-visible` rather than `focus`: a mouse click should not leave a ring
    // behind, but every control that is reachable by Tab must show one.
    expect(FOCUS_RING).toContain("focus-visible:");
    expect(FOCUS_RING).not.toMatch(/(^|\s)focus:/);
  });
});

describe("ErrorNotice", () => {
  it("is announced, and its dismiss control has a name", async () => {
    const user = userEvent.setup();
    const onDismiss = vi.fn();
    render(
      <ErrorNotice
        error={
          new BackendError({
            kind: "validation",
            code: "invalid_url",
            message: "That link is not a Douyin URL.",
          })
        }
        onDismiss={onDismiss}
      />,
    );

    expect(screen.getByRole("alert")).toHaveTextContent("That link is not a Douyin URL.");
    // The control is a single glyph, so its name has to come from an attribute.
    await user.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(onDismiss).toHaveBeenCalled();
  });
});

describe("a field with something beside it", () => {
  it("still names the control, and not the control's neighbour", () => {
    // The reason `trailing` is a prop and not a wrapper the caller writes: a
    // `<div>` around the input would take the label's `for` and leave the input
    // with no accessible name at all — silently, and only for screen readers.
    render(
      <Field
        label="Download directory"
        trailing={<button type="button">Browse</button>}
      >
        <TextInput defaultValue={"C:\\Videos"} />
      </Field>,
    );

    expect(screen.getByLabelText("Download directory")).toHaveValue("C:\\Videos");
    // The neighbour is its own control with its own name.
    expect(screen.getByRole("button", { name: "Browse" })).toBeInTheDocument();
  });
});

describe("status is never carried by colour alone", () => {  // Every state a download or a library entry can be in is shown as a badge, and
  // the badge's tone is a *second* signal — the words are the first. A palette
  // change that made two states the same colour would leave the screen readable;
  // a change that dropped the words would not, so the words are what is pinned.
  const DOWNLOAD_STATES: DownloadState[] = [
    "queued",
    "downloading",
    "paused",
    "completed",
    "failed",
    "cancelled",
  ];

  const CONTENT_STATUSES: ContentStatus[] = [
    "new",
    "ready",
    "downloading",
    "downloaded",
    "failed",
    "unavailable",
    "private",
    "unsupported",
  ];

  it("gives every download state a word, not only a tone", () => {
    for (const state of DOWNLOAD_STATES) {
      const label = humanize(state);
      const { unmount } = render(<Badge tone="neutral">{label}</Badge>);
      expect(label.trim()).not.toBe("");
      expect(screen.getByText(label)).toBeInTheDocument();
      unmount();
    }
  });

  it("gives every content status a word, not only a tone", () => {
    for (const status of CONTENT_STATUSES) {
      const label = humanize(status);
      const { unmount } = render(<Badge tone="neutral">{label}</Badge>);
      expect(label.trim()).not.toBe("");
      expect(screen.getByText(label)).toBeInTheDocument();
      unmount();
    }
  });

  it("distinguishes states in text even when the tones are identical", () => {
    // The proof that the tone is not the whole message: the same tone in both,
    // and the two are still told apart by what they say.
    render(
      <>
        <Badge tone="neutral">{humanize("completed")}</Badge>
        <Badge tone="neutral">{humanize("failed")}</Badge>
      </>,
    );
    expect(screen.getByText("Completed")).toBeInTheDocument();
    expect(screen.getByText("Failed")).toBeInTheDocument();
  });
});
