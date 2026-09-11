import fs from 'node:fs';
import { defaultConfig, validateConfig } from './schema.mjs';

export function readConfig(file) {
  try { return validateConfig(JSON.parse(fs.readFileSync(file, 'utf8'))); }
  catch (error) { if (error?.code === 'ENOENT') return defaultConfig(); if (error instanceof SyntaxError) error.code = 'CONFIG_INVALID'; throw error; }
}
