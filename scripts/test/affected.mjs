#!/usr/bin/env node
// Selective test runner: map the changed surface onto declared test blocks and
// run only those. Node >= 20, no dependencies.
//
// Policies (docs/testing.md is the user-facing contract):
// - Collection: committed (merge-base..HEAD, --no-renames so a rename counts
//   as delete+add on both ends) ∪ tracked worktree changes ∪ expanded
//   untracked files. Git failures are fatal.
// - Unknown paths are NOT silently "unaffected": only the no-test whitelist
//   below is exempt; anything unmapped selects the conservative full set.
// - Full blocks supersede their sub-blocks (covers), so an expensive test
//   never runs twice in one invocation.
// - The slow lane (install/upgrade) is selected honestly by default; --fast
//   skips it and reports it as NOT RUN.
// - Node blocks run one file at a time with stdin ignored: this removes
//   cross-file interference inside one selector run and inherited-stdin
//   hangs, and is not a claim that no test can ever hang.

import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';

const REPO_ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..', '..');

// ---------------------------------------------------------------------------
// Blocks

const SLOW_LANE = ['node:install', 'node:upgrade'];
const DAEMON_SUBBLOCKS = ['rust:daemon-scheduler', 'rust:daemon-rpc', 'rust:daemon-mcp', 'rust:daemon-adapters'];
const ALL_NODE_BLOCKS = ['node:cli', 'node:contract', 'node:acceptance', 'node:platform', 'node:release', 'node:provider', 'node:integration', 'node:install', 'node:upgrade'];
const ALL_RUST_BLOCKS = [
  'rust:store', 'rust:runtime', 'rust:core', 'rust:contract', 'rust:subagents', 'rust:mcp-crate', 'rust:fixture-zcode',
  ...DAEMON_SUBBLOCKS, 'rust:daemon-full', 'rust:workspace-full',
];

// cost = rough warm estimate in seconds on a dev laptop; ordering hint only.
// The two broad rust blocks pin --test-threads=4: this repo's process-reap
// tests are load-sensitive at default parallelism (observed 2026-09-30/10-01).
export const BLOCKS = {
  'rust:store': { kind: 'cargo', args: ['test', '-p', 'external-store'], cost: 20, desc: 'external-store crate' },
  'rust:runtime': { kind: 'cargo', args: ['test', '-p', 'external-runtime'], cost: 5, desc: 'external-runtime crate' },
  'rust:core': { kind: 'cargo', args: ['test', '-p', 'external-core'], cost: 5, desc: 'external-core crate' },
  'rust:contract': { kind: 'cargo', args: ['test', '-p', 'external-contract'], cost: 5, desc: 'external-contract crate' },
  'rust:subagents': { kind: 'cargo', args: ['test', '-p', 'external-agent-agy', '-p', 'external-agent-codex', '-p', 'external-agent-dsh'], cost: 20, desc: 'subagent crates own tests' },
  'rust:mcp-crate': { kind: 'cargo', args: ['test', '-p', 'external-mcp'], cost: 10, desc: 'external-mcp crate' },
  'rust:fixture-zcode': { kind: 'cargo', args: ['test', '-p', 'external-fixture-zcode'], cost: 30, desc: 'zcode fixture crate (drives real scheduler/store)' },
  'rust:daemon-scheduler': { kind: 'cargo', args: ['test', '-p', 'external-daemon', '--', 'scheduler'], cost: 90, desc: 'daemon lib tests filtered: scheduler' },
  'rust:daemon-rpc': { kind: 'cargo', args: ['test', '-p', 'external-daemon', '--', 'rpc'], cost: 120, desc: 'daemon lib tests filtered: rpc' },
  'rust:daemon-mcp': { kind: 'cargo', args: ['test', '-p', 'external-daemon', '--', 'mcp'], cost: 45, desc: 'daemon lib tests filtered: mcp' },
  'rust:daemon-adapters': { kind: 'cargo', args: ['test', '-p', 'external-daemon', '--', 'agy', 'codex', 'dsh', 'zcode', 'agent_status'], cost: 120, desc: 'daemon lib tests filtered: adapters + agent_status' },
  'rust:daemon-full': { kind: 'cargo', args: ['test', '-p', 'external-daemon', '--', '--test-threads=4'], cost: 180, covers: DAEMON_SUBBLOCKS, desc: 'external-daemon all tests' },
  'rust:workspace-full': { kind: 'cargo', args: ['test', '--workspace', '--', '--test-threads=4'], cost: 360, covers: ALL_RUST_BLOCKS.filter((id) => id !== 'rust:workspace-full'), desc: 'whole workspace' },
  'node:cli': { kind: 'node', dir: 'cli', cost: 8, prebuild: 'debug-daemon', desc: 'tests/cli (agents-config runs target/debug daemon)' },
  'node:contract': { kind: 'node', dir: 'contract', cost: 3, desc: 'tests/contract (mock-wire projections)' },
  'node:acceptance': { kind: 'node', dir: 'acceptance', cost: 1, desc: 'tests/acceptance (reads docs/acceptance + plugin manifest)' },
  'node:platform': { kind: 'node', dir: 'platform', cost: 2, desc: 'tests/platform' },
  'node:release': { kind: 'node', dir: 'release', cost: 5, desc: 'tests/release (release pack gate regression)' },
  'node:provider': { kind: 'node', dir: 'provider-conformance', cost: 10, desc: 'tests/provider-conformance' },
  'node:integration': { kind: 'node', dir: 'integration', cost: 40, prebuild: 'debug-daemon', desc: 'tests/integration' },
  'node:install': { kind: 'node', dir: 'install', cost: 90, slow: true, desc: 'tests/install (payload+pack+install; debug profile builds)' },
  'node:upgrade': { kind: 'node', dir: 'upgrade', cost: 600, slow: true, desc: 'tests/upgrade (in-tree cargo builds + drain scenarios)' },
};

// ---------------------------------------------------------------------------
// Impact map. Order matters: the first matching rule wins per path.
// slow = the slow lane; it is selected honestly and skippable via --fast.

const SLOW = ['node:install', 'node:upgrade'];
const ROOT_MD = /^(?:README|LICENSE|CHANGELOG|AGENTS)\.md$/u;

function under(...prefixes) {
  return (p) => prefixes.some((prefix) => p === prefix || p.startsWith(prefix + '/'));
}
function is(p) {
  return (x) => x === p;
}

export const IMPACT_RULES = [
  { name: 'no-test whitelist', match: (p) => ROOT_MD.test(p) || under('.agent-work', '.agents', '.github', 'docs', 'git-worktree')(p) && !under('docs/acceptance')(p) || p === '.gitignore', blocks: [] },
  { name: 'selector itself', match: is('scripts/test/affected.mjs'), blocks: ['node:cli'] },
  { name: 'workspace toolchain', match: (p) => ['Cargo.toml', 'Cargo.lock', 'rust-toolchain', 'rust-toolchain.toml', 'clippy.toml', 'deny.toml'].includes(p), blocks: ['rust:workspace-full', ...SLOW] },
  { name: 'store', match: under('crates/external-store'), blocks: ['rust:store', 'rust:daemon-full', 'rust:fixture-zcode', 'node:integration', ...SLOW] },
  { name: 'daemon scheduler', match: (p) => p === 'crates/external-daemon/src/scheduler.rs' || under('crates/external-daemon/src/scheduler')(p), blocks: ['rust:daemon-scheduler', 'rust:fixture-zcode', 'node:integration', ...SLOW] },
  { name: 'daemon rpc', match: (p) => p === 'crates/external-daemon/src/rpc.rs' || under('crates/external-daemon/src/rpc')(p), blocks: ['rust:daemon-rpc', 'rust:fixture-zcode', 'node:cli', 'node:contract', ...SLOW] },
  { name: 'daemon mcp', match: (p) => p === 'crates/external-daemon/src/mcp.rs' || under('crates/external-daemon/src/mcp')(p), blocks: ['rust:daemon-mcp', ...SLOW] },
  { name: 'daemon agent_status', match: (p) => p === 'crates/external-daemon/src/agent_status.rs' || under('crates/external-daemon/src/agent_status')(p), blocks: ['rust:daemon-adapters', ...SLOW] },
  { name: 'daemon projection', match: is('crates/external-daemon/src/projection.rs'), blocks: ['rust:daemon-full', ...SLOW] },
  { name: 'daemon adapters', match: (p) => ['agy', 'codex', 'dsh', 'zcode'].some((a) => p === `crates/external-daemon/src/${a}.rs` || p.startsWith(`crates/external-daemon/src/${a}/`)), blocks: ['rust:daemon-adapters', 'node:integration', ...SLOW] },
  { name: 'daemon rest (conservative full)', match: under('crates/external-daemon'), blocks: ['rust:daemon-full', 'node:integration', ...SLOW] },
  { name: 'runtime', match: under('crates/external-runtime'), blocks: ['rust:runtime', 'rust:subagents', 'rust:daemon-full', 'node:integration', ...SLOW] },
  { name: 'core', match: under('crates/external-core'), blocks: ['rust:core', 'rust:subagents', 'rust:daemon-full', 'rust:fixture-zcode', 'node:integration', ...SLOW] },
  { name: 'contract', match: under('crates/external-contract'), blocks: ['rust:contract', 'rust:subagents', 'rust:daemon-full', 'node:integration', ...SLOW] },
  { name: 'mcp crate', match: under('crates/external-mcp'), blocks: ['rust:mcp-crate', 'node:install'] },
  { name: 'subagent crates', match: under('crates/subagents'), blocks: ['rust:subagents', 'rust:daemon-full', 'node:integration', ...SLOW] },
  { name: 'zcode fixture crate', match: under('crates/fixtures/zcode'), blocks: ['rust:fixture-zcode', 'rust:daemon-adapters', ...SLOW] },
  { name: 'embedded dsh inputs', match: under('profiles', 'plugins/dsh-write-guard'), blocks: ['rust:subagents', 'rust:daemon-adapters', 'node:integration', ...SLOW] },
  { name: 'public api schema', match: is('schema/external-subagent-public-api.json'), blocks: ['node:cli', 'node:contract'] },
  { name: 'observation schema (compiled in)', match: is('schema/observation.schema.json'), blocks: ['node:cli', 'rust:daemon-mcp', ...SLOW] },
  { name: 'reasoning source schema', match: is('schema/public-reasoning-source.json'), blocks: ['node:cli'] },
  { name: 'cli', match: under('cli'), blocks: [...ALL_NODE_BLOCKS] },
  { name: 'bin entries', match: under('bin'), blocks: ['node:cli', 'node:contract', 'node:install', 'node:platform'] },
  { name: 'packaging/release scripts', match: under('scripts'), blocks: ['node:install', 'node:upgrade', 'node:platform', 'node:release'] },
  { name: 'package inputs', match: (p) => p === 'package.json' || p === 'LICENSE' || under('npm', 'launchd')(p), blocks: ['node:install', 'node:platform'] },
  { name: 'plugins (manifest is acceptance-read)', match: under('plugins'), blocks: ['node:install', 'node:acceptance'] },
  { name: 'acceptance evidence docs', match: under('docs/acceptance'), blocks: ['node:acceptance'] },
  { name: 'fixture: cli daemon harness', match: (p) => ['tests/fixtures/restart-daemon.mjs', 'tests/fixtures/zcode-general.mjs'].includes(p), blocks: ['node:cli'] },
  { name: 'fixture: dsh-hi-probe', match: is('tests/fixtures/dsh-hi-probe.mjs'), blocks: ['node:cli', 'rust:daemon-adapters'] },
  { name: 'fixture: dsh acp fake server', match: under('tests/fixtures/dsh-acp'), blocks: ['node:integration'] },
  { name: 'fixture: agent-status probe', match: under('tests/fixtures/agent-status'), blocks: ['node:contract'] },
  { name: 'fixture: versioned daemon', match: is('tests/fixtures/versioned-daemon.mjs'), blocks: ['node:upgrade'] },
  { name: 'fixture: config matrix (rust reads it)', match: is('tests/fixtures/subagent-config-matrix.json'), blocks: ['node:cli', 'rust:daemon-rpc'] },
  { name: 'live-agent fixtures (rust embeds/reads)', match: under('tests/live-agent'), blocks: ['rust:daemon-full'] },
  { name: 'test files', match: (p) => /^tests\/([a-z-]+)\/[^/]+\.test\.mjs$/u.exec(p), blocks: null, dynamic: (m) => (m[1] === 'provider-conformance' ? ['node:provider'] : [`node:${m[1]}`]) },
  { name: 'unmapped fixture (conservative)', match: under('tests/fixtures'), blocks: ['rust:workspace-full', ...ALL_NODE_BLOCKS] },
];

export function ruleForPath(p) {
  for (const rule of IMPACT_RULES) {
    const m = rule.match(p);
    if (m) return { rule, blocks: rule.dynamic ? rule.dynamic(m) : rule.blocks };
  }
  return { rule: { name: 'unmapped path (conservative full)' }, blocks: ['rust:workspace-full', ...ALL_NODE_BLOCKS] };
}

// selection: Map<blockId, {reasons: Set<string>}>
export function selectBlocks(paths) {
  const selection = new Map();
  const add = (id, reason) => {
    if (!BLOCKS[id]) throw new Error(`unknown block in rules: ${id}`);
    if (!selection.has(id)) selection.set(id, { reasons: new Set() });
    selection.get(id).reasons.add(reason);
  };
  for (const p of paths) {
    const { rule, blocks } = ruleForPath(p);
    if (blocks.length === 0) continue;
    for (const id of blocks) add(id, `${p} → ${rule.name}`);
  }
  return selection;
}

// Supersede sub-blocks covered by a selected full block.
export function normalizeSelection(selection) {
  const covered = [];
  for (const [id, meta] of [...selection.entries()]) {
    for (const sub of BLOCKS[id]?.covers ?? []) {
      if (selection.has(sub) && sub !== id) {
        const removed = selection.get(sub);
        for (const r of removed.reasons) meta.reasons.add(`[covered from ${sub}] ${r}`);
        selection.delete(sub);
        covered.push({ id: sub, by: id });
      }
    }
  }
  return { selection, covered };
}

export function applyFast(selection) {
  const skipped = [];
  for (const id of SLOW_LANE) {
    if (selection.has(id)) {
      skipped.push({ id, meta: selection.get(id) });
      selection.delete(id);
    }
  }
  return { selection, skipped };
}

// ---------------------------------------------------------------------------
// Change collection (git)

function runGit(args, opts = {}) {
  const res = spawnSync('git', args, { cwd: REPO_ROOT, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024, ...opts });
  if (res.error || res.status !== 0) {
    process.stderr.write(`git ${args.join(' ')} failed: ${res.stderr || res.error?.message || res.status}\n`);
    process.exit(2);
  }
  return res.stdout;
}

function splitNul(out) {
  return out.split('\0').filter(Boolean);
}

export function collectChangedFiles(base = 'main') {
  const mergeBase = runGit(['merge-base', base, 'HEAD']).trim();
  const committed = splitNul(runGit(['diff', '--name-only', '-z', '--no-renames', `${mergeBase}..HEAD`]));
  const worktree = splitNul(runGit(['diff', '--name-only', '-z', '--no-renames', 'HEAD']));
  const untracked = splitNul(runGit(['ls-files', '--others', '--exclude-standard', '-z']));
  const sources = new Map();
  const note = (file, src) => sources.set(file, [...(sources.get(file) ?? []), src]);
  committed.forEach((f) => note(f, 'committed'));
  worktree.forEach((f) => note(f, 'worktree'));
  untracked.forEach((f) => note(f, 'untracked'));
  return { files: [...sources.keys()].sort(), sources };
}

// ---------------------------------------------------------------------------
// Execution

function nodeTestFiles(dir) {
  const abs = path.join(REPO_ROOT, 'tests', dir);
  if (!fs.existsSync(abs)) return [];
  return fs.readdirSync(abs).filter((f) => f.endsWith('.test.mjs')).sort().map((f) => `tests/${dir}/${f}`);
}

const PREBUILDS = {
  'debug-daemon': { label: 'cargo build --bin external-subagentd (target/debug daemon for node tests)', cmd: ['build', '--bin', 'external-subagentd'] },
};

function runSelection(selected) {
  const order = [...selected.keys()].sort((a, b) => (BLOCKS[a].cost - BLOCKS[b].cost) || a.localeCompare(b));
  // 1) prebuilds (deduped)
  const needed = new Map();
  for (const id of order) {
    const pre = BLOCKS[id].prebuild;
    if (pre) needed.set(pre, needed.get(pre) ?? new Set());
    needed.get(pre)?.add(id);
  }
  const prebuildOk = new Map();
  for (const [pre, dependents] of needed) {
    const def = PREBUILDS[pre];
    process.stdout.write(`\n▶ prebuild ${pre}: ${def.label}\n`);
    const t0 = Date.now();
    const res = spawnSync('cargo', def.cmd, { cwd: REPO_ROOT, stdio: 'inherit' });
    const ok = res.status === 0;
    prebuildOk.set(pre, ok);
    if (!ok) process.stdout.write(`  ✗ prebuild failed (${((Date.now() - t0) / 1000).toFixed(0)}s) — dependents BLOCKED: ${[...dependents].join(', ')}\n`);
  }
  // 2) blocks, cheapest first, continue after failures
  const results = [];
  for (const id of order) {
    const def = BLOCKS[id];
    if (def.prebuild && prebuildOk.get(def.prebuild) === false) {
      results.push({ id, status: 'BLOCKED', detail: `prebuild ${def.prebuild} failed` });
      continue;
    }
    process.stdout.write(`\n▶ ${id} (${def.desc}, ~${def.cost}s)\n`);
    const t0 = Date.now();
    let status = 'PASS';
    let detail = '';
    if (def.kind === 'cargo') {
      const res = spawnSync('cargo', def.args, { cwd: REPO_ROOT, stdio: 'inherit' });
      if (res.status !== 0) { status = 'FAIL'; detail = `cargo exit ${res.status}`; }
    } else {
      const files = nodeTestFiles(def.dir);
      const failed = [];
      for (const f of files) {
        const res = spawnSync(process.execPath, ['--test', f], { cwd: REPO_ROOT, stdio: ['ignore', 'inherit', 'inherit'] });
        if (res.status !== 0) failed.push({ file: f, code: res.status });
      }
      if (failed.length > 0) { status = 'FAIL'; detail = failed.map((x) => `${x.file} (exit ${x.code})`).join('; '); }
    }
    const secs = ((Date.now() - t0) / 1000).toFixed(0);
    results.push({ id, status, detail, secs });
    process.stdout.write(`  ${status === 'PASS' ? '✔' : '✖'} ${id} ${secs}s${detail ? ` — ${detail}` : ''}\n`);
  }
  return { results, order };
}

// ---------------------------------------------------------------------------
// CLI

function usage(rc = 2) {
  process.stdout.write(`usage: node scripts/test/affected.mjs [options]
  --base <ref>     git ref for the committed diff (default: main; merge-base semantics)
  --files a b ...  extra changed paths appended to the git-collected set
  --fast           skip the slow lane (install/upgrade); reports them NOT RUN
  --all            select every block (mutually exclusive with --fast)
  --only id,id     run exactly these blocks (still prebuilds + supersede report)
  --list           list all blocks with costs and exit
  --dry-run        print the selection + reasons, do not execute
`);
  process.exit(rc);
}

function parseArgs(argv) {
  const opts = { base: 'main', files: [], fast: false, all: false, only: null, list: false, dryRun: false };
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === '--base') opts.base = argv[++i];
    else if (a === '--files') { for (i += 1; i < argv.length && !argv[i].startsWith('--'); i += 1) opts.files.push(argv[i]); i -= 1; }
    else if (a === '--fast') opts.fast = true;
    else if (a === '--all') opts.all = true;
    else if (a === '--only') opts.only = (argv[++i] ?? '').split(',').map((s) => s.trim()).filter(Boolean);
    else if (a === '--list') opts.list = true;
    else if (a === '--dry-run') opts.dryRun = true;
    else if (a === '--help' || a === '-h') usage(0);
    else usage();
  }
  if (opts.fast && opts.all) usage();
  return opts;
}

function report(selection, covered, skippedFast, notRun, sources, executed) {
  process.stdout.write('\n— selection ' + '—'.repeat(40) + '\n');
  for (const [id, meta] of selection) {
    process.stdout.write(`  ${id}  (~${BLOCKS[id].cost}s${BLOCKS[id].slow ? ', slow' : ''})\n`);
    for (const r of meta.reasons) process.stdout.write(`      · ${r}\n`);
  }
  process.stdout.write('\n— NOT RUN ' + '—'.repeat(39) + '\n');
  if (covered.length === 0 && skippedFast.length === 0 && notRun.length === 0) process.stdout.write('  (none — every block selected)\n');
  for (const c of covered) process.stdout.write(`  ${c.id}: covered by ${c.by}\n`);
  for (const s of skippedFast) process.stdout.write(`  ${s.id}: skipped by --fast (would run: ${[...s.meta.reasons].slice(0, 2).join(' | ')}…)\n`);
  for (const n of notRun) process.stdout.write(`  ${n}: not impacted by the changed surface\n`);
  if (sources) process.stdout.write(`\nchanged files: ${sources.size} (committed/worktree/untracked union)\n`);
  if (executed) {
    process.stdout.write('\n— results ' + '—'.repeat(41) + '\n');
    for (const r of executed.results) process.stdout.write(`  ${r.status.padEnd(7)} ${r.id}${r.secs ? ` ${r.secs}s` : ''}${r.detail ? ` — ${r.detail}` : ''}\n`);
    const bad = executed.results.filter((r) => r.status !== 'PASS');
    if (bad.length > 0) {
      process.stdout.write('\nfailed/blocked blocks (known baseline failures are NOT auto-exempted;\n' +
        'compare against the recorded pre-existing set before treating a red block as a regression):\n');
      for (const b of bad) process.stdout.write(`  ${b.id}: ${b.detail}\n`);
    }
  }
}

function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.list) {
    for (const [id, def] of Object.entries(BLOCKS)) {
      process.stdout.write(`  ${id.padEnd(22)} ~${String(def.cost).padEnd(4)}s ${def.slow ? '[slow] ' : ''}${def.desc}\n`);
    }
    return 0;
  }
  let selection;
  let sources = null;
  if (opts.only) {
    for (const id of opts.only) if (!BLOCKS[id]) { process.stderr.write(`unknown block: ${id}\n`); usage(); }
    selection = new Map(opts.only.map((id) => [id, { reasons: new Set(['--only']) }]));
  } else if (opts.all) {
    selection = new Map(Object.keys(BLOCKS).map((id) => [id, { reasons: new Set(['--all']) }]));
  } else {
    const { files, sources: src } = collectChangedFiles(opts.base);
    sources = new Map([...src.entries()].map(([f, s]) => [f, s]));
    const allFiles = [...new Set([...files, ...opts.files])].sort();
    if (allFiles.length === 0) {
      process.stdout.write('no changed files (empty diff vs base and clean worktree) — nothing to run.\n');
      return 0;
    }
    selection = selectBlocks(allFiles);
    if (selection.size === 0) {
      process.stdout.write(`changed surface is entirely on the no-test whitelist (${allFiles.length} files) — no test block required.\n`);
      return 0;
    }
  }
  const { selection: norm, covered } = normalizeSelection(selection);
  const { selection: afterFast, skipped: skippedFast } = opts.fast ? applyFast(norm) : { selection: norm, skipped: [] };
  const notRun = Object.keys(BLOCKS).filter((id) => !afterFast.has(id) && !covered.some((c) => c.id === id) && !skippedFast.some((s) => s.id === id));
  process.stdout.write(`test:affected — base=${opts.only ? '(--only)' : opts.all ? '(--all)' : opts.base}${opts.fast ? ' --fast' : ''}\n`);
  if (afterFast.size === 0) {
    report(afterFast, covered, skippedFast, notRun, null, null);
    process.stdout.write('\nnothing to run after --fast.\n');
    return 0;
  }
  if (opts.dryRun) {
    report(afterFast, covered, skippedFast, notRun, sources, null);
    return 0;
  }
  const executed = runSelection(afterFast);
  report(afterFast, covered, skippedFast, notRun, sources, executed);
  return executed.results.some((r) => r.status !== 'PASS') ? 1 : 0;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  process.exit(main());
}
