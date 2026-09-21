/**
 * Unit tests for the dsh-write-guard plugin.
 *
 * The plugin imports `@deepseek-ai/dsh-fs` by bare package name; at runtime the
 * daemon materializes a per-task `node_modules` symlink next to the plugin so
 * that import resolves into the host dsh install closure. A repository checkout
 * has no such node_modules, so this suite copies the checked-in plugin into a
 * temporary directory and creates the resolution symlink inside that copy. The
 * repository tree is never touched and the temporary directory is removed on
 * teardown.
 */
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test, { after } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PLUGIN_DIR = path.resolve(HERE, "..");
const LIB_SOURCE = path.join(PLUGIN_DIR, "lib", "index.js");
const REAL_DSH_FS =
  "/opt/homebrew/lib/node_modules/@deepseek-ai/dsh/node_modules/@deepseek-ai/dsh-fs";

const tempRoot = fs.mkdtempSync(path.join(os.tmpdir(), "dsh-write-guard-"));
after(() => {
  fs.rmSync(tempRoot, { recursive: true, force: true });
});

const copyDir = path.join(tempRoot, "dsh-write-guard");
const copyLibDir = path.join(copyDir, "lib");
fs.mkdirSync(copyLibDir, { recursive: true });
fs.copyFileSync(LIB_SOURCE, path.join(copyLibDir, "index.js"));
// The copy lives outside the checked-in package, so without this Node would
// have to infer the module format from syntax (automatic detection landed in
// Node 22.7). A minimal package.json pins the copied `.js` to ESM explicitly,
// keeping the suite valid on Node 20 as well.
fs.writeFileSync(
  path.join(copyDir, "package.json"),
  JSON.stringify({ type: "module" }),
);

const realFsAvailable = fs.existsSync(path.join(REAL_DSH_FS, "package.json"));
const scopeDir = path.join(copyDir, "node_modules", "@deepseek-ai");
fs.mkdirSync(scopeDir, { recursive: true });
const linkedFsDir = path.join(scopeDir, "dsh-fs");
if (realFsAvailable) {
  fs.symlinkSync(REAL_DSH_FS, linkedFsDir, "dir");
} else {
  // Degraded environment: a local stand-in keeps the module loadable; the real
  // `instanceof FsError` assertion below is skipped and the reason is reported.
  fs.mkdirSync(linkedFsDir, { recursive: true });
  fs.writeFileSync(
    path.join(linkedFsDir, "package.json"),
    JSON.stringify({
      name: "@deepseek-ai/dsh-fs",
      type: "module",
      main: "index.js",
      exports: { ".": "./index.js" },
    }),
  );
  fs.writeFileSync(
    path.join(linkedFsDir, "index.js"),
    "export class FsError extends Error {\n" +
      "  constructor(message, code, options) {\n" +
      "    super(message, options);\n" +
      "    this.code = code;\n" +
      "  }\n" +
      "}\n",
  );
}

const plugin = await import(pathToFileURL(path.join(copyLibDir, "index.js")).href);

let RealFsError = null;
if (realFsAvailable) {
  // Resolve the same bare specifier the plugin resolves, from the same
  // directory, so the imported class is the identical module instance.
  fs.writeFileSync(
    path.join(copyLibDir, "fs-probe.mjs"),
    'export { FsError } from "@deepseek-ai/dsh-fs";\n',
  );
  ({ FsError: RealFsError } = await import(
    pathToFileURL(path.join(copyLibDir, "fs-probe.mjs")).href
  ));
}

/**
 * A minimal cordis-context stand-in that records `on(event, listener, prepend)`
 * calls and maintains one listener registry per event with the real
 * push/unshift semantics.
 */
function createFakeCtx() {
  const registrations = [];
  const registries = new Map();
  const effects = [];
  const ctx = {
    on(event, listener, prepend) {
      registrations.push({ event, listener, prepend });
      if (!registries.has(event)) registries.set(event, []);
      const listeners = registries.get(event);
      if (prepend === true) listeners.unshift(listener);
      else listeners.push(listener);
      return () => {};
    },
    effect(callback, label) {
      effects.push({ callback, label });
      return () => {};
    },
  };
  return { ctx, registrations, registries, effects };
}

/**
 * Mirror cordis' first-decider waterfall: run listeners in registration order
 * until one returns without calling `next()`.
 */
function runWaterfall(registries, event, target, actor) {
  const listeners = (registries.get(event) ?? []).slice();
  const inner = () => ({ kind: "bare-provider" });
  const next = () => (listeners.shift() ?? inner)(target, actor, next);
  return next();
}

function makeGuard(manifest) {
  const harness = createFakeCtx();
  plugin.apply(harness.ctx, { manifest });
  return harness;
}

function targetFor(displayPath) {
  return { displayPath, targetKey: displayPath };
}

function decide(harness, event, displayPath) {
  return runWaterfall(harness.registries, event, targetFor(displayPath), {});
}

function decideTarget(harness, event, target) {
  return runWaterfall(harness.registries, event, target, {});
}

function assertDenied(fn, displayPath) {
  assert.throws(fn, (error) => {
    assert.equal(error.code, "FS_WRITE_MANIFEST_DENIED");
    assert.match(error.message, /^\[write-guard\]/);
    assert.ok(
      error.message.includes(displayPath),
      `message should quote ${displayPath}: ${error.message}`,
    );
    return true;
  });
}

test("exports the cordis plugin shape", () => {
  assert.equal(plugin.name, "dsh-write-guard");
  assert.equal(typeof plugin.apply, "function");
});

test("exercises the checked-in artifact (temporary copy is byte-identical)", () => {
  assert.equal(
    fs.readFileSync(LIB_SOURCE, "utf8"),
    fs.readFileSync(path.join(copyLibDir, "index.js"), "utf8"),
  );
});

test("pins the temporary copy to ESM via a minimal package.json (Node 20 parity)", () => {
  const copyPackageJson = path.join(copyDir, "package.json");
  assert.ok(fs.existsSync(copyPackageJson), "temporary copy must carry a package.json");
  assert.deepEqual(JSON.parse(fs.readFileSync(copyPackageJson, "utf8")), {
    type: "module",
  });
});

test("registers both intent listeners with prepend === true", () => {
  const { registrations } = makeGuard(["docs"]);

  const write = registrations.filter((r) => r.event === "fs/write-intent");
  const edit = registrations.filter((r) => r.event === "fs/edit-intent");

  assert.equal(write.length, 1);
  assert.equal(write[0].prepend, true);
  assert.equal(edit.length, 1);
  assert.equal(edit[0].prepend, true);
  // `ctx.on(event, listener, true)` is exactly the boolean prepend shorthand.
  assert.equal(registrations.length, 2);
});

test("registers a teardown effect", () => {
  const { effects } = makeGuard(["docs"]);
  assert.equal(effects.length, 1);
  assert.equal(effects[0].label, "dsh-write-guard teardown");
  assert.equal(typeof effects[0].callback, "function");
});

test("prepends ahead of an existing first-decider and passes through to it", () => {
  const harness = createFakeCtx();
  const seen = [];
  const existingFirstDecider = () => {
    seen.push("existing");
    return { kind: "existing-decision" };
  };
  harness.ctx.on("fs/write-intent", existingFirstDecider);

  plugin.apply(harness.ctx, { manifest: ["docs"] });

  assert.notEqual(
    harness.registries.get("fs/write-intent")[0],
    existingFirstDecider,
    "guard must be registered before the pre-existing decider",
  );

  // Denied: the guard throws before the pre-existing decider is consulted.
  assertDenied(
    () => decide(harness, "fs/write-intent", path.join(process.cwd(), "secret.txt")),
    path.join(process.cwd(), "secret.txt"),
  );
  assert.deepEqual(seen, []);

  // Allowed: `next()` reaches the pre-existing decider and returns its decision.
  const decision = decide(
    harness,
    "fs/write-intent",
    path.join(process.cwd(), "docs", "a.txt"),
  );
  assert.deepEqual(decision, { kind: "existing-decision" });
  assert.deepEqual(seen, ["existing"]);
});

test("applies the same first-decider behavior to fs/edit-intent", () => {
  const harness = createFakeCtx();
  const seen = [];
  const existingFirstDecider = () => {
    seen.push("existing");
    return { version: "v1" };
  };
  harness.ctx.on("fs/edit-intent", existingFirstDecider);

  plugin.apply(harness.ctx, { manifest: ["docs"] });

  assertDenied(
    () => decide(harness, "fs/edit-intent", path.join(process.cwd(), "secret.txt")),
    path.join(process.cwd(), "secret.txt"),
  );
  assert.deepEqual(seen, []);

  const decision = decide(
    harness,
    "fs/edit-intent",
    path.join(process.cwd(), "docs", "a.txt"),
  );
  assert.deepEqual(decision, { version: "v1" });
  assert.deepEqual(seen, ["existing"]);
});

test("allows a target equal to a root and returns next()'s result", () => {
  const harness = makeGuard(["docs", "src/a.rs"]);
  const root = path.join(process.cwd(), "src", "a.rs");
  assert.deepEqual(decide(harness, "fs/write-intent", root), { kind: "bare-provider" });
  assert.deepEqual(decide(harness, "fs/edit-intent", root), { kind: "bare-provider" });
});

test("allows nested descendants of every root", () => {
  const harness = makeGuard(["docs", "src"]);
  const nestedDocs = path.join(process.cwd(), "docs", "a", "b.txt");
  const nestedSrc = path.join(process.cwd(), "src", "deep", "c.rs");
  assert.deepEqual(decide(harness, "fs/write-intent", nestedDocs), { kind: "bare-provider" });
  assert.deepEqual(decide(harness, "fs/write-intent", nestedSrc), { kind: "bare-provider" });
});

test("rejects sibling paths that only share a string prefix (docs vs docs-x)", () => {
  const harness = makeGuard(["docs"]);
  const sibling = path.join(process.cwd(), "docs-x");
  const siblingChild = path.join(process.cwd(), "docs-x", "y.txt");
  assertDenied(() => decide(harness, "fs/write-intent", sibling), sibling);
  assertDenied(() => decide(harness, "fs/write-intent", siblingChild), siblingChild);
  assertDenied(
    () => decide(harness, "fs/write-intent", path.join(process.cwd(), "docsx", "y")),
    path.join(process.cwd(), "docsx", "y"),
  );
});

test("treats a trailing-separator entry exactly like its separator-less form", () => {
  const harness = makeGuard(["docs/"]);
  const root = path.join(process.cwd(), "docs");
  const nested = path.join(process.cwd(), "docs", "a", "b.txt");
  const sibling = path.join(process.cwd(), "docs-x");
  const siblingChild = path.join(process.cwd(), "docs-x", "y.txt");

  // Equal-to-root and nested descendants are allowed, on both waterfalls.
  assert.deepEqual(decide(harness, "fs/write-intent", root), { kind: "bare-provider" });
  assert.deepEqual(decide(harness, "fs/edit-intent", root), { kind: "bare-provider" });
  assert.deepEqual(decide(harness, "fs/write-intent", nested), { kind: "bare-provider" });
  assert.deepEqual(decide(harness, "fs/edit-intent", nested), { kind: "bare-provider" });

  // Normalizing the root must not widen containment to a sibling prefix.
  assertDenied(() => decide(harness, "fs/write-intent", sibling), sibling);
  assertDenied(() => decide(harness, "fs/write-intent", siblingChild), siblingChild);
});

test("roots a relative entry at process.cwd()", () => {
  const harness = makeGuard(["nested/dir"]);
  const under = path.join(process.cwd(), "nested", "dir", "file.txt");
  assert.deepEqual(decide(harness, "fs/write-intent", under), { kind: "bare-provider" });

  const elsewhere = path.join(process.cwd(), "..", "nested", "dir", "file.txt");
  assertDenied(() => decide(harness, "fs/write-intent", elsewhere), elsewhere);
});

test("joins absolute-looking entries lexically under cwd and still contains", () => {
  // The manifest contract is repository-relative, so an entry that merely looks
  // absolute (`/etc`) is joined *lexically* under process.cwd() (`path.join`),
  // never resolved against the filesystem root (`path.resolve`).
  const harness = makeGuard(["/etc"]);
  const joinedUnderCwd = path.join(process.cwd(), "etc", "x");

  // (a) join semantics: the entry roots at <cwd>/etc, so <cwd>/etc/x is allowed.
  //     A resolve-based implementation would root at /etc and reject this,
  //     which is exactly the regression this assertion pins.
  assert.deepEqual(decide(harness, "fs/write-intent", joinedUnderCwd), {
    kind: "bare-provider",
  });

  // (b) the displayPath side is still contained: the real /etc/x lies outside
  //     <cwd>/etc and is rejected (a resolve-based implementation would allow it).
  const outside = path.join(path.sep, "etc", "x");
  assertDenied(() => decide(harness, "fs/write-intent", outside), outside);
});

test("rejection is a typed FsError with code FS_WRITE_MANIFEST_DENIED", () => {
  const harness = makeGuard(["docs"]);
  const deniedPath = path.join(process.cwd(), "secret.txt");
  let captured = null;
  try {
    decide(harness, "fs/write-intent", deniedPath);
  } catch (error) {
    captured = error;
  }
  assert.ok(captured, "expected the guard to throw");
  assert.equal(captured.code, "FS_WRITE_MANIFEST_DENIED");
  assert.equal(
    captured.message,
    `[write-guard] cannot write "${deniedPath}": outside the task write manifest`,
  );
});

test("rejects with a stable placeholder when displayPath is missing or non-string", () => {
  const harness = makeGuard(["docs"]);
  const expected =
    '[write-guard] cannot write "<unresolved>": outside the task write manifest';

  const cases = [
    ["missing displayPath", { targetKey: "k" }],
    ["undefined displayPath", { displayPath: undefined }],
    ["null displayPath", { displayPath: null }],
    ["numeric displayPath", { displayPath: 42 }],
    ["object displayPath", { displayPath: { path: "/ws/docs" } }],
    ["missing target", undefined],
  ];
  for (const [label, target] of cases) {
    assert.throws(
      () => decideTarget(harness, "fs/write-intent", target),
      (error) => {
        assert.equal(error.code, "FS_WRITE_MANIFEST_DENIED", label);
        assert.equal(error.message, expected, label);
        return true;
      },
      label,
    );
  }
});

test(
  "rejection is an instanceof the real @deepseek-ai/dsh-fs FsError",
  { skip: realFsAvailable ? false : `real dsh install not found at ${REAL_DSH_FS}` },
  () => {
    const harness = makeGuard(["docs"]);
    const deniedPath = path.join(process.cwd(), "secret.txt");
    assert.throws(
      () => decide(harness, "fs/write-intent", deniedPath),
      (error) => {
        assert.equal(error.code, "FS_WRITE_MANIFEST_DENIED");
        assert.ok(error instanceof RealFsError, "must be a real FsError instance");
        return true;
      },
    );
  },
);

test("config fail-loud: every malformed manifest throws at load time", () => {
  const cases = [
    ["missing config", undefined],
    ["null config", null],
    ["missing manifest", {}],
    ["non-array manifest", { manifest: "docs" }],
    ["empty manifest", { manifest: [] }],
    ["empty-string entry", { manifest: ["docs", ""] }],
    ["non-string entry", { manifest: ["docs", 7] }],
    ["NUL entry", { manifest: ["docs\u0000x"] }],
    ["257 entries", { manifest: Array.from({ length: 257 }, (_, i) => `f${i}`) }],
    ["over 64KiB serialized", { manifest: ["a".repeat(64 * 1024 + 1)] }],
  ];
  for (const [label, config] of cases) {
    const { ctx } = createFakeCtx();
    assert.throws(
      () => plugin.apply(ctx, config),
      (error) => {
        assert.match(error.message, /\[write-guard\]/, label);
        return true;
      },
      label,
    );
  }
});

test("config accepts the boundary limits (256 entries, exactly 64KiB)", () => {
  const maxEntries = Array.from({ length: 256 }, (_, i) => `f${i}`);
  const harness = createFakeCtx();
  plugin.apply(harness.ctx, { manifest: maxEntries });
  assert.equal(harness.registrations.length, 2);

  // JSON.stringify(["a".repeat(65532)]) is exactly 65536 bytes.
  const exactBytes = ["a".repeat(64 * 1024 - 4)];
  const harness2 = createFakeCtx();
  plugin.apply(harness2.ctx, { manifest: exactBytes });
  assert.equal(harness2.registrations.length, 2);
});
