import fs from 'node:fs';
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

const PROFILE_MUTEX_ERROR = 'profile cannot be combined with subagent, permission_mode, model, or effort; specify these in the profile JSON or omit profile';

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

// A profile file is strict JSON, and the scanner must reproduce the daemon's
// `serde_json` decisions. `JSON.parse` differs from serde_json on three points
// that matter here, so the raw document text is validated *before* parsing.
// Validating the text (rather than the collapsed object) makes duplicate-key
// shadowing structurally irrelevant: a shadowed bad value is still visible.
//   1. lone surrogate escapes (`"\uD800"`) are accepted by `JSON.parse` but
//      rejected by serde_json, so every `\u` escape in a string is paired here
//      (high D800-DBFF immediately followed by low DC00-DFFF) and an unpaired
//      escape rejects the whole document;
//   2. `JSON.parse` accepts lossless integer literals of any magnitude, while
//      serde_json parses an integer that does not fit `u64`/`i64` as an `f64`
//      and rejects the document only when that conversion is infinite (de.rs
//      `parse_exponent_overflow` / `f64_long_from_parts`); every number literal
//      outside a string is therefore rejected iff `Number(token)` is not finite.
//      This uses correctly-rounded IEEE rounding; serde_json's default
//      accumulator can round to infinity within one ulp of `f64::MAX` (a
//      recorded residual, see S02-HANDOFF).
//   3. `JSON.parse` has no container-depth limit (it is iterative), while
//      serde_json starts with `remaining_depth = 128` (de.rs:63) and
//      `check_recursion` decrements before descending and errors when it reaches
//      zero (de.rs:1372-1387); the 128th nested `{`/`[` is rejected, so at most
//      127 containers may nest.
// A leading UTF-8 BOM is also stripped by `TextDecoder` unless `ignoreBOM` is
// set, and serde_json rejects a BOM; `scanProfilesDir` therefore preserves the
// BOM so that `JSON.parse` fails exactly like serde_json.
const PROFILE_FIELDS = ['name', 'subagent', 'permission_mode', 'model', 'effort', 'developer_instructions'];

// Match a complete JSON number literal (RFC 8259) at the sticky lastIndex.
const JSON_NUMBER = /-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?/y;

function decodeHex4(text, index) {
  const hex = text.slice(index, index + 4);
  if (!/^[0-9a-fA-F]{4}$/u.test(hex)) return null;
  return parseInt(hex, 16);
}

// Validate the raw JSON text against serde_json's lexical rules and recursion
// limit that `JSON.parse` does not enforce. Returns a diagnostic fragment, or
// null when the text is acceptable. Structural syntax errors are left to
// `JSON.parse`; this walk only needs to track string/escape state so that
// braces, digits and `\u` escapes are interpreted in the right context.
function rawJsonDiagnostic(text) {
  let inString = false;
  let depth = 0;
  let i = 0;
  while (i < text.length) {
    const ch = text[i];
    if (inString) {
      if (ch === '"') {
        inString = false;
        i += 1;
        continue;
      }
      if (ch === '\\') {
        const esc = text[i + 1];
        if (esc === 'u') {
          const code = decodeHex4(text, i + 2);
          if (code === null) {
            // Malformed escape: let JSON.parse report the syntax error.
            i += 2;
            continue;
          }
          if (code >= 0xd800 && code <= 0xdbff) {
            let paired = false;
            if (text[i + 6] === '\\' && text[i + 7] === 'u') {
              const low = decodeHex4(text, i + 8);
              paired = low !== null && low >= 0xdc00 && low <= 0xdfff;
            }
            if (!paired) return 'lone surrogate code point';
            i += 12; // consume both escapes of a valid surrogate pair
            continue;
          }
          if (code >= 0xdc00 && code <= 0xdfff) return 'lone surrogate code point';
          i += 6;
          continue;
        }
        // \" \\ \/ \b \f \n \r \t: an escaped backslash never starts a `\u`.
        i += 2;
        continue;
      }
      i += 1;
      continue;
    }
    if (ch === '"') {
      inString = true;
      i += 1;
      continue;
    }
    if (ch === '{' || ch === '[') {
      depth += 1;
      if (depth >= 128) return 'recursion limit exceeded';
      i += 1;
      continue;
    }
    if (ch === '}' || ch === ']') {
      if (depth > 0) depth -= 1;
      i += 1;
      continue;
    }
    if (ch === '-' || (ch >= '0' && ch <= '9')) {
      JSON_NUMBER.lastIndex = i;
      const match = JSON_NUMBER.exec(text);
      if (match !== null) {
        if (!Number.isFinite(Number(match[0]))) return `number out of range: ${match[0]}`;
        i += match[0].length;
        continue;
      }
    }
    i += 1;
  }
  return null;
}

// A decoded astral character is a legitimate UTF-16 surrogate pair in a JS
// string; an unpaired code unit is a lone surrogate.
function containsLoneSurrogate(text) {
  for (let i = 0; i < text.length; i += 1) {
    const code = text.charCodeAt(i);
    if (code >= 0xd800 && code <= 0xdbff) {
      const next = text.charCodeAt(i + 1);
      if (!(next >= 0xdc00 && next <= 0xdfff)) return true;
      i += 1; // consume the matching low surrogate of a valid pair
    } else if (code >= 0xdc00 && code <= 0xdfff) {
      return true;
    }
  }
  return false;
}

// Iterative traversal (never recursive) so an arbitrarily deep decoded value
// can never raise a `RangeError`; the raw depth gate already rejects documents
// deeper than serde_json's limit before `JSON.parse` runs.
function hasLoneSurrogate(value) {
  const stack = [value];
  while (stack.length > 0) {
    const current = stack.pop();
    if (typeof current === 'string') {
      if (containsLoneSurrogate(current)) return true;
      continue;
    }
    if (Array.isArray(current)) {
      for (const entry of current) stack.push(entry);
      continue;
    }
    if (current !== null && typeof current === 'object') {
      for (const [key, entry] of Object.entries(current)) {
        if (containsLoneSurrogate(key)) return true;
        stack.push(entry);
      }
    }
  }
  return false;
}

function invalidJson(diagnostic) {
  return { name: null, rawName: null, valid: false, syntaxValid: false, errors: [`invalid JSON: ${diagnostic}`] };
}

// Scan one profile document. `name` is the authoritative owner identity used
// for duplicate detection and mirrors the daemon's `loose_top_level_name`: a
// document that parses as JSON, has a plain-object top level, and a string
// `name` owns that name (trimmed, non-empty, at most 128 bytes, no NUL) even
// when the profile shape is otherwise invalid. `valid` additionally requires the
// canonical `RawProfile` shape (only the six known fields; a string `name`; each
// optional field a string or null) and a usable name. Field *value* validation
// (for example the permission_mode enum) is deliberately left to the daemon.
export function scanProfileJson(content, filePath = '<unknown>') {
  const rawDiagnostic = rawJsonDiagnostic(content);
  if (rawDiagnostic !== null) return invalidJson(`${rawDiagnostic} in ${filePath}`);

  let value;
  try {
    value = JSON.parse(content);
  } catch (err) {
    return invalidJson(err.message);
  }

  if (hasLoneSurrogate(value)) {
    return invalidJson(`lone surrogate code point in ${filePath}`);
  }

  const isObject = value !== null && typeof value === 'object' && !Array.isArray(value);

  let name = null;
  if (isObject && typeof value.name === 'string') {
    const raw = value.name;
    const trimmed = trimRust(raw);
    if (trimmed.length > 0 && Buffer.byteLength(raw, 'utf8') <= 128 && !raw.includes('\0')) {
      name = trimmed;
    }
  }

  const errors = [];
  let shapeValid = true;

  if (!isObject) {
    errors.push('top-level value must be an object');
    shapeValid = false;
  } else {
    for (const key of Object.keys(value)) {
      if (!PROFILE_FIELDS.includes(key)) {
        errors.push(`unknown top-level key '${key}'`);
        shapeValid = false;
      }
    }
    if (!Object.hasOwn(value, 'name')) {
      errors.push("missing required field 'name'");
      shapeValid = false;
    } else if (typeof value.name !== 'string') {
      errors.push("field 'name': must be a string");
      shapeValid = false;
    }
    for (const key of PROFILE_FIELDS) {
      if (key === 'name' || !Object.hasOwn(value, key)) continue;
      const fieldValue = value[key];
      if (fieldValue !== null && typeof fieldValue !== 'string') {
        errors.push(`field '${key}': must be a string`);
        shapeValid = false;
      }
    }
    if (shapeValid && name === null && typeof value.name === 'string') {
      const raw = value.name;
      if (trimRust(raw).length === 0) errors.push("field 'name': cannot be empty");
      else if (Buffer.byteLength(raw, 'utf8') > 128) errors.push("field 'name': exceeds 128 bytes");
      else errors.push("field 'name': cannot contain NUL byte");
    }
  }

  return { name, rawName: name, valid: shapeValid && name !== null, syntaxValid: true, errors };
}

// Rust `Path::extension()` semantics for candidate selection: the extension is
// derived from the file name only and is absent when the name has no dot or
// begins with a dot and has no further dot, so a file named exactly ".json" is
// not a JSON candidate. Comparison stays case-sensitive.
function rustExtension(fileName) {
  const dot = fileName.lastIndexOf('.');
  if (dot <= 0) return null;
  return fileName.slice(dot + 1);
}

// Build a child path by string concatenation, never `path.join`, which
// lexically folds `..` away. Mirrors Rust `Path::join` for the shapes produced
// by `profilesDir` (an empty directory yields the bare name).
function joinChild(dir, name) {
  if (dir === '') return name;
  if (dir.endsWith('/')) return dir + name;
  return `${dir}/${name}`;
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
      // Match the daemon's candidate selection exactly: `Path::extension()`
      // yields "json" (case-sensitive) from the file name, so a file named
      // exactly ".json" has no extension and is never a candidate.
      if (rustExtension(entry.name) !== 'json') continue;
      const full = joinChild(dir, entry.name);
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
      // instead substitute U+FFFD and wrongly accept the file. `ignoreBOM: true`
      // keeps a leading BOM in the decoded string so `JSON.parse` rejects it,
      // matching serde_json; the default decoder would silently strip it.
      const bytes = fs.readFileSync(filePath);
      content = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(bytes);
    } catch (err) {
      fileErrors.push({
        file: filePath,
        diagnostic: `profile file '${filePath}' is unreadable: ${err.message}`,
      });
      continue;
    }

    const res = scanProfileJson(content, filePath);
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
