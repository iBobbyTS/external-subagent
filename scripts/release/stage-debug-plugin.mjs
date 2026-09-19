#!/usr/bin/env node
// Stage the debug-variant plugin source at plugins/codex/external-subagent-debug
// from the release source at plugins/codex/external-subagent: a byte-level copy
// with the plugin identity renamed (plugin.json name/displayName, the skill
// directory and its frontmatter name).  The .mcp.json placeholder and the
// hooks/scripts/lib payloads stay identical — the host bindings rewrite the
// placeholder at staging time, and tool names are the daemon's, not the
// plugin's.  The generated tree is a build artifact (gitignored) owned by
// build-native-payload.mjs --variant debug.
import fs from 'node:fs';
import path from 'node:path';

const DEBUG_PLUGIN_NAME = 'external-subagent-debug';
const SOURCE_PLUGIN_NAME = 'external-subagent';

export function stageDebugPlugin(packageRoot) {
  const source = path.join(packageRoot, 'plugins', 'codex', SOURCE_PLUGIN_NAME);
  const target = path.join(packageRoot, 'plugins', 'codex', DEBUG_PLUGIN_NAME);
  if (!fs.existsSync(path.join(source, '.codex-plugin', 'plugin.json'))) {
    throw new Error(`release plugin source is missing: ${source}`);
  }
  fs.rmSync(target, { recursive: true, force: true });
  fs.cpSync(source, target, { recursive: true });
  const rmJunk = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const child = path.join(dir, entry.name);
      if (entry.isDirectory()) rmJunk(child);
      else if (entry.name === '.DS_Store') fs.rmSync(child);
    }
  };
  rmJunk(target);

  const manifestPath = path.join(target, '.codex-plugin', 'plugin.json');
  const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
  manifest.name = DEBUG_PLUGIN_NAME;
  manifest.interface = {
    ...manifest.interface,
    displayName: `${manifest.interface?.displayName ?? SOURCE_PLUGIN_NAME} Debug`,
    shortDescription: `Debug instance: ${manifest.interface?.shortDescription ?? ''}`.trim(),
  };
  fs.writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);

  fs.renameSync(path.join(target, 'skills', SOURCE_PLUGIN_NAME), path.join(target, 'skills', DEBUG_PLUGIN_NAME));
  const skillPath = path.join(target, 'skills', DEBUG_PLUGIN_NAME, 'SKILL.md');
  const skill = fs.readFileSync(skillPath, 'utf8');
  const rewritten = skill.replace(`name: ${SOURCE_PLUGIN_NAME}\n`, `name: ${DEBUG_PLUGIN_NAME}\n`, 1);
  fs.writeFileSync(skillPath, rewritten);
  return target;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const packageRoot = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..', '..');
  try {
    const target = stageDebugPlugin(packageRoot);
    process.stdout.write(`debug plugin source staged: ${target}\n`);
  } catch (error) {
    process.stderr.write(`${error.message}\n`);
    process.exit(1);
  }
}
