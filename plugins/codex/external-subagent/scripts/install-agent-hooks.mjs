#!/usr/bin/env node
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

function arg(name) {
  const index = process.argv.indexOf(name);
  const value = index >= 0 ? process.argv[index + 1] : undefined;
  if (!value || value.startsWith('--')) throw new Error('usage: install-agent-hooks.mjs --config <file> [--provenance <file>]');
  return value;
}
const configPath = path.resolve(arg('--config'));
const provenanceIndex = process.argv.indexOf('--provenance');
const provenancePath = path.resolve(provenanceIndex >= 0 ? arg('--provenance') : path.join(path.dirname(configPath), 'external-subagent-hook-provenance.json'));
if (configPath === provenancePath) throw new Error('config and provenance paths must differ');
let config = {};
if (fs.existsSync(configPath)) config = JSON.parse(fs.readFileSync(configPath, 'utf8'));
if (!config || Array.isArray(config) || typeof config !== 'object') throw new Error('config must be a JSON object');
const next = structuredClone(config);
next.hooks ??= {};
next.hooks.enabled = true;
next.hooks.events ??= {};
const root = path.resolve(new URL('..', import.meta.url).pathname);
const filePolicy = path.join(root, 'hooks', 'check-agent-files.mjs');
const audit = path.join(root, 'hooks', 'audit-bash-result.mjs');
const managed = {
  PreToolUse: [{ matcher: '^(Read|Grep|Glob|Write|Edit|Delete|Move)$', script: filePolicy }],
  PostToolUse: [{ matcher: 'Bash', script: audit }],
  PostToolUseFailure: [{ matcher: 'Bash', script: audit }],
};
for (const [event, entries] of Object.entries(managed)) {
  const existing = Array.isArray(next.hooks.events[event]) ? next.hooks.events[event] : [];
  const unrelated = existing.filter((entry) => !entries.some((item) => item.matcher === entry?.matcher));
  next.hooks.events[event] = [...unrelated, ...entries.map((entry) => ({ matcher: entry.matcher, hooks: [{ type: 'process', command: process.execPath, args: [entry.script], timeoutMs: 5000 }] }))];
}
const bytes = Buffer.from(`${JSON.stringify(next, null, 2)}\n`);
fs.mkdirSync(path.dirname(configPath), { recursive: true, mode: 0o700 });
fs.writeFileSync(configPath, bytes, { mode: 0o600 });
const digest = (file) => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const provenance = { schema_version: 1, product: 'external-subagent', effective_config_path: configPath, effective_config_sha256: crypto.createHash('sha256').update(bytes).digest('hex'), file_policy_path: filePolicy, file_policy_sha256: digest(filePolicy), audit_wrapper_path: audit, audit_wrapper_sha256: digest(audit), activation_method: 'outer-plugin-install' };
fs.mkdirSync(path.dirname(provenancePath), { recursive: true, mode: 0o700 });
fs.writeFileSync(provenancePath, `${JSON.stringify(provenance, null, 2)}\n`, { mode: 0o600 });
process.stdout.write(`${JSON.stringify({ config: configPath, provenance: provenancePath, file_policy_sha256: provenance.file_policy_sha256 })}\n`);
