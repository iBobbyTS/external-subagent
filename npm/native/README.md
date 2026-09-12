# Native payload

`npm/native/darwin-arm64/` carries the prebuilt native payload for the npm
package: the `external-subagentd` daemon and the `external-subagent-mcp`
stdio facade, plus the generated `payload.json` manifest (product version,
platform, and per-file bytes/sha256/mode).

The directory is build output, not source:

```
node scripts/release/build-native-payload.mjs           # build + manifest
node scripts/release/build-native-payload.mjs --if-stale
```

Supported platforms install without compiling Rust; the installer verifies
the payload manifest, digest, permissions (755), and Mach-O arm64 image
before any `init` step runs.  There is no payload for other platforms in
S05; unsupported platforms keep `help`/`version` working and reject business
commands without writing HOME.
