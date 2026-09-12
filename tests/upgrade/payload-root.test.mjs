import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { verifyPayload } from '../../cli/install/payload.mjs';

test('verifyPayload validates an independent candidate root', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'candidate-'));
  const dir = path.join(root, 'npm/native/darwin-arm64'); fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ version: '2.0.0' }));
  const files = ['external-subagentd', 'external-subagent-mcp'].map((name) => {
    const bytes = Buffer.alloc(32); bytes.writeUInt32LE(0xfeedfacf); bytes.writeUInt32LE(0x0100000c, 4);
    fs.writeFileSync(path.join(dir, name), bytes, { mode: 0o755 });
    return { name, bytes: bytes.length, sha256: crypto.createHash('sha256').update(bytes).digest('hex') };
  });
  fs.writeFileSync(path.join(dir, 'payload.json'), JSON.stringify({ schema_version: 1, product: 'external-subagent', platform: 'darwin-arm64', version: '2.0.0', files }));
  const result = verifyPayload({ root, platform: 'darwin-arm64' });
  assert.equal(result.version, '2.0.0');
  fs.rmSync(root, { recursive: true, force: true });
});
