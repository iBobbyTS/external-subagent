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

const failClosed = (code, message, details = {}) => {
  process.stderr.write(`${JSON.stringify({ code, error: message, ...details })}\n`);
  process.exit(2);
};
const digestBytes = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');
const digestFile = (file) => digestBytes(fs.readFileSync(file));

// No verified ZCode policy implementation is shipped yet. Refuse to create
// placeholder `{ ok: true }` hooks or mutate any existing policy configuration.
if (!fs.existsSync(configPath)) failClosed('HOOK_POLICY_UNSUPPORTED', 'real ZCode policy hooks are unavailable; refusing to create placeholder hooks', { config: configPath, provenance: provenancePath });
const configBytes = fs.readFileSync(configPath);
let config;
try { config = JSON.parse(configBytes); } catch (error) { failClosed('HOOK_CONFIG_INVALID', `config is not valid JSON: ${error.message}`, { config: configPath }); }
if (!config || Array.isArray(config) || typeof config !== 'object') failClosed('HOOK_CONFIG_INVALID', 'config must be a JSON object', { config: configPath });

const root = path.resolve(new URL('..', import.meta.url).pathname);
const filePolicy = path.join(root, 'hooks', 'check-agent-files.mjs');
const audit = path.join(root, 'hooks', 'audit-bash-result.mjs');
if (fs.existsSync(provenancePath)) {
  let provenance;
  try { provenance = JSON.parse(fs.readFileSync(provenancePath, 'utf8')); } catch (error) { failClosed('HOOK_PROVENANCE_INVALID', `provenance is not valid JSON: ${error.message}`, { config: configPath, provenance: provenancePath }); }
  const expected = { product: 'external-subagent', effective_config_path: configPath, effective_config_sha256: digestBytes(configBytes), file_policy_path: filePolicy, file_policy_sha256: digestFile(filePolicy), audit_wrapper_path: audit, audit_wrapper_sha256: digestFile(audit) };
  const mismatch = Object.entries(expected).find(([key, value]) => provenance?.[key] !== value);
  if (mismatch) failClosed('HOOK_PROVENANCE_INVALID', `hook provenance does not match verifier field ${mismatch[0]}`, { config: configPath, provenance: provenancePath, field: mismatch[0] });
}
failClosed('HOOK_POLICY_UNSUPPORTED', 'real ZCode policy hooks and a matching verifier are unavailable; refusing to modify existing configuration', { config: configPath, provenance: provenancePath });
