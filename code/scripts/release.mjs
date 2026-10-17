#!/usr/bin/env node
/**
 * Build, sign, checksum and describe a release.
 *
 * One command, and it stops at the first thing that is wrong. That is the whole
 * design: a release is a set of artifacts that have to agree with each other —
 * three copies of the version, a signed package for each installer, the
 * checksums, and the manifest the updater reads — and every one of those is a
 * thing that can silently be stale. A script that carried on past a failure
 * would produce a directory that looks like a release and is not one.
 *
 *     node scripts/release.mjs                 # checks, build, checksums, manifest
 *     node scripts/release.mjs --skip-checks   # iteration only; never for a release
 *
 * Environment:
 *   TAURI_SIGNING_PRIVATE_KEY           path to, or contents of, the updater key
 *   TAURI_SIGNING_PRIVATE_KEY_PASSWORD  its passphrase, when it has one
 *   DOUYIN_RELEASE_BASE_URL             where the artifacts will be served from
 *
 * The base URL is required. The updater manifest carries absolute URLs, and a
 * release whose manifest points somewhere that is not where the artifacts will
 * live is a release nobody can update from.
 */

import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const skipChecks = process.argv.includes("--skip-checks");

const releaseDir = path.join(root, "release");
const bundleDir = path.join(root, "src-tauri", "target", "release", "bundle");

function fail(message) {
  console.error(`\nrelease: ${message}`);
  process.exit(1);
}

function step(message) {
  console.log(`\n=== ${message}`);
}

/** Quote an argument for `cmd.exe`, which is what a shelled call goes through. */
function quote(argument) {
  return /[\s"]/.test(argument) ? `"${argument.replace(/"/g, '""')}"` : argument;
}

function run(command, args, options = {}) {
  const line = [command, ...args].map(quote).join(" ");
  console.log(`$ ${line}`);
  // `npm` and `npx` are `.cmd` shims on Windows and CreateProcess cannot run one
  // directly, so a shell is the only way to reach them. Going through `cmd.exe`
  // explicitly — rather than letting Node build a command line — keeps the
  // quoting in one place instead of splitting it between here and Node.
  const shelled = process.platform === "win32";
  execFileSync(shelled ? "cmd.exe" : command, shelled ? ["/d", "/s", "/c", line] : args, {
    cwd: options.cwd ?? root,
    stdio: "inherit",
    env: { ...process.env, ...options.env },
  });
}

/** The `version` field of a TOML file, read as text. */
function cargoVersion(file) {
  const text = fs.readFileSync(file, "utf8");
  const match = text.match(/^version\s*=\s*"([^"]+)"/m);
  if (!match) fail(`no version field in ${path.relative(root, file)}`);
  return match[1];
}

function jsonVersion(file) {
  const json = JSON.parse(fs.readFileSync(file, "utf8"));
  if (typeof json.version !== "string") {
    fail(`no version field in ${path.relative(root, file)}`);
  }
  return json.version;
}

// --- 1. One version, or stop ------------------------------------------------

step("Checking the release identity");

const version = cargoVersion(path.join(root, "src-tauri", "Cargo.toml"));
const tauriVersion = jsonVersion(path.join(root, "src-tauri", "tauri.conf.json"));
const npmVersion = jsonVersion(path.join(root, "package.json"));

for (const [where, found] of [
  ["src-tauri/tauri.conf.json", tauriVersion],
  ["package.json", npmVersion],
]) {
  if (found !== version) {
    fail(
      `${where} says ${found} but Cargo.toml says ${version}. The installer would record ` +
        `one version while the application reported another.`,
    );
  }
}
if (!/^\d+\.\d+\.\d+$/.test(version)) {
  fail(`the version ${version} is not MAJOR.MINOR.PATCH, which is what the updater compares by`);
}
console.log(`version ${version}`);

// --- 2. The release configuration, before anything is built -----------------

step("Checking the release configuration");

if (!process.env.TAURI_SIGNING_PRIVATE_KEY) {
  fail(
    "TAURI_SIGNING_PRIVATE_KEY is not set. Release artifacts are signed so that an " +
      "installed copy will accept them as updates; without the key the bundle step " +
      "fails, and it should. See docs/RELEASE_PROCESS.md.",
  );
}
// Deliberately quiet about the value: whether it is set is all this needs to
// say, and echoing any part of it would put it in a build log.
console.log("signing key present");

const baseUrl = process.env.DOUYIN_RELEASE_BASE_URL;
if (!baseUrl) {
  fail(
    "DOUYIN_RELEASE_BASE_URL is not set. The updater manifest needs absolute URLs, so " +
      "a release cannot be described without knowing where it will be served from.",
  );
}
const base = baseUrl.endsWith("/") ? baseUrl : `${baseUrl}/`;

// The public half of the signing key. The bundler compiles it into the
// application, where it is what decides which packages may be installed, so a
// build with the wrong key produces an application that refuses every genuine
// update — and one with the key of whoever set the variable.
const pubkeyInput = process.env.DOUYIN_UPDATER_PUBKEY;
if (!pubkeyInput) {
  fail(
    "DOUYIN_UPDATER_PUBKEY is not set. It is the public half of the signing key, and " +
      "it is what the installed application verifies updates against.",
  );
}
const pubkey = fs.existsSync(pubkeyInput)
  ? fs.readFileSync(pubkeyInput, "utf8").trim()
  : pubkeyInput.trim();
if (!pubkey.includes("\n") && !pubkey.startsWith("dW50cnVzdGVk")) {
  fail(`the updater public key does not look like a minisign public key`);
}
console.log(`serving from ${base}`);

// The committed configuration deliberately carries no update feed: a default
// build must not reach for an endpoint nobody chose, and it must not ship
// somebody's key. A release supplies both, here, and only for this build.
const overlayPath = path.join(releaseDir, "release-config.json");
fs.mkdirSync(releaseDir, { recursive: true });
fs.writeFileSync(
  overlayPath,
  `${JSON.stringify(
    {
      bundle: { createUpdaterArtifacts: true },
      plugins: {
        updater: {
          endpoints: [`${base}latest.json`],
          pubkey,
        },
      },
    },
    null,
    2,
  )}\n`,
);

// --- 3. Checks --------------------------------------------------------------

if (skipChecks) {
  console.log("\n=== Skipping checks (--skip-checks)");
} else {
  step("Rust tests");
  run("cargo", ["test", "--all-targets"], { cwd: path.join(root, "src-tauri") });
  step("Rust static checks");
  run("cargo", ["clippy", "--all-targets", "--", "-D", "warnings"], {
    cwd: path.join(root, "src-tauri"),
  });
  step("Frontend tests");
  run("npm", ["test"]);
  step("Frontend typecheck");
  run("npm", ["run", "typecheck"]);
}

// --- 4. Build ---------------------------------------------------------------

step("Building the Windows x64 release");
run("npx", ["tauri", "build", "--config", overlayPath]);

// --- 5. Collect what was actually produced ----------------------------------

step("Collecting artifacts");

if (!fs.existsSync(bundleDir)) {
  fail(`the bundler produced no output directory at ${path.relative(root, bundleDir)}`);
}

/** Every file under `dir`, recursively. */
function walk(dir) {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const full = path.join(dir, entry.name);
    return entry.isDirectory() ? walk(full) : [full];
  });
}

const produced = walk(bundleDir);
// Filtered by version on purpose: a stale artifact from an earlier release sits
// in the same directory, and shipping it would publish the wrong binary under
// the right name.
const named = (suffix) =>
  produced.filter((file) => path.basename(file).includes(version) && file.endsWith(suffix));

// The updater downloads the installer itself, and verifies the signature the
// bundler wrote beside it. (Tauri 1 emitted a separate `.zip` for this; this
// version signs the bundle it produced, so there is one artifact and one
// signature rather than a package, its signature, and an installer.)
const installers = [...named(".msi"), ...named("-setup.exe")];

if (installers.length === 0) {
  fail(`no installer for version ${version} was produced (is the bundle step enabled?)`);
}
for (const installer of installers) {
  const signature = `${installer}.sig`;
  if (!fs.existsSync(signature)) {
    fail(
      `${path.basename(installer)} was produced without a signature, so an installed ` +
        `copy could never accept it as an update. Is createUpdaterArtifacts on?`,
    );
  }
  if (fs.readFileSync(signature, "utf8").trim().length === 0) {
    fail(`${path.basename(signature)} is empty`);
  }
}

// --- 6. Checksums and the manifest ------------------------------------------

step("Writing checksums");

fs.rmSync(releaseDir, { recursive: true, force: true });
fs.mkdirSync(releaseDir, { recursive: true });

const sha256 = (file) => createHash("sha256").update(fs.readFileSync(file)).digest("hex");

const shipped = installers.flatMap((installer) => [installer, `${installer}.sig`]);
const lines = shipped
  .map((file) => `${sha256(file)}  ${path.basename(file)}`)
  .sort();
fs.writeFileSync(path.join(releaseDir, "SHA256SUMS.txt"), `${lines.join("\n")}\n`);

// Everything the release consists of, copied beside its checksum so the
// directory can be uploaded as it stands.
for (const file of shipped) {
  fs.copyFileSync(file, path.join(releaseDir, path.basename(file)));
}

step("Writing the update manifest");

// The updater looks for `{os}-{arch}-{installer}` and falls back to
// `{os}-{arch}`; both installers are described so a copy installed by either one
// is offered the same release.
const platforms = {};
for (const [key, match] of [
  ["windows-x86_64-nsis", (f) => f.endsWith("-setup.exe")],
  ["windows-x86_64-msi", (f) => f.endsWith(".msi")],
]) {
  const installer = installers.find(match);
  if (!installer) continue;
  platforms[key] = {
    signature: fs.readFileSync(`${installer}.sig`, "utf8").trim(),
    url: `${base}${encodeURIComponent(path.basename(installer))}`,
  };
}

if (Object.keys(platforms).length === 0) {
  fail("no installer could be described in the update manifest");
}

const notesFile = path.join(root, "docs", "RELEASE_NOTES.md");
const manifest = {
  version,
  notes: fs.existsSync(notesFile) ? fs.readFileSync(notesFile, "utf8") : "",
  pub_date: new Date().toISOString().replace(/\.\d{3}Z$/, "Z"),
  platforms,
};
fs.writeFileSync(
  path.join(releaseDir, "latest.json"),
  `${JSON.stringify(manifest, null, 2)}\n`,
);

// --- 7. Say what was produced ----------------------------------------------

console.log(`\n=== Release ${version} is in ${path.relative(root, releaseDir)}/`);
for (const line of lines) {
  console.log(`  ${line}`);
}
console.log(`  ${path.basename(path.join(releaseDir, "latest.json"))} (channel manifest)`);
console.log(
  "\nNothing has been uploaded. Publishing is a separate, deliberate step; see\n" +
    "docs/RELEASE_PROCESS.md.",
);
