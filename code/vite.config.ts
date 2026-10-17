import { fileURLToPath, URL } from "node:url";

import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri expects a fixed dev-server port and must not clear the screen so that
// Rust compiler output stays visible while the frontend reloads.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  resolve: {
    // Mirrors the `@/*` path mapping in tsconfig.json. TypeScript resolves the
    // alias from `paths`; Rollup needs it repeated here.
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // src-tauri is owned by Rust; do not let Vite watch it.
      ignored: ["**/src-tauri/**"],
    },
  },
  // Produce a build that Tauri can embed.
  build: {
    target: "es2021",
    minify: "esbuild",
    sourcemap: false,
  },
});
