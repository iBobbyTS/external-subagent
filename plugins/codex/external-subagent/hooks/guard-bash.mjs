#!/usr/bin/env node
let raw = '';
process.stdin.setEncoding('utf8');
for await (const chunk of process.stdin) raw += chunk;
try {
  const input = JSON.parse(raw);
  const managed = process.env.ZCODE_AGENT_POLICY === '1';
  const mode = process.env.ZCODE_AGENT_PERMISSION_MODE || process.env.ZCODE_PERMISSION_MODE;
  if (!managed || mode !== 'plan' || (input?.tool_name ?? input?.toolName) !== 'Bash') process.exit(0);
  process.stdout.write(`${JSON.stringify({ hookSpecificOutput: { hookEventName: 'PreToolUse', permissionDecision: 'deny', permissionDecisionReason: 'external-subagent policy: Bash is disabled in plan probes' } })}\n`);
} catch {
  if (process.env.ZCODE_AGENT_POLICY === '1') {
    process.stdout.write(`${JSON.stringify({ hookSpecificOutput: { hookEventName: 'PreToolUse', permissionDecision: 'deny', permissionDecisionReason: 'external-subagent policy: malformed hook input' } })}\n`);
  }
}
