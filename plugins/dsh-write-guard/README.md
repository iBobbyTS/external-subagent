# dsh-write-guard

A private, self-contained cordis plugin for [dsh](https://github.com/deepseek-ai/deepseek-harness)
that confines filesystem mutations to a per-task **write manifest**. It
registers two first-decider listeners on the dsh filesystem waterfall and
rejects any write or edit whose resolved target falls outside the manifest.

The package is **check-in-is-artifact**: `lib/index.js` is hand-written ESM with
no build step, no `dependencies`, and no `scripts`. It is not published
(`private: true`); the daemon embeds it into the `external-subagent` binary and
materializes it per task at spawn time.

## Composition

The plugin is appended to the dsh plugin tree with an **insert** entry (an
override entry for a name the base composition does not contain is silently
skipped by `cordis-plugin-include`, so the `insert` form is required):

```yaml
- insert:
    - name: /<task-tempdir>/dsh-write-guard/lib/index.js
      config:
        manifest: ["src/a.rs", "docs/"]
```

The `insert` entry's `config` object is passed verbatim as the **second
argument** to `apply(ctx, config)`. This is the config mechanism used by the
guard; it was verified against a real dsh boot with a minimal guard plugin
(`.agent-work/tmp/r2-plan-review/good.patch.yml` + `boot-good2.txt`).

## Config schema

| Field | Type | Required | Contract |
| --- | --- | --- | --- |
| `manifest` | `string[]` | yes | Non-empty, ≤ 256 entries, each a non-empty string with no NUL, and `JSON.stringify(manifest) ≤ 64 KiB`. Repository-relative paths. |

A malformed manifest is **fail-loud**: `apply()` throws at load time so the
plugin tree fails to load instead of silently degrading to "allow everything".
The daemon already guarantees a non-empty manifest, but the plugin defends
itself.

## Semantics

- Roots are
  `config.manifest.map((entry) => stripTrailingSeparator(path.join(process.cwd(), entry)))`
  — a purely lexical join of each repository-relative entry against the task
  working directory, with any trailing `path.sep` stripped. So a `"docs/"`
  entry and a `"docs"` entry behave identically (the filesystem root itself is
  preserved: `"/"` stays `"/"`).
- Containment is separator-aware: a target is allowed when its `displayPath`
  equals a root, or starts with `root + path.sep`. The sibling `docs-x` does
  **not** match the root `docs`.
- Both listeners are registered with **prepend**:
  `ctx.on("fs/write-intent", guard, true)` and
  `ctx.on("fs/edit-intent", guard, true)`. These events are single-slot
  first-decider waterfalls: the first listener that returns a decision owns it
  and later listeners are never consulted. An appended guard would never run
  once the base composition's `fs-observation-policy` (which always returns a
  decision) is registered.
- Allowed targets call `next()` so subsequent deciders (for example
  `fs-observation-policy`) still make their decision. The guard never swallows
  them.
- Denied targets throw a real `FsError`:

  ```js
  new FsError(
    '[write-guard] cannot write "<path>": outside the task write manifest',
    "FS_WRITE_MANIFEST_DENIED",
  )
  ```

  When `displayPath` is missing or not a string, the message renders the
  placeholder `<unresolved>` in place of `"<path>"`; the rejection itself is
  unchanged (still fail-closed, same `FS_WRITE_MANIFEST_DENIED` code).

  The tool layer renders this as a structured rejection. The guard runs before
  `writeText`/`editText` and outside the sandbox escalation path, so a denied
  path cannot be widened by sandbox retry.

### Rejection sample

With `manifest: ["docs"]` and `process.cwd() === /ws`:

```text
deny  /ws/secret.txt   -> FsError code FS_WRITE_MANIFEST_DENIED
deny  /ws/docs-x/a.txt -> FsError code FS_WRITE_MANIFEST_DENIED
allow /ws/docs/a.txt   -> next() -> observation-policy decision
allow /ws/docs         -> next()
```

The guard registers no service, observes nothing on `fs/observed`, and writes
no files. It only clears its (empty) state through a `ctx.effect` teardown hook
for shape parity with `fs-observation-policy` and HMR safety.

## Deployment note

`lib/index.js` imports `FsError` by its **bare** package name:

```js
import { FsError } from "@deepseek-ai/dsh-fs";
```

A bare Node import from this file resolves by walking up `node_modules`
directories; without a matching entry it is an `ERR_MODULE_NOT_FOUND`. The
daemon therefore materializes, inside the per-task plugin package directory:

```text
<tempdir>/dsh-write-guard/node_modules/@deepseek-ai/dsh-fs
    -> <host dsh install>/node_modules/@deepseek-ai/dsh-fs
```

so the bare import resolves into the **host dsh install closure**, and
`instanceof FsError` works against the same class `dsh-tool-fs` uses. The
checked-in file is never rewritten. `@deepseek-ai/dsh-fs` is a runtime peer
(for `FsError`); `@deepseek-ai/cordis` is a peer for type semantics only. Both
are declared in `peerDependencies` and are intentionally not installed here —
they come from the host closure via that symlink.

## Self-contained / removable

The package is a standalone directory under `plugins/` with no imports from any
other repository file. Copying or deleting `plugins/dsh-write-guard/` in one
piece changes nothing else in the repository; the only consumer is the daemon's
embedding/patch construction, which reads `package.json` and `lib/index.js`
byte-for-byte.

## Tests

```bash
node --check plugins/dsh-write-guard/lib/index.js
node --check plugins/dsh-write-guard/test/index.test.mjs
node --test  plugins/dsh-write-guard/test/index.test.mjs
```

`test/index.test.mjs` uses `node:test` with a fake cordis context. Because a
repository checkout has no `node_modules`, the suite copies the checked-in
plugin into a temporary directory and creates the resolution symlink inside the
copy (never in the repository tree) so the module can load. If the host dsh
install is absent it falls back to a local `FsError` stand-in, the real
`instanceof` assertion is skipped (with the reason reported), and every other
test still runs. The temporary directory is removed on teardown.
