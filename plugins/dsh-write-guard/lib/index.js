/**
 * dsh-write-guard — per-task write-manifest guard for the dsh filesystem seam.
 *
 * The daemon materializes this package per task (embedded at build time, written
 * to a TempDir on spawn) and inserts it into the dsh plugin tree with a
 * `config.manifest` list. The guard turns that list into a lexical containment
 * gate on `fs/write-intent` and `fs/edit-intent`, throwing a typed `FsError`
 * before an out-of-manifest mutation can reach the provider. It registers no
 * service, observes nothing, and writes nothing.
 *
 * The import below keeps the bare package name on purpose: at runtime a
 * `node_modules/@deepseek-ai/dsh-fs` symlink next to this file resolves it into
 * the host dsh install closure. The checked-in file is never rewritten.
 *
 * @module dsh-write-guard
 */
import path from "node:path";
import { FsError } from "@deepseek-ai/dsh-fs";

/** Cordis plugin name used by loader diagnostics. */
export const name = "dsh-write-guard";

/** Hard cap on manifest entry count (mirrors the daemon-side task contract). */
const MAX_MANIFEST_ENTRIES = 256;

/** Hard cap on the JSON-serialized manifest size. */
const MAX_MANIFEST_BYTES = 64 * 1024;

/**
 * Validate the `config.manifest` supplied through the patch insert line.
 * Fail-loud: a malformed manifest aborts plugin load instead of degrading to
 * "allow everything".
 *
 * @param {unknown} config - the second argument the cordis loader passes to
 *   {@link apply}; `config.manifest` is the repository-relative write manifest.
 * @returns {string[]} the validated entries (defensive copy).
 */
function requireManifest(config) {
  const manifest = config?.manifest;
  if (!Array.isArray(manifest)) {
    throw new Error("[write-guard] config.manifest must be an array of strings");
  }
  if (manifest.length === 0) {
    throw new Error("[write-guard] config.manifest must not be empty");
  }
  if (manifest.length > MAX_MANIFEST_ENTRIES) {
    throw new Error(
      `[write-guard] config.manifest must contain at most ${MAX_MANIFEST_ENTRIES} entries, got ${manifest.length}`,
    );
  }
  for (const entry of manifest) {
    if (typeof entry !== "string" || entry.length === 0) {
      throw new Error("[write-guard] config.manifest entries must be non-empty strings");
    }
    if (entry.includes("\0")) {
      throw new Error(
        `[write-guard] config.manifest entries must not contain NUL: ${JSON.stringify(entry)}`,
      );
    }
  }
  const serializedBytes = Buffer.byteLength(JSON.stringify(manifest), "utf8");
  if (serializedBytes > MAX_MANIFEST_BYTES) {
    throw new Error(
      `[write-guard] config.manifest must serialize to at most ${MAX_MANIFEST_BYTES} bytes, got ${serializedBytes}`,
    );
  }
  return manifest.slice();
}

/**
 * Separator-aware lexical containment: a target is inside a root when it equals
 * the root or sits under `root + path.sep`. A sibling that merely shares a
 * string prefix (`docs` vs `docs-x`) does not match.
 *
 * @param {string} displayPath - absolute lexical target path from `FsTarget`.
 * @param {readonly string[]} roots - absolute lexical roots.
 * @returns {boolean}
 */
function isWithin(displayPath, roots) {
  for (const root of roots) {
    if (displayPath === root) return true;
    if (displayPath.startsWith(root + path.sep)) return true;
  }
  return false;
}

/**
 * Drop trailing path separators from a lexical root while preserving the
 * filesystem root's meaning (`"/"` stays `"/"`, `"C:\\"` stays `"C:\\"`).
 * Without this, a manifest entry like `"docs/"` keeps its trailing separator
 * through `path.join`, so containment probes `root + path.sep` (a doubled
 * separator) and never matches a `displayPath`, which never has a trailing
 * separator. Normalizing here makes `"docs/"` behave exactly like `"docs"`.
 *
 * @param {string} root - absolute lexical root.
 * @returns {string} the root with any trailing separators removed.
 */
function stripTrailingSeparator(root) {
  const filesystemRoot = path.parse(root).root;
  let trimmed = root;
  while (trimmed.length > filesystemRoot.length && trimmed.endsWith(path.sep)) {
    trimmed = trimmed.slice(0, -1);
  }
  return trimmed;
}

/**
 * Register the write-manifest guard on the two filesystem intent waterfalls.
 *
 * Both listeners prepend (`ctx.on(event, listener, true)`) because `fs/write-
 * intent` and `fs/edit-intent` are first-decider waterfalls: the first listener
 * to return a decision owns it and the rest are never consulted. A guard
 * appended after the base composition's observation policy would never run.
 * Allowed paths are passed through with `next()` so later deciders still apply;
 * denied paths throw a real `FsError`.
 *
 * @param {import("@deepseek-ai/cordis").Context} ctx - plugin context.
 * @param {{ manifest: string[] }} config - insert-line config.
 */
export function apply(ctx, config) {
  const manifest = requireManifest(config);
  const roots = manifest.map((entry) =>
    stripTrailingSeparator(path.join(process.cwd(), entry)),
  );

  const guard = (target, _exec, next) => {
    const displayPath = target?.displayPath;
    if (typeof displayPath === "string" && isWithin(displayPath, roots)) {
      return next();
    }
    // A missing/non-string displayPath is still fail-closed; render a stable
    // placeholder instead of leaking `"undefined"` into the rejection message.
    const renderedPath = typeof displayPath === "string" ? displayPath : "<unresolved>";
    throw new FsError(
      `[write-guard] cannot write "${renderedPath}": outside the task write manifest`,
      "FS_WRITE_MANIFEST_DENIED",
    );
  };

  ctx.on("fs/write-intent", guard, true);
  ctx.on("fs/edit-intent", guard, true);

  ctx.effect(() => () => {}, "dsh-write-guard teardown");
}
