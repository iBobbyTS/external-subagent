import test from 'node:test';
import assert from 'node:assert/strict';
import { access } from 'node:fs/promises';

test('external namespace exposes ZCode walking-path components', async () => {
  for (const path of [
    'crates/external-daemon/src/lib.rs',
    'crates/external-mcp/src/lib.rs',
    'crates/external-runtime/src/lib.rs',
    'crates/external-store/src/lib.rs',
    'schema/zas-observation-v1.1.schema.json',
  ]) await access(path);
  assert.ok(true);
});
