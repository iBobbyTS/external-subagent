import assert from 'node:assert/strict';
import test from 'node:test';
import { profilesDir, rustPathParent } from '../../cli/paths.mjs';

// The Windows profile directory must mirror the daemon's Rust
// `Path::parent().join("profiles")` exactly (rpc/profiles.rs on a Windows
// target).  Every expectation below was captured from a real Rust binary
// (`std::path::Path`) running on Windows 11, so this table is an oracle for
// the JS twin, not an interpretation of it:
//   path => parent => parent.join("profiles")
// Rust preserves the original separators (mixed `\`/`/` included), drops
// non-leading `.` components, keeps `..`, and returns None for every root or
// prefix-only path.  join() appends with `\`.
const WINDOWS = Object.freeze({ EXTERNAL_SUBAGENT_TEST_PLATFORM: 'win32' });
const win = (config) => profilesDir({ ...WINDOWS, EXTERNAL_SUBAGENT_CONFIG: config });

test('win32 rustPathParent itself returns the AC2 parents', () => {
  assert.equal(rustPathParent('C:\\Users\\admin\\AppData\\Local\\config.json', 'win32'), 'C:\\Users\\admin\\AppData\\Local');
  assert.equal(rustPathParent('C:\\', 'win32'), null);
  assert.equal(rustPathParent('\\\\server\\share\\', 'win32'), null);
  assert.equal(rustPathParent('\\\\?\\C:\\Users\\admin\\config.json', 'win32'), '\\\\?\\C:\\Users\\admin');
  // The Unix grammar is untouched.
  assert.equal(rustPathParent('/etc/app/config.json', 'linux'), '/etc/app');
  assert.equal(rustPathParent('a/b/agents.json', 'linux'), 'a/b');
  assert.equal(rustPathParent('/', 'linux'), null);
});

test('win32 rustPathParent matches Rust Path::parent() for disk, root, UNC, and verbatim paths', () => {
  const cases = [
    ['C:\\Users\\admin\\AppData\\Local\\config.json', 'C:\\Users\\admin\\AppData\\Local\\profiles'],
    ['C:\\', null],
    ['C:/', null],
    ['C:\\Users', 'C:\\profiles'],
    ['C:\\Users\\', 'C:\\profiles'],
    ['C:', null],
    ['C:foo', 'C:profiles'],
    ['C:foo\\bar', 'C:foo\\profiles'],
    ['\\foo\\bar', '\\foo\\profiles'],
    ['\\', null],
    ['/', null],
    ['//', null],
    ['\\\\', null],
    ['\\\\server\\share\\', null],
    ['\\\\server\\share', null],
    ['\\\\server\\share\\dir', '\\\\server\\share\\profiles'],
    ['\\\\server\\share\\dir\\file.txt', '\\\\server\\share\\dir\\profiles'],
    ['\\\\?\\C:\\Users\\admin\\config.json', '\\\\?\\C:\\Users\\admin\\profiles'],
    ['\\\\?\\C:\\', null],
    ['\\\\?\\C:\\Users', '\\\\?\\C:\\profiles'],
    ['\\\\?\\UNC\\server\\share\\dir\\file.txt', '\\\\?\\UNC\\server\\share\\dir\\profiles'],
    ['\\\\?\\UNC\\server\\share\\', null],
    ['\\\\?\\UNC\\server\\share', null],
    ['', null],
    ['.', 'profiles'],
    ['..', 'profiles'],
    ['\\\\.\\pipe\\x', '\\\\.\\pipe\\profiles'],
    ['//foo', '/profiles'],
    ['\\\\server', '\\profiles'],
    ['\\\\server\\', '\\profiles'],
    ['\\\\\\server\\share', '\\\\\\server\\profiles'],
  ];
  for (const [input, expected] of cases) {
    assert.equal(win(input), expected, JSON.stringify(input));
  }
});

test('win32 rustPathParent preserves mixed separators, dot components, and ..', () => {
  const cases = [
    ['a/b', 'a\\profiles'],
    ['a\\b', 'a\\profiles'],
    ['a/b/', 'a\\profiles'],
    ['a\\', 'profiles'],
    ['a/', 'profiles'],
    ['a\\.', 'profiles'],
    ['a\\b\\.', 'a\\profiles'],
    ['a/./b', 'a\\profiles'],
    ['a\\.\\.\\', 'profiles'],
    ['a\\..\\.', 'a\\profiles'],
    ['a\\..', 'a\\profiles'],
    ['a\\b/c\\d', 'a\\b/c\\profiles'],
    ['C:\\Users\\.', 'C:\\profiles'],
    ['C:\\Users\\.\\', 'C:\\profiles'],
    ['C:\\Users\\..', 'C:\\Users\\profiles'],
    ['C:\\.\\Users', 'C:\\profiles'],
    ['C:/./Users', 'C:/profiles'],
    ['\\.\\a', '\\profiles'],
    ['.\\a', '.\\profiles'],
    ['..\\a', '..\\profiles'],
  ];
  for (const [input, expected] of cases) {
    assert.equal(win(input), expected, JSON.stringify(input));
  }
});

test('unix/darwin profile resolution is byte-for-byte unchanged under its own seam', () => {
  const LINUX = Object.freeze({ EXTERNAL_SUBAGENT_TEST_PLATFORM: 'linux' });
  const linux = (config) => profilesDir({ ...LINUX, EXTERNAL_SUBAGENT_CONFIG: config });
  const cases = [
    ['a/../agents.json', 'a/../profiles'],
    ['agents.json', 'profiles'],
    ['.', 'profiles'],
    ['./agents.json', './profiles'],
    ['a/b/agents.json', 'a/b/profiles'],
    ['a/b/', 'a/profiles'],
    ['/', null],
    ['//', null],
    ['', null],
    ['a/.', 'profiles'],
    ['a/b/.', 'a/profiles'],
    ['/.', null],
    ['a/./.', 'profiles'],
    ['./.', 'profiles'],
    ['a/../.', 'a/profiles'],
    ['a/..', 'a/profiles'],
    ['a//.', 'profiles'],
  ];
  for (const [input, expected] of cases) {
    assert.equal(linux(input), expected, JSON.stringify(input));
  }
});
