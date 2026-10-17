/**
 * Test setup, run once before each test file.
 *
 * Only two things happen here, and both are about making assertions read like
 * the question being asked: the DOM matchers (`toBeVisible`, `toHaveAttribute`,
 * …) are registered, and every test starts from a clean DOM.
 */

import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

// jsdom implements no media queries at all, and the theme hook asks one
// question of them: whether the system prefers a dark scheme. Answering "no" is
// what a browser with no dark preference says, and it keeps a missing API from
// reading as a failure of the component under test.
//
// Defined rather than guarded: jsdom exposes the property without implementing
// it, so a truthiness check passes and the call returns nothing.
Object.defineProperty(window, "matchMedia", {
  writable: true,
  value: (query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  }),
});

afterEach(() => {
  cleanup();
});
