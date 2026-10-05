import fs from 'node:fs';
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

// Rust `str::trim` strips the Unicode White_Space property (which includes
// U+0085 NEL and U+00A0 NBSP) but NOT U+FEFF, whereas JavaScript `String.trim`
// strips U+FEFF and leaves U+0085. Name identity must match the daemon's
// `raw.name.trim()`, so trim against Rust's exact character set.
const RUST_WHITESPACE = /[\u0009-\u000d\u0020\u0085\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000]/u;

function trimRust(value) {
  let start = 0;
  let end = value.length;
  while (start < end && RUST_WHITESPACE.test(value[start])) start += 1;
  while (end > start && RUST_WHITESPACE.test(value[end - 1])) end -= 1;
  return value.slice(start, end);
}

// Bare (unquoted) TOML values are only legal when they are a boolean, an
// integer (64-bit signed), a float (including inf/nan), or a date/time. The
// scanner must reject boundary violations exactly like the daemon's `toml`
// crate: out-of-range integers, invalid calendar/clock fields, malformed
// date-times, and illegal control characters are whole-document syntax errors
// (they erase any partial name owner). Anything else (e.g. `unquoted`, `12abc`)
// makes the whole document invalid TOML, so a malformed primitive must not be
// silently consumed as an opaque token.
const TOML_DEC_INT = /^[+-]?(0|[1-9](_?[0-9])*)$/u;
const TOML_HEX_INT = /^0x[0-9A-Fa-f](_?[0-9A-Fa-f])*$/u;
const TOML_OCT_INT = /^0o[0-7](_?[0-7])*$/u;
const TOML_BIN_INT = /^0b[01](_?[01])*$/u;
const TOML_FLOAT = /^[+-]?(0|[1-9](_?[0-9])*)((\.([0-9](_?[0-9])*))([eE][+-]?[0-9](_?[0-9])*)?|([eE][+-]?[0-9](_?[0-9])*))$/u;
const TOML_SPECIAL_FLOAT = /^[+-]?(inf|nan)$/u;
const TOML_DATE = /^(\d{4})-(\d{2})-(\d{2})$/u;
const TOML_TIME = /^(\d{2}):(\d{2}):(\d{2})(?:\.\d+)?$/u;
// A full date-time may separate date and time with `T`, `t`, or a single space
// (TOML 1.0). Offsets are `Z`/`z` or `+HH:MM`/`-HH:MM`.
const TOML_DATETIME = /^(\d{4})-(\d{2})-(\d{2})[Tt ](\d{2}):(\d{2}):(\d{2})(?:\.\d+)?(?:[Zz]|[+-](\d{2}):(\d{2}))?$/u;

const I64_MAX = 9223372036854775807n;
const I64_MIN = -9223372036854775808n;
const MONTH_DAYS = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

function isLeapYear(year) {
  return (year % 4 === 0 && year % 100 !== 0) || year % 400 === 0;
}

function validDateParts(year, month, day) {
  if (month < 1 || month > 12) return false;
  const maxDay = month === 2 && isLeapYear(year) ? 29 : MONTH_DAYS[month - 1];
  return day >= 1 && day <= maxDay;
}

function validTimeParts(hour, minute, second) {
  return hour <= 23 && minute <= 59 && second <= 59;
}

function intInI64Range(token) {
  try {
    const value = BigInt(token.replace(/_/gu, ''));
    return value >= I64_MIN && value <= I64_MAX;
  } catch {
    return false;
  }
}

function isValidTomlDate(token) {
  const match = TOML_DATE.exec(token);
  return Boolean(match) && validDateParts(Number(match[1]), Number(match[2]), Number(match[3]));
}

function isValidTomlTime(token) {
  const match = TOML_TIME.exec(token);
  return Boolean(match) && validTimeParts(Number(match[1]), Number(match[2]), Number(match[3]));
}

function isValidTomlDatetime(token) {
  const match = TOML_DATETIME.exec(token);
  if (!match) return false;
  if (!validDateParts(Number(match[1]), Number(match[2]), Number(match[3]))) return false;
  if (!validTimeParts(Number(match[4]), Number(match[5]), Number(match[6]))) return false;
  if (match[7] !== undefined && !(Number(match[7]) <= 23 && Number(match[8]) <= 59)) return false;
  return true;
}

function isValidTomlPrimitive(token) {
  if (TOML_DEC_INT.test(token) || TOML_HEX_INT.test(token) || TOML_OCT_INT.test(token) || TOML_BIN_INT.test(token)) {
    return intInI64Range(token);
  }
  if (TOML_FLOAT.test(token) || TOML_SPECIAL_FLOAT.test(token)) return true;
  return isValidTomlDate(token) || isValidTomlTime(token) || isValidTomlDatetime(token);
}

function parseUnicodeEscape(hex, label) {
  const codePoint = parseInt(hex, 16);
  if (codePoint > 0x10ffff || (codePoint >= 0xd800 && codePoint <= 0xdfff)) {
    throw new Error(`Invalid Unicode code point in \\${label} escape: ${hex}`);
  }
  return String.fromCodePoint(codePoint);
}

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
    if (this.peek() !== '#') return;
    this.advance(); // consume '#'
    while (this.pos < this.len) {
      const ch = this.peek();
      if (ch === '\n') return;
      if (ch === '\r') {
        if (this.peekAt(1) === '\n') return;
        throw new Error(`Disallowed control character 0x0d in comment at line ${this.line}`);
      }
      const code = ch.charCodeAt(0);
      if ((code < 0x20 && code !== 0x09) || code === 0x7f) {
        throw new Error(`Disallowed control character 0x${code.toString(16)} in comment at line ${this.line}`);
      }
      this.advance();
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
          result += parseUnicodeEscape(hex, 'u');
        } else if (esc === 'U') {
          let hex = '';
          for (let i = 0; i < 8; i++) {
            if (this.pos >= this.len) throw new Error('Unterminated \\U escape at EOF');
            hex += this.advance();
          }
          if (!/^[0-9a-fA-F]{8}$/.test(hex)) throw new Error(`Invalid \\U escape: \\U${hex}`);
          result += parseUnicodeEscape(hex, 'U');
        } else {
          throw new Error(`Invalid escape sequence: \\${esc}`);
        }
      } else {
        const code = ch.charCodeAt(0);
        if ((code < 0x20 && code !== 0x09) || code === 0x7f) {
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
          // At most two quotes may precede the closing delimiter; a run of six
          // or more cannot be split into `[1-2 content] + closing 3` and is
          // invalid TOML.
          if (quoteCount > 5) {
            throw new Error(`Too many consecutive quotes in multiline basic string at line ${this.line}`);
          }
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
        // TOML line-ending backslash: `\` + optional space/tab + required
        // newline, then all following whitespace/newlines are trimmed.
        let look = this.pos;
        while (look < this.len && (this.content[look] === ' ' || this.content[look] === '\t')) look += 1;
        if (look < this.len && (this.content[look] === '\n' || this.content[look] === '\r')) {
          while (this.pos < look) this.advance();
          this.consumeMultilineNewline('multiline basic string');
          while (this.pos < this.len) {
            const c = this.peek();
            if (c === ' ' || c === '\t' || c === '\n') {
              this.advance();
            } else if (c === '\r') {
              this.consumeMultilineNewline('multiline basic string');
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
          for (let i = 0; i < 4; i++) {
            if (this.pos >= this.len) throw new Error('Unterminated \\u escape at EOF');
            hex += this.advance();
          }
          if (!/^[0-9a-fA-F]{4}$/.test(hex)) throw new Error(`Invalid \\u escape: \\u${hex}`);
          result += parseUnicodeEscape(hex, 'u');
        } else if (esc === 'U') {
          let hex = '';
          for (let i = 0; i < 8; i++) {
            if (this.pos >= this.len) throw new Error('Unterminated \\U escape at EOF');
            hex += this.advance();
          }
          if (!/^[0-9a-fA-F]{8}$/.test(hex)) throw new Error(`Invalid \\U escape: \\U${hex}`);
          result += parseUnicodeEscape(hex, 'U');
        } else {
          throw new Error(`Invalid escape sequence: \\${esc}`);
        }
        continue;
      }
      if (this.peek() === '\r') {
        // TOML decodes CRLF to LF inside multi-line strings; a lone CR is illegal.
        this.consumeMultilineNewline('multiline basic string');
        result += '\n';
        continue;
      }
      const ch = this.advance();
      const code = ch.charCodeAt(0);
      if ((code < 0x20 && code !== 0x09 && code !== 0x0a) || code === 0x7f) {
        throw new Error(`Disallowed control character 0x${code.toString(16)} in multiline basic string`);
      }
      result += ch;
    }
    throw new Error('Unterminated multiline basic string');
  }

  // Consume a CRLF (decoded to one logical newline) or bare LF, rejecting a
  // lone CR the same way the TOML grammar does.
  consumeMultilineNewline(context) {
    if (this.peek() === '\r') {
      if (this.peekAt(1) !== '\n') throw new Error(`Disallowed control character 0x0d in ${context}`);
      this.advance();
      this.advance();
    } else {
      this.advance();
    }
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
      if ((code < 0x20 && code !== 0x09) || code === 0x7f) {
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
          if (quoteCount > 5) {
            throw new Error(`Too many consecutive quotes in multiline literal string at line ${this.line}`);
          }
          const extraQuotes = quoteCount - 3;
          result += '\''.repeat(extraQuotes);
          return { value: result, isString: true };
        } else {
          result += '\''.repeat(quoteCount);
          continue;
        }
      }
      if (this.peek() === '\r') {
        // TOML decodes CRLF to LF inside multi-line strings; a lone CR is illegal.
        this.consumeMultilineNewline('multiline literal string');
        result += '\n';
        continue;
      }
      const ch = this.advance();
      const code = ch.charCodeAt(0);
      if ((code < 0x20 && code !== 0x09 && code !== 0x0a) || code === 0x7f) {
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
      if (ch === ' ' || ch === '\t' || ch === '\n') {
        this.advance();
      } else if (ch === '\r') {
        // Only CRLF is a legal newline; a lone CR is an illegal control char.
        if (this.peekAt(1) !== '\n') {
          throw new Error(`Disallowed control character 0x0d at line ${this.line}`);
        }
        this.advance();
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
    const seenKeys = new Set();
    this.skipWhitespace();
    if (this.peek() === '}') {
      this.advance();
      return { value: table, isString: false, isInlineTable: true };
    }
    while (this.pos < this.len) {
      const keyParts = this.parseKey();
      const flatKey = JSON.stringify(keyParts);
      if (seenKeys.has(flatKey)) {
        throw new Error(`Duplicate key '${keyParts.join('.')}' in inline table at line ${this.line}`);
      }
      seenKeys.add(flatKey);
      this.skipWhitespace();
      if (this.peek() !== '=') throw new Error(`Expected '=' in inline table at line ${this.line}`);
      this.advance();
      const val = this.parseValue();
      table[keyParts.join('.')] = val;
      this.skipWhitespace();
      if (this.peek() === ',') {
        this.advance();
        this.skipWhitespace();
        // TOML forbids a trailing comma in an inline table.
        if (this.peek() === '}') {
          throw new Error(`Trailing comma in inline table at line ${this.line}`);
        }
        continue;
      }
      if (this.peek() === '}') {
        this.advance();
        return { value: table, isString: false, isInlineTable: true };
      }
      throw new Error(`Expected ',' or '}' in inline table at line ${this.line}`);
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
    if (token.length === 0) throw new Error(`Expected value at line ${this.line}`);
    if (token === 'true') return { value: true, isString: false, isBoolean: true };
    if (token === 'false') return { value: false, isString: false, isBoolean: true };
    // TOML permits a single space instead of `T` between a full date and time:
    // `1979-05-27 07:32:00`. The token scan stops at that space, so splice the
    // candidate time back on only when it forms a valid date-time.
    if (TOML_DATE.test(token) && this.peek() === ' ') {
      let look = this.pos + 1;
      let candidate = '';
      while (look < this.len && /[0-9:.+\-Zz]/u.test(this.content[look])) {
        candidate += this.content[look];
        look += 1;
      }
      const combined = `${token} ${candidate}`;
      if (candidate.length > 0 && isValidTomlDatetime(combined)) {
        this.advance(); // the separating space
        for (let i = 0; i < candidate.length; i++) this.advance();
        token = combined;
      }
    }
    if (isValidTomlPrimitive(token)) return { value: token, isString: false, isPrimitive: true };
    throw new Error(`Invalid TOML primitive '${token}' at line ${this.line}`);
  }

  scanDocument() {
    let nameValue = null;
    let nameIsString = false;
    let nameSeen = false;
    const syntaxErrors = [];
    const schemaErrors = [];
    const fieldTypeErrors = [];
    let syntaxValid = true;

    // Namespace model: every absolute TOML key path records how it was defined
    // so scalar/table redefinitions are rejected exactly like the daemon's
    // `toml` crate. A syntax-level conflict anywhere invalidates the whole
    // document, which in turn erases any partial name owner.
    //   value    -> `key = <string/scalar/array/inline table>`
    //   dotted   -> table implicitly created by a dotted-key prefix
    //   table    -> explicit `[header]`
    //   array    -> explicit `[[header]]`
    //   implicit -> table implicitly created by a table-header prefix
    const namespaces = [];
    const pathKey = (parts) => JSON.stringify(parts);
    const kindOf = (parts) => {
      const key = pathKey(parts);
      const entry = namespaces.find((candidate) => pathKey(candidate.path) === key);
      return entry ? entry.kind : null;
    };
    const setKind = (parts, kind) => {
      const key = pathKey(parts);
      const entry = namespaces.find((candidate) => pathKey(candidate.path) === key);
      if (entry) entry.kind = kind;
      else namespaces.push({ path: parts, kind });
    };
    const prefixIsValue = (parts) => {
      for (let i = 1; i < parts.length; i++) {
        if (kindOf(parts.slice(0, i)) === 'value') return true;
      }
      return false;
    };

    // `[]` means top level; a non-empty path means keys currently live inside
    // that table header, so they neither define top-level profile fields nor a
    // top-level name.
    let currentTable = [];

    while (this.pos < this.len) {
      this.skipWhitespace();
      if (this.pos >= this.len) break;
      const ch = this.peek();
      if (ch === '#') {
        try {
          this.skipComment();
        } catch (err) {
          syntaxErrors.push(err.message);
          syntaxValid = false;
          break;
        }
        continue;
      }
      if (ch === '\n') {
        this.advance();
        continue;
      }
      if (ch === '\r') {
        if (this.peekAt(1) !== '\n') {
          syntaxErrors.push(`Disallowed control character 0x0d at line ${this.line}`);
          syntaxValid = false;
          break;
        }
        this.advance();
        this.advance();
        continue;
      }
      if (ch === '[') {
        // Table header `[table]` or `[[array_table]]`. A well-formed header is a
        // schema error (profiles do not allow tables), not a syntax error: the
        // daemon's LooseName decode still exposes any name declared before the
        // header, and RawProfile then rejects the table. The remainder of the
        // document must still be fully validated, because a later syntax error
        // makes the whole document invalid and erases the owner.
        const headerLine = this.line;
        this.advance();
        const isArray = this.peek() === '[';
        if (isArray) this.advance();
        let headerParts;
        try {
          headerParts = this.parseKey();
        } catch (err) {
          syntaxErrors.push(`malformed table header: ${err.message}`);
          syntaxValid = false;
          break;
        }
        this.skipWhitespace();
        let closed = false;
        if (isArray) {
          if (this.peek() === ']' && this.peekAt(1) === ']') {
            this.advance(); this.advance(); closed = true;
          }
        } else if (this.peek() === ']') {
          this.advance(); closed = true;
        }
        if (!closed) {
          syntaxErrors.push(`malformed table header at line ${headerLine}: expected closing bracket`);
          syntaxValid = false;
          break;
        }
        try {
          this.skipWhitespace();
          this.skipComment();
        } catch (err) {
          syntaxErrors.push(err.message);
          syntaxValid = false;
          break;
        }
        if (this.pos < this.len && this.peek() !== '\r' && this.peek() !== '\n') {
          syntaxErrors.push(`malformed table header at line ${headerLine}: unexpected trailing characters`);
          syntaxValid = false;
          break;
        }

        if (prefixIsValue(headerParts)) {
          syntaxErrors.push(`namespace conflict: table header '${headerParts.join('.')}' extends a scalar value at line ${headerLine}`);
          syntaxValid = false;
          break;
        }
        const existing = kindOf(headerParts);
        const allowed = (isArray && existing === 'array') || (!isArray && existing === 'implicit');
        if (existing !== null && !allowed) {
          let verb = 'is declared more than once';
          if (existing === 'dotted') verb = 'redefines a dotted-key table';
          else if (existing === 'implicit') verb = 'redefines an implicit table';
          else if (existing === 'value') verb = 'redefines a value';
          else if (existing === 'array') verb = 'conflicts with an array-of-tables';
          syntaxErrors.push(`namespace conflict: table header '${headerParts.join('.')}' ${verb} at line ${headerLine}`);
          syntaxValid = false;
          break;
        }
        for (let i = 1; i < headerParts.length; i++) {
          if (kindOf(headerParts.slice(0, i)) === null) setKind(headerParts.slice(0, i), 'implicit');
        }
        setKind(headerParts, isArray ? 'array' : 'table');

        schemaErrors.push(`unexpected table header '${headerParts.join('.')}' (profiles do not allow tables)`);
        currentTable = headerParts;
        continue;
      }

      let keyParts;
      try {
        keyParts = this.parseKey();
      } catch (err) {
        syntaxErrors.push(`syntax error in key: ${err.message}`);
        syntaxValid = false;
        break;
      }
      this.skipWhitespace();
      if (this.peek() !== '=') {
        syntaxErrors.push(`expected '=' after key '${keyParts.join('.')}' at line ${this.line}`);
        syntaxValid = false;
        break;
      }
      this.advance();

      let valObj;
      try {
        valObj = this.parseValue();
      } catch (err) {
        syntaxErrors.push(`syntax error in value for key '${keyParts.join('.')}' at line ${this.line}: ${err.message}`);
        syntaxValid = false;
        break;
      }

      try {
        this.skipWhitespace();
        this.skipComment();
      } catch (err) {
        syntaxErrors.push(err.message);
        syntaxValid = false;
        break;
      }
      if (this.pos < this.len && this.peek() !== '\r' && this.peek() !== '\n') {
        syntaxErrors.push(`unexpected trailing characters after value for key '${keyParts.join('.')}' at line ${this.line}`);
        syntaxValid = false;
        break;
      }

      const absolute = [...currentTable, ...keyParts];
      if (prefixIsValue(absolute)) {
        syntaxErrors.push(`namespace conflict: key '${absolute.join('.')}' extends a scalar value at line ${this.line}`);
        syntaxValid = false;
        break;
      }
      if (kindOf(absolute) !== null) {
        // A redefinition (duplicate key or overwriting a table) makes the
        // document invalid TOML, not merely an invalid profile shape, so it must
        // not leave a partial name owner behind.
        syntaxErrors.push(`duplicate key '${absolute.join('.')}' at line ${this.line}`);
        syntaxValid = false;
        break;
      }
      for (let i = 1; i < absolute.length; i++) {
        if (kindOf(absolute.slice(0, i)) === null) setKind(absolute.slice(0, i), 'dotted');
      }
      setKind(absolute, 'value');

      if (currentTable.length !== 0) continue;
      if (keyParts.length > 1) {
        schemaErrors.push(`dotted key '${keyParts.join('.')}' is not allowed at top level of profile`);
        continue;
      }

      const key = keyParts[0];
      if (key === 'name') {
        nameSeen = true;
        nameIsString = valObj.isString;
        nameValue = valObj.value;
      } else if (ALLOWED_PROFILE_KEYS.has(key)) {
        // The daemon's RawProfile types every non-name field as Option<String>,
        // so a valid TOML value of any other type fails the whole RawProfile
        // decode. LooseName (which only reads `name`) still succeeds, so the
        // name remains an owner even though the file is not a usable profile.
        if (!valObj.isString) fieldTypeErrors.push(`field '${key}': must be a string`);
      } else {
        schemaErrors.push(`unknown top-level key '${key}'`);
      }
    }

    // Owner/identity name: only a fully TOML-syntax-valid document can provide a
    // top-level name, mirroring the daemon's `toml::from_str::<LooseName>`
    // (which fails wholesale on a syntax error, regardless of where the error
    // sits). Field-level, schema-level, and field-type problems do NOT erase the
    // identity; the daemon still registers the name as an owner in those cases.
    let name = null;
    if (syntaxValid && nameIsString && nameValue !== null) {
      const trimmed = trimRust(nameValue);
      if (trimmed.length > 0
        && Buffer.byteLength(nameValue, 'utf8') <= 128
        && !nameValue.includes('\0')) {
        name = trimmed;
      }
    }

    const errors = [...syntaxErrors, ...schemaErrors, ...fieldTypeErrors];
    if (syntaxValid && name === null) {
      if (!nameSeen) errors.push("missing required field 'name'");
      else if (!nameIsString) errors.push("field 'name': must be a string");
      else if (trimRust(nameValue).length === 0) errors.push("field 'name': cannot be empty");
      else if (Buffer.byteLength(nameValue, 'utf8') > 128) errors.push("field 'name': exceeds 128 bytes");
      else errors.push("field 'name': cannot contain NUL byte");
    }

    const valid = syntaxValid && schemaErrors.length === 0 && fieldTypeErrors.length === 0 && name !== null;
    return {
      name,
      rawName: name,
      valid,
      syntaxValid,
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
      if (!entry.name.endsWith('.toml')) continue;
      const full = path.join(dir, entry.name);
      // The daemon selects candidates with `Path::is_file()`, which follows
      // symbolic links; `Dirent.isFile()` does not. Use stat, and silently
      // skip entries that stat cannot resolve (e.g. dangling links), matching
      // `is_file() == false`.
      let stat;
      try {
        stat = fs.statSync(full);
      } catch {
        continue;
      }
      if (stat.isFile()) entries.push(full);
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
      // Read raw bytes and decode strictly: the daemon uses
      // `fs::read_to_string`, which fails on invalid UTF-8 and skips the file
      // without registering a name owner. `readFileSync(path, 'utf8')` would
      // instead substitute U+FFFD and wrongly accept the file.
      const bytes = fs.readFileSync(filePath);
      content = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
    } catch (err) {
      fileErrors.push({
        file: filePath,
        diagnostic: `profile file '${filePath}' is unreadable: ${err.message}`,
      });
      continue;
    }

    const res = scanProfileToml(content, filePath);
    if (res.name) {
      if (!nameOwners.has(res.name)) nameOwners.set(res.name, []);
      nameOwners.get(res.name).push(filePath);
    }

    if (res.valid) {
      candidates.push({ name: res.name, filePath, content });
    } else {
      fileErrors.push({
        file: filePath,
        profileName: res.name || null,
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
  // Resolution is a pure function of the injected paths/env; there is no
  // test-only production branch keyed on os.homedir().
  const dir = profilesDir(env, paths?.home);

  if (input.operation === 'list') {
    if (dir === null) {
      return {
        operation: 'list',
        directory: null,
        profiles: [],
        warnings: [],
        message: 'profiles are disabled: the configuration path environment variable is exported empty',
      };
    }
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
    if (dir === null) throw new CliError('INVALID_ARGUMENT', `profile '${name}' not found; available profiles: none`, 2);
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
