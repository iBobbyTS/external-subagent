#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';

function value(name) {
  const index = process.argv.indexOf(name);
  const result = index >= 0 ? process.argv[index + 1] : undefined;
  if (!result || result.startsWith('--')) process.exit(2);
  return result;
}

const workspace = path.resolve(value('--workspace'));
const home = path.resolve(value('--home'));
const mode = value('--permission-mode');
const manifest = value('--write-manifest');
if (mode !== 'plan' || manifest !== '[]' || !fs.statSync(workspace).isDirectory()) process.exit(2);
const configPath = path.join(home, '.zcode', 'cli', 'config.json');
const config = JSON.parse(fs.readFileSync(configPath, 'utf8'));
const events = config?.hooks?.events;
if (config?.hooks?.enabled !== true || !events || !Array.isArray(events.PreToolUse)) process.exit(2);
const pre = events.PreToolUse.find((entry) => entry?.matcher === '^(Read|Grep|Glob|Write|Edit|Delete|Move)$');
const hook = pre?.hooks?.find((entry) => entry?.type === 'process' && Array.isArray(entry.args));
const script = hook?.args?.find((candidate) => typeof candidate === 'string' && candidate.endsWith('/hooks/check-agent-files.mjs'));
const guard = events.PreToolUse.find((entry) => entry?.matcher === 'Bash')?.hooks?.find((entry) => entry?.type === 'process')?.args?.[0];
const audit = events.PostToolUse?.find((entry) => entry?.matcher === 'Bash')?.hooks?.find((entry) => entry?.type === 'process')?.args?.[0];
if (!script || !fs.statSync(script).isFile() || hook.command !== process.execPath || !guard || !fs.statSync(guard).isFile() || !audit || !fs.statSync(audit).isFile()) process.exit(2);
// The provenance is staged by install-agent-hooks.mjs beside this verifier,
// inside the product data directory of the probe home. The candidates mirror
// the daemon's policy_verifier_candidates exactly: the frozen macOS
// `~/Library/Application Support` bytes on darwin; elsewhere an exported
// absolute $XDG_DATA_HOME is probed first, then the `~/.local/share` XDG
// fallback for the scope home.
const provenanceCandidates = process.platform === 'darwin'
  ? [path.join(home, 'Library', 'Application Support', 'external-subagent', 'zcode-agent-hook-provenance.json')]
  : [
      ...(typeof process.env.XDG_DATA_HOME === 'string' && path.isAbsolute(process.env.XDG_DATA_HOME)
        ? [path.join(process.env.XDG_DATA_HOME, 'external-subagent', 'zcode-agent-hook-provenance.json')]
        : []),
      path.join(home, '.local', 'share', 'external-subagent', 'zcode-agent-hook-provenance.json'),
    ];
const provenancePath = provenanceCandidates.find((candidate) => fs.existsSync(candidate));
if (!provenancePath) process.exit(2);
const provenance = JSON.parse(fs.readFileSync(provenancePath, 'utf8'));
const hash = (file) => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const policy = path.resolve(path.dirname(script), '../lib/agent-file-policy.mjs');
if (provenance.effective_config_path !== path.resolve(configPath)
  || provenance.hook_activation_verified !== true
  || provenance.effective_file_policy_version !== 'zcode-agent-file-policy/v1.0.0'
  || provenance.effective_file_policy_path !== policy
  || provenance.effective_file_policy_sha256 !== hash(policy)
  || provenance.effective_guard_wrapper_path !== path.resolve(guard)
  || provenance.effective_guard_wrapper_sha256 !== hash(guard)
  || provenance.effective_file_wrapper_path !== path.resolve(script)
  || provenance.effective_file_wrapper_sha256 !== hash(script)
  || provenance.effective_audit_wrapper_path !== path.resolve(audit)
  || provenance.effective_audit_wrapper_sha256 !== hash(audit)) process.exit(2);
process.exit(0);
