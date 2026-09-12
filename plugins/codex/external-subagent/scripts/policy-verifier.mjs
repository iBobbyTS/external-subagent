#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';

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
if (!script || !fs.statSync(script).isFile()) process.exit(2);
const provenancePath = path.join(home, 'Library', 'Application Support', 'external-subagent', 'zcode-agent-hook-provenance.json');
if (!fs.existsSync(provenancePath)) process.exit(2);
const provenance = JSON.parse(fs.readFileSync(provenancePath, 'utf8'));
if (provenance.effective_config_path !== path.resolve(configPath) || provenance.hook_activation_verified !== true) process.exit(2);
process.exit(0);
