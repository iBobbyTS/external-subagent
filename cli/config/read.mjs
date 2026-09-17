import fs from 'node:fs';
import { defaultConfig, validateConfig } from './schema.mjs';
import { CliError } from '../errors.mjs';

function migrateLegacyConfig(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return value;
  if (value.schema_version !== undefined && value.schema_version !== 1) return value;
  const legacy = Object.hasOwn(value, 'agents') || Object.hasOwn(value, 'default_agent');
  if (Object.hasOwn(value, 'subagents') || Object.hasOwn(value, 'default_subagent')) {
    if (legacy || value.schema_version === 1) throw new CliError('CONFIG_INVALID', 'schema-1 config cannot contain canonical subagent fields', 2);
    return value;
  }
  const { agents, default_agent, ...rest } = value;
  return { ...rest, schema_version: 2, ...(agents === undefined ? {} : { subagents: agents }), ...(default_agent === undefined ? {} : { default_subagent: default_agent }) };
}

export function parseConfig(value) {
  return validateConfig(migrateLegacyConfig(value));
}

export function readConfig(file) {
  try { return parseConfig(JSON.parse(fs.readFileSync(file, 'utf8'))); }
  catch (error) { if (error?.code === 'ENOENT') return defaultConfig(); if (error instanceof SyntaxError) error.code = 'CONFIG_INVALID'; throw error; }
}
