/**
 * The Content Security Policy, and the one inline script it has to name.
 *
 * The policy lives in exactly one place — the `<meta http-equiv>` in
 * `index.html` — and `tauri.conf.json` declares no policy of its own, so there
 * is nothing here that can drift out of step with what the packaged application
 * actually enforces.
 *
 * The reason this file exists: a hash is a silent failure mode. Editing the
 * inline theme script without updating the hash produces an application that
 * starts, looks fine, and has one script quietly refused — which is precisely
 * how the theme flash described in `index.html` came back the first time. These
 * recompute the hash from the file so the mistake is a failing test that prints
 * the value to paste in, rather than a mystery in a console nobody reads.
 *
 * Both files are pulled in as text, and the digest is taken with the same
 * `crypto.subtle` a browser would use, so this test needs no filesystem and no
 * Node type declarations — it reads the artefacts the way the runtime does.
 */

import { beforeEach, describe, expect, it, vi } from "vitest";

import page from "../../index.html?raw";
import tauriConfig from "../../src-tauri/tauri.conf.json?raw";

const raw = page;

/**
 * The markup with comments removed, which is what a parser actually sees.
 *
 * This is not fastidiousness. The comment above the policy discusses the script
 * element, and an early version of this file quoted the tag inside it — after
 * which the extraction below confidently found a "script" that began inside a
 * comment and ended at the real closing tag, produced a hash of prose, and
 * reported the policy as wrong. Comments are not markup and must not be scanned
 * as if they were.
 */
const html = raw.replace(/<!--[\s\S]*?-->/g, "");

const tauri = JSON.parse(tauriConfig) as {
  app: { security: { csp: unknown; assetProtocol: { enable: boolean; scope: string[] } } };
};

/** The `<meta http-equiv="Content-Security-Policy">` content, as written. */
function declaredPolicy(): string {
  const match = html.match(/<meta\s+http-equiv="Content-Security-Policy"\s+content="([^"]*)"/);
  if (!match) throw new Error("index.html declares no Content-Security-Policy");
  return match[1];
}

/** Every inline script — one with no `src`, whatever its other attributes. */
function inlineScripts(): string[] {
  return [...html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/g)]
    .filter(([, attributes]) => !/\bsrc\s*=/.test(attributes))
    .map(([, , body]) => body);
}

/** The CSP hash form of a script, computed the way a browser computes it. */
async function hashOf(script: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(script));
  const bytes = String.fromCharCode(...new Uint8Array(digest));
  return `sha256-${btoa(bytes)}`;
}

/** One directive's sources, by name. */
function sources(policy: string, directive: string): string[] {
  const found = policy
    .split(";")
    .map((part) => part.trim())
    .find((part) => part === directive || part.startsWith(`${directive} `));
  return found ? found.split(/\s+/).slice(1) : [];
}

describe("the content security policy", () => {
  it("is declared once, and only in the page", () => {
    // Two policies would both apply, and the intersection is not what either
    // one says — so a second declaration is worse than none.
    expect(raw.match(/http-equiv="Content-Security-Policy"/g)).toHaveLength(1);
    expect(tauri.app.security.csp).toBeNull();
  });

  it("names every inline script by hash, and each hash matches the script it names", async () => {
    const scripts = inlineScripts();
    // Exactly the theme script. If a second inline script is added this fails,
    // which is the point: it needs a hash of its own, listed in index.html.
    expect(scripts).toHaveLength(1);

    const declared = sources(declaredPolicy(), "script-src");
    for (const script of scripts) {
      const hash = await hashOf(script);
      // Quoted, because that is how a hash is written in a policy — the quotes
      // are part of the source expression, not decoration.
      expect(
        declared,
        `the inline script's hash is not in script-src. Paste this in:\n  '${hash}'`,
      ).toContain(`'${hash}'`);
    }
  });

  it("still allows the application's own bundle", () => {
    // A hash-only script-src would refuse the module the whole application is.
    expect(sources(declaredPolicy(), "script-src")).toContain("'self'");
  });

  it("does not permit inline, evaluated or wildcard script", () => {
    const script = sources(declaredPolicy(), "script-src");
    for (const forbidden of ["'unsafe-inline'", "'unsafe-eval'", "*", "data:", "https:"]) {
      expect(script, `script-src must not allow ${forbidden}`).not.toContain(forbidden);
    }
  });

  it("allows media and images from the asset origin and nothing wider", () => {
    const media = sources(declaredPolicy(), "media-src");
    expect(media).toContain("'self'");
    expect(media).toContain("http://asset.localhost");

    // The origin a video is loaded from is exactly the one an image is. A video
    // from anywhere else — a remote URL, say — is refused, which is what stops
    // the preview from becoming a general-purpose network client.
    for (const forbidden of ["*", "https:", "http:", "data:"]) {
      expect(media, `media-src must not allow ${forbidden}`).not.toContain(forbidden);
    }

    const images = sources(declaredPolicy(), "img-src");
    expect(images).toContain("http://asset.localhost");
    for (const forbidden of ["*", "http:"]) {
      expect(images, `img-src must not allow ${forbidden}`).not.toContain(forbidden);
    }
  });

  it("keeps the asset protocol scoped rather than open", () => {
    // The runtime grant is what actually decides which directories may be read;
    // the policy only decides which origin may be asked.
    expect(tauri.app.security.assetProtocol.enable).toBe(true);
    expect(tauri.app.security.assetProtocol.scope).toEqual([]);
  });
});

describe("the inline theme script the policy names", () => {
  beforeEach(() => {
    document.documentElement.className = "dark";
  });

  /** Run the page's own theme script against a stated system preference. */
  function runThemeScript(prefersLight: boolean) {
    window.matchMedia = vi.fn().mockImplementation((query: string) => ({
      matches: query.includes("light") ? prefersLight : !prefersLight,
      media: query,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      addListener: vi.fn(),
      removeListener: vi.fn(),
      onchange: null,
      dispatchEvent: vi.fn(),
    })) as unknown as typeof window.matchMedia;

    const [script] = inlineScripts();
    new Function(script)();
    return document.documentElement.classList.contains("dark");
  }

  it("paints the theme the system asked for, before anything else runs", () => {
    // The script exists so that a light-mode user does not get a dark frame
    // while the stored setting is still being read. It is the only thing that
    // can do that, because the setting lives in SQLite and arrives later.
    expect(runThemeScript(true)).toBe(false);
    expect(runThemeScript(false)).toBe(true);
  });
});
