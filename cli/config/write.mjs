import { atomicWrite, jsonBytes } from '../fs-atomic.mjs';
import { validateConfig } from './schema.mjs';

export function writeConfig(file, value) {
  const config = validateConfig(value);
  config.revision += 1;
  atomicWrite(file, jsonBytes(config));
  return config;
}
