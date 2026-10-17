/**
 * The boundary that keeps one screen's failure from taking the application.
 *
 * React unmounts the whole tree when a render throws and nothing catches it,
 * which in a desktop application means a blank window with no navigation and no
 * way back — there is no address bar to escape to. These check that the failure
 * stays inside the screen that caused it.
 */

import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { PageBoundary } from "@/components/layout/PageBoundary";

/** A screen that throws when asked to, and renders when not. */
function Exploding({ when }: { when: boolean }) {
  if (when) throw new Error("the profile listing could not be built");
  return <p>the screen is fine</p>;
}

beforeEach(() => {
  // React logs the caught error itself; the boundary's own message is what is
  // under test, and the console noise would bury the output.
  vi.spyOn(console, "error").mockImplementation(() => {});
});

describe("a screen that fails to render", () => {
  it("shows what failed instead of an empty window", () => {
    render(
      <PageBoundary title="Profile Downloader">
        <Exploding when />
      </PageBoundary>,
    );

    expect(screen.getByText(/profile downloader could not be shown/i)).toBeInTheDocument();
    expect(screen.getByText("the profile listing could not be built")).toBeInTheDocument();
  });

  it("leaves everything around it working", () => {
    render(
      <div>
        <nav>
          <a href="#/downloads">Downloads</a>
        </nav>
        <PageBoundary title="Profile Downloader">
          <Exploding when />
        </PageBoundary>
      </div>,
    );

    // The navigation is outside the boundary and is untouched, which is the
    // whole point: there is somewhere to go.
    expect(screen.getByRole("link", { name: "Downloads" })).toBeInTheDocument();
  });

  it("shows the failed message rather than a stack trace", () => {
    render(
      <PageBoundary title="Single URL">
        <Exploding when />
      </PageBoundary>,
    );

    const shown = screen.getByText("the profile listing could not be built").textContent ?? "";
    expect(shown).not.toMatch(/\bat \w/);
    expect(shown).not.toMatch(/\.tsx/);
  });

  it("recovers when the user asks it to", async () => {
    const user = userEvent.setup();

    // Rendered so the first attempt throws, then re-rendered without the fault.
    const { rerender } = render(
      <PageBoundary title="Single URL">
        <Exploding when />
      </PageBoundary>,
    );
    expect(screen.getByText(/could not be shown/i)).toBeInTheDocument();

    rerender(
      <PageBoundary title="Single URL">
        <Exploding when={false} />
      </PageBoundary>,
    );
    // Still showing the failure: the boundary holds it until it is reset, which
    // is what stops a screen that throws on every render from looping.
    expect(screen.getByText(/could not be shown/i)).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Try again" }));
    expect(screen.getByText("the screen is fine")).toBeInTheDocument();
  });
});
