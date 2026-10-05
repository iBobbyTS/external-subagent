import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { CliError } from '../errors.mjs';
import { profilesDir } from '../paths.mjs';

const SPAWN_FIELDS = new Set(['profile', 'subagent', 'repository', 'permission_mode', 'prompt', 'model', 'effort', 'write_manifest']);
const FLAG_FIELDS = new Map([
  ['--profile', 'profile'],
  ['--subagent', 'subagent'],
  ['--repository', 'repository'],
  ['--prompt', 'prompt'],
  ['--permission-mode', 'permission_mode'],
  ['--model', 'model'],
  ['--effort', 'effort'],
]);

const PROFILE_MUTEX_ERROR = 'profile cannot be combined with subagent, permission_mode, model, or effort; specify these in the profile TOML or omit profile';

function flagValue(args, option) {
  const value = args.shift();
  if (value === undefined || value === '' || value.startsWith('--')) {
    throw new CliError('INVALID_ARGUMENT', `${option} requires a non-null value`, 2);
  }
  return value;
}

export function parseSpawnArgs(args) {
  const input = {};
  const remaining = [...args];
  while (remaining.length > 0) {
    const option = remaining.shift();
    if (option === '--write-manifest') {
      (input.write_manifest ||= []).push(flagValue(remaining, option));
      continue;
    }
    const field = FLAG_FIELDS.get(option);
    if (!field) throw new CliError('INVALID_ARGUMENT', `unsupported spawn option: ${option}`, 2);
    if (Object.hasOwn(input, field)) throw new CliError('INVALID_ARGUMENT', `${option} may be provided only once`, 2);
    input[field] = flagValue(remaining, option);
  }
  if (Object.hasOwn(input, 'profile')) {
    if (Object.hasOwn(input, 'subagent') || Object.hasOwn(input, 'permission_mode') || Object.hasOwn(input, 'model') || Object.hasOwn(input, 'effort')) {
      throw new CliError('INVALID_ARGUMENT', PROFILE_MUTEX_ERROR, 2);
    }
  }
  if (!Object.hasOwn(input, 'repository')) throw new CliError('INVALID_ARGUMENT', '--repository is required', 2);
  if (!Object.hasOwn(input, 'prompt')) throw new CliError('INVALID_ARGUMENT', '--prompt is required', 2);
  return prepareSpawnInput(input);
}

export function prepareSpawnInput(input) {
  if (!input || typeof input !== 'object' || Array.isArray(input)) throw new CliError('INVALID_ARGUMENT', 'spawn input must be an object', 2);
  for (const key of Object.keys(input)) {
    if (!SPAWN_FIELDS.has(key)) throw new CliError('INVALID_ARGUMENT', `spawn contains unsupported field: ${key}`, 2);
  }
  if (Object.prototype.hasOwnProperty.call(input, 'profile') && input.profile === null) {
    throw new CliError('INVALID_ARGUMENT', 'profile must be omitted or a non-null profile name; null is invalid', 2);
  }
  if (input.profile !== undefined && (typeof input.profile !== 'string' || input.profile.length === 0)) {
    throw new CliError('INVALID_ARGUMENT', 'profile must be a non-empty string', 2);
  }
  if (input.profile !== undefined) {
    if (input.subagent !== undefined || input.permission_mode !== undefined || input.model !== undefined || input.effort !== undefined) {
      throw new CliError('INVALID_ARGUMENT', PROFILE_MUTEX_ERROR, 2);
    }
  }
  if (Object.prototype.hasOwnProperty.call(input, 'subagent') && input.subagent === null) {
    throw new CliError('INVALID_ARGUMENT', 'subagent must be omitted or a supported subagent id; null is invalid', 2);
  }
  if (input.subagent !== undefined && (typeof input.subagent !== 'string' || input.subagent.length === 0)) {
    throw new CliError('INVALID_ARGUMENT', 'subagent must be a non-empty string', 2);
  }
  if (Object.prototype.hasOwnProperty.call(input, 'model') && input.model === null) {
    throw new CliError('INVALID_ARGUMENT', 'model must be omitted or a non-null model token', 2);
  }
  if (input.model !== undefined && (typeof input.model !== 'string' || input.model.length === 0)) {
    throw new CliError('INVALID_ARGUMENT', 'model must be a non-empty string', 2);
  }
  if (Object.prototype.hasOwnProperty.call(input, 'effort') && input.effort === null) {
    throw new CliError('INVALID_ARGUMENT', 'effort must be omitted or a non-null effort token', 2);
  }
  if (input.effort !== undefined && (typeof input.effort !== 'string' || input.effort.length === 0)) {
    throw new CliError('INVALID_ARGUMENT', 'effort must be a non-empty string', 2);
  }
  return { ...input };
}

const ALLOWED_PROFILE_KEYS = new Set([
  'name',
  'subagent',
  'permission_mode',
  'model',
  'effort',
  'developer_instructions',
]);

export class TomlScanner {
  constructor(content, filePath = '<unknown>') {
    this.content = content;
    this.filePath = filePath;
    this.pos = 0;
    this.len = content.length;
    this.line = 1;
    this.col = 1;
  }

  peek() {
    return this.pos < this.len ? this.content[this.pos] : '';
  }

  peekAt(offset) {
    const idx = this.pos + offset;
    return idx < this.len ? this.content[idx] : '';
  }

  advance() {
    if (this.pos >= this.len) return '';
    const ch = this.content[this.pos++];
    if (ch === '\n') {
      this.line++;
      this.col = 1;
    } else {
      this.col++;
    }
    return ch;
  }

  skipWhitespace() {
    while (this.pos < this.len) {
      const ch = this.content[this.pos];
      if (ch === ' ' || ch === '\t') {
        this.advance();
      } else {
        break;
      }
    }
  }

  skipComment() {
    if (this.peek() === '#') {
      while (this.pos < this.len && this.peek() !== '\n') {
        this.advance();
      }
    }
  }

  parseBasicString() {
    this.advance(); // consume opening "
    let result = '';
    while (this.pos < this.len) {
      const ch = this.advance();
      if (ch === '"') {
        return { value: result, isString: true };
      }
      if (ch === '\n' || ch === '\r') {
        throw new Error(`Unescaped newline in basic string at line ${this.line}`);
      }
      if (ch === '\\') {
        if (this.pos >= this.len) throw new Error('Unterminated escape sequence at EOF');
        const esc = this.advance();
        if (esc === '"') result += '"';
        else if (esc === '\\') result += '\\';
        else if (esc === 'b') result += '\b';
        else if (esc === 'f') result += '\f';
        else if (esc === 'n') result += '\n';
        else if (esc === 'r') result += '\r';
        else if (esc === 't') result += '\t';
        else if (esc === 'u') {
          let hex = '';
          for (let i = 0; i < 4; i++) {
            if (this.pos >= this.len) throw new Error('Unterminated \\u escape at EOF');
            hex += this.advance();
          }
          if (!/^[0-9a-fA-F]{4}$/.test(hex)) throw new Error(`Invalid \\u escape: \\u${hex}`);
          result += String.fromCodePoint(parseInt(hex, 16));
        } else if (esc === 'U') {
          let hex = '';
          for (let i = 0; i < 8; i++) {
            if (this.pos >= this.len) throw new Error('Unterminated \\U escape at EOF');
            hex += this.advance();
          }
          if (!/^[0-9a-fA-F]{8}$/.test(hex)) throw new Error(`Invalid \\U escape: \\U${hex}`);
          const codePoint = parseInt(hex, 16);
          if (codePoint > 0x10ffff || (codePoint >= 0xd800 && codePoint <= 0xdfff)) {
            throw new Error(`Invalid Unicode code point in \\U escape: ${hex}`);
          }
          result += String.fromCodePoint(codePoint);
        } else {
          throw new Error(`Invalid escape sequence: \\${esc}`);
        }
      } else {
        const code = ch.charCodeAt(0);
        if (code < 0x20 && code !== 0x09) {
          throw new Error(`Disallowed control character 0x${code.toString(16)} in basic string`);
        }
        result += ch;
      }
    }
    throw new Error('Unterminated basic string');
  }

  parseMultilineBasicString() {
    this.advance(); this.advance(); this.advance(); // consume """
    if (this.peek() === '\r' && this.peekAt(1) === '\n') {
      this.advance(); this.advance();
    } else if (this.peek() === '\n') {
      this.advance();
    }
    let result = '';
    while (this.pos < this.len) {
      if (this.peek() === '"') {
        let quoteCount = 0;
        while (this.peek() === '"') {
          quoteCount++;
          this.advance();
        }
        if (quoteCount >= 3) {
          const extraQuotes = quoteCount - 3;
          result += '"'.repeat(extraQuotes);
          return { value: result, isString: true };
        } else {
          result += '"'.repeat(quoteCount);
          continue;
        }
      }
      if (this.peek() === '\\') {
        this.advance();
        if (this.pos >= this.len) throw new Error('Unterminated escape in multiline string');
        const next = this.peek();
        if (next === '\r' || next === '\n') {
          if (next === '\r' && this.peekAt(1) === '\n') {
            this.advance(); this.advance();
          } else {
            this.advance();
          }
          while (this.pos < this.len) {
            const c = this.peek();
            if (c === ' ' || c === '\t' || c === '\r' || c === '\n') {
              this.advance();
            } else {
              break;
            }
          }
          continue;
        }
        const esc = this.advance();
        if (esc === '"') result += '"';
        else if (esc === '\\') result += '\\';
        else if (esc === 'b') result += '\b';
        else if (esc === 'f') result += '\f';
        else if (esc === 'n') result += '\n';
        else if (esc === 'r') result += '\r';
        else if (esc === 't') result += '\t';
        else if (esc === 'u') {
          let hex = '';
          for (let i = 0; i < 4; i++) hex += this.advance();
          if (!/^[0-9a-fA-F]{4}$/.test(hex)) throw new Error(`Invalid \\u escape: \\u${hex}`);
          result += String.fromCodePoint(parseInt(hex, 16));
        } else if (esc === 'U') {
          let hex = '';
          for (let i = 0; i < 8; i++) hex += this.advance();
          if (!/^[0-9a-fA-F]{8}$/.test(hex)) throw new Error(`Invalid \\U escape: \\U${hex}`);
          const codePoint = parseInt(hex, 16);
          if (codePoint > 0x10ffff || (codePoint >= 0xd800 && codePoint <= 0xdfff)) {
            throw new Error(`Invalid Unicode code point in \\U escape: ${hex}`);
          }
          result += String.fromCodePoint(codePoint);
        } else {
          throw new Error(`Invalid escape sequence: \\${esc}`);
        }
        continue;
      }
      const ch = this.advance();
      const code = ch.charCodeAt(0);
      if (code < 0x20 && code !== 0x09 && code !== 0x0a && code !== 0x0d) {
        throw new Error(`Disallowed control character 0x${code.toString(16)} in multiline basic string`);
      }
      result += ch;
    }
    throw new Error('Unterminated multiline basic string');
  }

  parseLiteralString() {
    this.advance(); // consume opening '
    let result = '';
    while (this.pos < this.len) {
      const ch = this.advance();
      if (ch === '\'') {
        return { value: result, isString: true };
      }
      if (ch === '\n' || ch === '\r') {
        throw new Error('Unescaped newline in literal string');
      }
      const code = ch.charCodeAt(0);
      if (code < 0x20 && code !== 0x09) {
        throw new Error(`Disallowed control character 0x${code.toString(16)} in literal string`);
      }
      result += ch;
    }
    throw new Error('Unterminated literal string');
  }

  parseMultilineLiteralString() {
    this.advance(); this.advance(); this.advance(); // consume '''
    if (this.peek() === '\r' && this.peekAt(1) === '\n') {
      this.advance(); this.advance();
    } else if (this.peek() === '\n') {
      this.advance();
    }
    let result = '';
    while (this.pos < this.len) {
      if (this.peek() === '\'') {
        let quoteCount = 0;
        while (this.peek() === '\'') {
          quoteCount++;
          this.advance();
        }
        if (quoteCount >= 3) {
          const extraQuotes = quoteCount - 3;
          result += '\''.repeat(extraQuotes);
          return { value: result, isString: true };
        } else {
          result += '\''.repeat(quoteCount);
          continue;
        }
      }
      const ch = this.advance();
      const code = ch.charCodeAt(0);
      if (code < 0x20 && code !== 0x09 && code !== 0x0a && code !== 0x0d) {
        throw new Error(`Disallowed control character 0x${code.toString(16)} in multiline literal string`);
      }
      result += ch;
    }
    throw new Error('Unterminated multiline literal string');
  }

  parseKeyComponent() {
    this.skipWhitespace();
    if (this.peek() === '"') {
      return this.parseBasicString().value;
    }
    if (this.peek() === '\'') {
      return this.parseLiteralString().value;
    }
    let bare = '';
    while (this.pos < this.len) {
      const ch = this.peek();
      if (/^[A-Za-z0-9_-]$/.test(ch)) {
        bare += this.advance();
      } else {
        break;
      }
    }
    if (bare.length === 0) {
      throw new Error(`Expected key at line ${this.line}, col ${this.col}`);
    }
    return bare;
  }

  parseKey() {
    const parts = [this.parseKeyComponent()];
    while (true) {
      this.skipWhitespace();
      if (this.peek() === '.') {
        this.advance();
        this.skipWhitespace();
        parts.push(this.parseKeyComponent());
      } else {
        break;
      }
    }
    return parts;
  }

  parseValue() {
    this.skipWhitespace();
    if (this.pos >= this.len) throw new Error('Expected value but reached EOF');
    if (this.content.startsWith('"""', this.pos)) {
      return this.parseMultilineBasicString();
    }
    if (this.content.startsWith("'''", this.pos)) {
      return this.parseMultilineLiteralString();
    }
    if (this.peek() === '"') {
      return this.parseBasicString();
    }
    if (this.peek() === '\'') {
      return this.parseLiteralString();
    }
    if (this.peek() === '[') {
      return this.parseArray();
    }
    if (this.peek() === '{') {
      return this.parseInlineTable();
    }
    return this.parsePrimitive();
  }

  skipWhitespaceAndNewlinesAndComments() {
    while (this.pos < this.len) {
      const ch = this.peek();
      if (ch === ' ' || ch === '\t' || ch === '\r' || ch === '\n') {
        this.advance();
      } else if (ch === '#') {
        this.skipComment();
      } else {
        break;
      }
    }
  }

  parseArray() {
    this.advance(); // consume [
    const elements = [];
    while (this.pos < this.len) {
      this.skipWhitespaceAndNewlinesAndComments();
      if (this.peek() === ']') {
        this.advance();
        return { value: elements, isString: false, isArray: true };
      }
      const val = this.parseValue();
      elements.push(val);
      this.skipWhitespaceAndNewlinesAndComments();
      if (this.peek() === ',') {
        this.advance();
        this.skipWhitespaceAndNewlinesAndComments();
        if (this.peek() === ']') {
          this.advance();
          return { value: elements, isString: false, isArray: true };
        }
      } else if (this.peek() === ']') {
        this.advance();
        return { value: elements, isString: false, isArray: true };
      } else {
        throw new Error(`Expected ',' or ']' in array at line ${this.line}`);
      }
    }
    throw new Error('Unterminated array');
  }

  parseInlineTable() {
    this.advance(); // consume {
    const table = {};
    while (this.pos < this.len) {
      this.skipWhitespace();
      if (this.peek() === '}') {
        this.advance();
        return { value: table, isString: false, isInlineTable: true };
      }
      const keyParts = this.parseKey();
      this.skipWhitespace();
      if (this.peek() !== '=') throw new Error(`Expected '=' in inline table at line ${this.line}`);
      this.advance();
      const val = this.parseValue();
      table[keyParts.join('.')] = val;
      this.skipWhitespace();
      if (this.peek() === ',') {
        this.advance();
        this.skipWhitespace();
        if (this.peek() === '}') {
          this.advance();
          return { value: table, isString: false, isInlineTable: true };
        }
      } else if (this.peek() === '}') {
        this.advance();
        return { value: table, isString: false, isInlineTable: true };
      } else {
        throw new Error(`Expected ',' or '}' in inline table at line ${this.line}`);
      }
    }
    throw new Error('Unterminated inline table');
  }

  parsePrimitive() {
    let token = '';
    while (this.pos < this.len) {
      const ch = this.peek();
      if (ch === ' ' || ch === '\t' || ch === '\r' || ch === '\n' || ch === '#' || ch === ',' || ch === ']' || ch === '}') {
        break;
      }
      token += this.advance();
    }
    if (token === 'true') return { value: true, isString: false, isBoolean: true };
    if (token === 'false') return { value: false, isString: false, isBoolean: true };
    if (token.length === 0) throw new Error(`Expected value at line ${this.line}`);
    return { value: token, isString: false, isPrimitive: true };
  }

  scanDocument() {
    let topLevelName = null;
    let nameRawValue = null;
    let nameIsString = false;
    const topLevelKeys = new Set();
    const errors = [];

    while (this.pos < this.len) {
      this.skipWhitespace();
      if (this.pos >= this.len) break;
      const ch = this.peek();
      if (ch === '#') {
        this.skipComment();
        continue;
      }
      if (ch === '\r' || ch === '\n') {
        this.advance();
        continue;
      }
      if (ch === '[') {
        // Table header [table] or [[array_table]]
        this.advance();
        const isArray = this.peek() === '[';
        if (isArray) this.advance();
        let headerKey = [];
        try {
          headerKey = this.parseKey();
        } catch {}
        this.skipWhitespace();
        if (isArray) {
          if (this.peek() === ']' && this.peekAt(1) === ']') {
            this.advance(); this.advance();
          }
        } else if (this.peek() === ']') {
          this.advance();
        }
        errors.push(`unexpected table header '[${headerKey.join('.')}]' (profiles do not allow tables)`);
        break;
      }

      let keyParts;
      try {
        keyParts = this.parseKey();
      } catch (err) {
        errors.push(`syntax error in key: ${err.message}`);
        break;
      }
      this.skipWhitespace();
      if (this.peek() !== '=') {
        errors.push(`expected '=' after key '${keyParts.join('.')}' at line ${this.line}`);
        break;
      }
      this.advance();

      let valObj;
      try {
        valObj = this.parseValue();
      } catch (err) {
        errors.push(`syntax error in value for key '${keyParts.join('.')}' at line ${this.line}: ${err.message}`);
        break;
      }

      this.skipWhitespace();
      this.skipComment();
      if (this.pos < this.len && this.peek() !== '\r' && this.peek() !== '\n') {
        errors.push(`unexpected trailing characters after value for key '${keyParts.join('.')}' at line ${this.line}`);
        break;
      }

      if (keyParts.length > 1) {
        errors.push(`dotted key '${keyParts.join('.')}' is not allowed at top level of profile`);
        continue;
      }

      const key = keyParts[0];
      if (topLevelKeys.has(key)) {
        errors.push(`duplicate key '${key}' at line ${this.line}`);
        continue;
      }
      topLevelKeys.add(key);

      if (!ALLOWED_PROFILE_KEYS.has(key)) {
        errors.push(`unknown top-level key '${key}'`);
        continue;
      }

      if (key === 'name') {
        nameIsString = valObj.isString;
        nameRawValue = valObj.value;
      }
    }

    if (!nameIsString && nameRawValue !== null) {
      errors.push(`field 'name': must be a string`);
    } else if (nameRawValue === null && !errors.some((e) => e.includes('name'))) {
      errors.push(`missing required field 'name'`);
    } else if (nameIsString) {
      const trimmed = nameRawValue.trim();
      if (trimmed.length === 0) {
        errors.push(`field 'name': cannot be empty`);
      } else if (Buffer.byteLength(nameRawValue, 'utf8') > 128) {
        errors.push(`field 'name': exceeds 128 bytes`);
      } else if (nameRawValue.includes('\0')) {
        errors.push(`field 'name': cannot contain NUL byte`);
      } else {
        topLevelName = trimmed;
      }
    }

    return {
      name: errors.length === 0 ? topLevelName : null,
      rawName: topLevelName,
      valid: errors.length === 0 && topLevelName !== null,
      errors,
    };
  }
}

export function scanProfileToml(content, filePath = '<unknown>') {
  return new TomlScanner(content, filePath).scanDocument();
}

export function scanProfilesDir(dir) {
  const profiles = new Map();
  const fileErrors = [];
  const nameOwners = new Map();

  if (!fs.existsSync(dir)) {
    return {
      exists: false,
      directory: dir,
      profiles,
      availableNames: [],
      warnings: fileErrors,
      duplicates: new Map(),
    };
  }

  let entries = [];
  try {
    const dirEntries = fs.readdirSync(dir, { withFileTypes: true });
    for (const entry of dirEntries) {
      if (entry.isFile() && entry.name.endsWith('.toml')) {
        entries.push(path.join(dir, entry.name));
      }
    }
  } catch (err) {
    fileErrors.push({
      file: dir,
      diagnostic: `profile directory '${dir}' is unreadable: ${err.message}`,
    });
    return {
      exists: true,
      directory: dir,
      profiles,
      availableNames: [],
      warnings: fileErrors,
      duplicates: new Map(),
    };
  }

  entries.sort();

  const candidates = [];

  for (const filePath of entries) {
    let content;
    try {
      content = fs.readFileSync(filePath, 'utf8');
    } catch (err) {
      fileErrors.push({
        file: filePath,
        diagnostic: `profile file '${filePath}' is unreadable: ${err.message}`,
      });
      continue;
    }

    const res = scanProfileToml(content, filePath);
    if (res.rawName) {
      if (!nameOwners.has(res.rawName)) nameOwners.set(res.rawName, []);
      nameOwners.get(res.rawName).push(filePath);
    }

    if (res.valid) {
      candidates.push({ name: res.name, filePath, content });
    } else {
      fileErrors.push({
        file: filePath,
        profileName: res.rawName || null,
        diagnostic: `profile file '${filePath}' is invalid: ${res.errors.join('; ')}`,
      });
    }
  }

  for (const candidate of candidates) {
    const owners = nameOwners.get(candidate.name) || [];
    if (owners.length > 1) {
      const other = owners.find((p) => p !== candidate.filePath);
      fileErrors.push({
        file: candidate.filePath,
        profileName: candidate.name,
        diagnostic: `profile file '${candidate.filePath}' is invalid: duplicate profile name '${candidate.name}' already defined in '${other}'`,
      });
    } else {
      profiles.set(candidate.name, candidate);
    }
  }

  for (const [name, owners] of nameOwners.entries()) {
    if (owners.length > 1) {
      for (const owner of owners) {
        const hasDupDiag = fileErrors.some((w) => w.file === owner && w.diagnostic.includes('duplicate profile name'));
        if (!hasDupDiag) {
          const other = owners.find((p) => p !== owner);
          fileErrors.push({
            file: owner,
            profileName: name,
            diagnostic: `profile file '${owner}' is invalid: duplicate profile name '${name}' already defined in '${other}'`,
          });
        }
      }
    }
  }

  const availableNames = Array.from(profiles.keys()).sort();
  const duplicates = new Map([...nameOwners.entries()].filter(([, v]) => v.length > 1));

  return {
    exists: true,
    directory: dir,
    profiles,
    availableNames,
    warnings: fileErrors,
    duplicates,
  };
}

export function parseProfileArgs(args) {
  if (!args || args.length === 0) throw new CliError('INVALID_ARGUMENT', 'usage: profile list | profile show <name>', 2);
  const [operation, ...rest] = args;
  if (operation === 'list') {
    if (rest.length !== 0) throw new CliError('INVALID_ARGUMENT', 'usage: profile list', 2);
    return { operation: 'list' };
  }
  if (operation === 'show') {
    if (rest.length !== 1 || !rest[0] || rest[0].startsWith('--')) {
      throw new CliError('INVALID_ARGUMENT', 'usage: profile show <name>', 2);
    }
    return { operation: 'show', name: rest[0] };
  }
  throw new CliError('INVALID_ARGUMENT', `unsupported profile operation: ${operation}`, 2);
}

export function profileCommand(paths, inputOrArgs, env = process.env) {
  const input = Array.isArray(inputOrArgs) ? parseProfileArgs(inputOrArgs) : inputOrArgs;
  if (!input || typeof input !== 'object' || Array.isArray(input)) {
    throw new CliError('INVALID_ARGUMENT', 'profile input must be an object', 2);
  }
  const dir = (paths?.home && paths.home !== os.homedir() && env === process.env)
    ? path.join(paths.data, 'profiles')
    : profilesDir(env, paths?.home);

  if (input.operation === 'list') {
    const scanned = scanProfilesDir(dir);
    if (!scanned.exists) {
      return {
        operation: 'list',
        directory: dir,
        profiles: [],
        warnings: [],
        message: `profiles directory does not exist: ${dir}`,
      };
    }
    return {
      operation: 'list',
      directory: dir,
      profiles: scanned.availableNames,
      warnings: scanned.warnings.map((w) => ({ file: w.file, diagnostic: w.diagnostic })),
    };
  }

  if (input.operation === 'show') {
    const name = input.name;
    if (!name || typeof name !== 'string') throw new CliError('INVALID_ARGUMENT', 'usage: profile show <name>', 2);
    const scanned = scanProfilesDir(dir);
    if (!scanned.exists) {
      throw new CliError('INVALID_ARGUMENT', `profile '${name}' not found; available profiles: none`, 2);
    }
    if (scanned.duplicates.has(name)) {
      const files = scanned.duplicates.get(name);
      throw new CliError('INVALID_ARGUMENT', `profile '${name}' is ambiguous; defined in multiple files: ${files.join(', ')}`, 2);
    }
    if (scanned.profiles.has(name)) {
      const profile = scanned.profiles.get(name);
      return {
        operation: 'show',
        name,
        file: profile.filePath,
        content: profile.content,
        warnings: scanned.warnings.map((w) => ({ file: w.file, diagnostic: w.diagnostic })),
      };
    }
    const availableList = scanned.availableNames.length > 0 ? `[${scanned.availableNames.join(', ')}]` : 'none';
    const matchedInvalid = scanned.warnings.find((w) => w.profileName === name);
    if (matchedInvalid) {
      throw new CliError('INVALID_ARGUMENT', `${matchedInvalid.diagnostic}; available profiles: ${availableList}`, 2);
    }
    if (scanned.warnings.length > 0) {
      const details = scanned.warnings.map((w) => w.diagnostic).join('; ');
      throw new CliError('INVALID_ARGUMENT', `profile '${name}' not found (directory contains invalid files: ${details}); available profiles: ${availableList}`, 2);
    }
    throw new CliError('INVALID_ARGUMENT', `profile '${name}' not found; available profiles: ${availableList}`, 2);
  }

  throw new CliError('INVALID_ARGUMENT', `unsupported profile operation: ${input.operation}`, 2);
}
