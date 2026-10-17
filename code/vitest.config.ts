import { mergeConfig } from "vite";
import { defineConfig } from "vitest/config";

import viteConfig from "./vite.config";

/**
 * Test configuration.
 *
 * Merged from the application's Vite config rather than written alongside it, so
 * the `@/` alias resolves to the same place in a test as in the app. A second
 * copy of that mapping is the kind of thing that works until someone moves a
 * directory.
 *
 * The environment is `jsdom` because what these tests exercise is the DOM the
 * components produce — labels, roles, focus — and none of it exists without a
 * document to render into.
 *
 * Globals are deliberately *off*: every test imports `describe`, `it` and
 * `expect` from `vitest` explicitly. A test file that needs no import to get
 * `expect` is also a test file where a typo in a helper name silently refers to
 * something else.
 */
export default mergeConfig(
  viteConfig,
  defineConfig({
    test: {
      environment: "jsdom",
      setupFiles: ["./src/test/setup.ts"],
      include: ["src/**/*.test.{ts,tsx}"],
      // The Tauri IPC layer is not present in a test; anything that reaches it
      // is stubbed at the call site rather than globally, so a component that
      // unexpectedly starts calling the backend fails loudly.
      restoreMocks: true,
    },
  }),
);
