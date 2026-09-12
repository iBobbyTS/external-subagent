import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { verifyPayload } from '../../cli/install/payload.mjs';
import { updateInstallation } from '../../cli/install/update.mjs';

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

test('updateInstallation switches between two independent candidate roots', () => {
  const make = (version) => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'candidate-update-'));
    fs.mkdirSync(path.join(root, 'npm/native/darwin-arm64'), { recursive: true });
    fs.mkdirSync(path.join(root, 'bin'), { recursive: true }); fs.writeFileSync(path.join(root, 'bin/external-subagent.mjs'), '#!/usr/bin/env node\n');
    fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ version }));
    const files = ['external-subagentd', 'external-subagent-mcp'].map((name) => { const b=Buffer.alloc(32); b.writeUInt32LE(0xfeedfacf); b.writeUInt32LE(0x0100000c,4); fs.writeFileSync(path.join(root,'npm/native/darwin-arm64',name),b,{mode:0o755}); return {name,bytes:b.length,sha256:crypto.createHash('sha256').update(b).digest('hex')}; });
    fs.writeFileSync(path.join(root,'npm/native/darwin-arm64/payload.json'),JSON.stringify({schema_version:1,product:'external-subagent',platform:'darwin-arm64',version,files})); return root;
  };
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'update-state-')); const p={data,state:path.join(data,'state.json')};
  const a=make('1.0.0'), b=make('2.0.0');
  const opts = (version, root) => ({version,candidateRoot:root,platform:'darwin-arm64',availableVersions:['1.0.0','2.0.0']});
  assert.equal(updateInstallation(p,opts('1.0.0',a)).active.root,a);
  assert.equal(updateInstallation(p,opts('2.0.0',b)).active.root,b);
  fs.rmSync(a,{recursive:true,force:true}); fs.rmSync(b,{recursive:true,force:true}); fs.rmSync(data,{recursive:true,force:true});
});
