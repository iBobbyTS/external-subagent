import fs from 'node:fs';
import path from 'node:path';
import { CliError } from '../errors.mjs';
import { sha256 } from '../fs-atomic.mjs';
import { NATIVE_DIR_NAME, PRODUCT_NAME } from '../constants.mjs';
import {
  NATIVE_PLATFORMS, cliVersion, nativePayloadDir, nativePlatform, packageVersion, payloadManifestPath,
} from './layout.mjs';

const MH_MAGIC_64 = 0xfeedfacf;
const CPU_TYPE_ARM64 = 0x0100000c;
// The single executable mode every released payload file must carry.
const RELEASE_FILE_MODE = 0o755;

// ELF64 header identity, read little-endian:
//   e_ident[0..3] = 0x7f 'E' 'L' 'F' (0x464c457f as a LE uint32)
//   e_ident[4]    = EI_CLASS  (2 = ELFCLASS64)
//   e_ident[5]    = EI_DATA   (1 = ELFDATA2LSB)
//   e_machine     = uint16 at offset 18 (0x3e = EM_X86_64)
const ELF_MAGIC = 0x464c457f;
const ELFCLASS64 = 2;
const ELFDATA2LSB = 1;
const EM_X86_64 = 0x3e;
const ELF_E_MACHINE_OFFSET = 18;
const ELF_HEADER_MIN_BYTES = ELF_E_MACHINE_OFFSET + 2;

export function machoArch(bytes) {
  if (bytes.length < 8) return null;
  if (bytes.readUInt32LE(0) !== MH_MAGIC_64) return null;
  return bytes.readUInt32LE(4) === CPU_TYPE_ARM64 ? 'arm64' : null;
}

// The Linux image check mirrors machoArch: only a 64-bit little-endian x86-64
// ELF is a supported payload.  A truncated header (shorter than e_machine), a
// 32-bit or big-endian image, or any other machine returns null and is
// rejected as an unsupported architecture.
export function elfArch(bytes) {
  if (bytes.length < ELF_HEADER_MIN_BYTES) return null;
  if (bytes.readUInt32LE(0) !== ELF_MAGIC) return null;
  if (bytes[4] !== ELFCLASS64 || bytes[5] !== ELFDATA2LSB) return null;
  return bytes.readUInt16LE(ELF_E_MACHINE_OFFSET) === EM_X86_64 ? 'x64' : null;
}

// Each supported platform has exactly one image identity.  verifyPayload reads
// the tuple once and uses the paired probe, so a valid image of the *other*
// platform can never satisfy this one.
const IMAGE_CHECKS = Object.freeze({
  'darwin-arm64': { arch: 'arm64', image: 'Mach-O arm64 image', probe: machoArch },
  'linux-x64': { arch: 'x64', image: 'ELF x86-64 image', probe: elfArch },
});

export function readPayloadManifest(options = {}) {
  const platform = options.platform || nativePlatform();
  const root = options.root || undefined;
  const file = root ? path.join(root, 'npm', NATIVE_DIR_NAME, platform || '', 'payload.json') : payloadManifestPath(platform);
  if (file === null || !fs.existsSync(file)) {
    throw new CliError('PAYLOAD_MANIFEST_MISSING', `the ${platform || 'current'} platform payload manifest is missing from this package`);
  }
  let manifest;
  try {
    manifest = JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch (error) {
    throw new CliError('PAYLOAD_MANIFEST_INVALID', `payload manifest is not valid JSON: ${error.message}`);
  }
  if (manifest?.schema_version !== 1 || manifest?.product !== PRODUCT_NAME || !Array.isArray(manifest.files)) {
    throw new CliError('PAYLOAD_MANIFEST_INVALID', 'payload manifest identity or file table is invalid');
  }
  return manifest;
}

// Verify the staged native payload: the manifest, package, and CLI versions
// must agree, and every binary must exist with release permissions and a
// little-endian Mach-O arm64 image.  A failed verification is an installation
// defect, never a silent downgrade path.
export function verifyPayload(options = {}) {
  const platform = options.platform || nativePlatform();
  if (!NATIVE_PLATFORMS.includes(platform)) {
    throw new CliError('UNSUPPORTED_PAYLOAD_PLATFORM', `no native payload exists for ${platform ?? `${process.platform}-${process.arch}`}; supported platforms: ${NATIVE_PLATFORMS.join(', ')}`);
  }
  const image = IMAGE_CHECKS[platform];
  const manifest = readPayloadManifest({ platform, root: options.root });
  if (manifest.platform !== platform) {
    throw new CliError('PAYLOAD_PLATFORM_MISMATCH', `payload manifest declares ${manifest.platform}, expected ${platform}`);
  }
  let candidatePackage = packageVersion();
  if (options.root) {
    try { candidatePackage = JSON.parse(fs.readFileSync(path.join(options.root, 'package.json'), 'utf8')).version; }
    catch (error) { throw new CliError('PAYLOAD_PACKAGE_INVALID', `candidate package manifest is unavailable: ${error.message}`); }
  }
  const versions = { package: candidatePackage, cli: options.root ? candidatePackage : cliVersion(), payload: manifest.version };
  if (versions.package !== versions.cli || versions.payload !== versions.cli) {
    throw new CliError('PAYLOAD_VERSION_MISMATCH', `payload ${versions.payload}, package ${versions.package}, and CLI ${versions.cli} versions disagree`);
  }
  const dir = options.root ? path.join(options.root, 'npm', NATIVE_DIR_NAME, platform) : nativePayloadDir(platform);
  const files = manifest.files.map((record) => {
    if (!record || typeof record.name !== 'string' || path.basename(record.name) !== record.name || record.name.includes('..')) {
      throw new CliError('PAYLOAD_MANIFEST_INVALID', 'payload file name must be a direct child');
    }
    const target = path.join(dir, record.name);
    let stat;
    try { stat = fs.statSync(target); } catch (error) {
      throw new CliError('PAYLOAD_FILE_MISSING', `native payload file is missing: ${record.name} (${error.code})`);
    }
    if (!stat.isFile() || fs.lstatSync(target).isSymbolicLink() || (stat.mode & 0o777) !== 0o755) {
      throw new CliError('PAYLOAD_PERMISSIONS_INVALID', `native payload file ${record.name} must be a regular file with mode 755`);
    }
    const bytes = fs.readFileSync(target);
    if (bytes.length !== record.bytes || sha256(bytes) !== record.sha256) {
      throw new CliError('PAYLOAD_DIGEST_MISMATCH', `native payload file ${record.name} does not match the release digest`);
    }
    const arch = image.probe(bytes);
    if (arch !== image.arch) {
      throw new CliError('PAYLOAD_ARCH_UNSUPPORTED', `native payload file ${record.name} is not a ${image.image}`);
    }
    return { name: record.name, bytes: bytes.length, mode: '755', sha256: record.sha256, arch };
  });
  return { status: 'verified', platform, version: manifest.version, files };
}

// npm materializes the packaged payload with the permission recorded in the
// tarball, masked by the installing process's umask.  A host running under a
// permissive umask (e.g. 0002) can therefore extract an archived 0755 binary
// as 0775, and the strict gate in verifyPayload below rejects it as
// PAYLOAD_PERMISSIONS_INVALID — correct, since a group/world-writable native
// binary is an installation defect and the gate must never be loosened.  The
// install paths instead repair the extracted mode before verification: only
// the permission is restored, the bytes are untouched, and the digest/arch
// checks still run.  Returns true iff the mode actually changed; a missing or
// non-regular file (directory, symlink) is never chmodded and returns false so
// verifyPayload keeps owning that rejection.
export function normalizePayloadMode(target) {
  let stat;
  try { stat = fs.lstatSync(target); } catch { return false; }
  if (!stat.isFile() || stat.isSymbolicLink()) return false;
  if ((stat.mode & 0o777) === RELEASE_FILE_MODE) return false;
  fs.chmodSync(target, RELEASE_FILE_MODE);
  return true;
}

// Normalize every payload file the manifest names, exactly the set and order
// verifyPayload will read.  An absent/invalid manifest or an unreadable file is
// left for verifyPayload to reject with its own code — this helper never turns
// a rejection into a pass.  Returns the repaired entries with their prior octal
// mode so the install step can report what it changed.
export function normalizePayloadFiles(options = {}) {
  const platform = options.platform || nativePlatform();
  if (!NATIVE_PLATFORMS.includes(platform)) return [];
  let manifest;
  try { manifest = readPayloadManifest({ platform, root: options.root }); } catch { return []; }
  const dir = options.root
    ? path.join(options.root, 'npm', NATIVE_DIR_NAME, platform)
    : nativePayloadDir(platform);
  const normalized = [];
  for (const record of manifest.files) {
    if (!record || typeof record.name !== 'string' || path.basename(record.name) !== record.name || record.name.includes('..')) continue;
    const target = path.join(dir, record.name);
    let stat;
    try { stat = fs.lstatSync(target); } catch { continue; }
    if (!stat.isFile() || stat.isSymbolicLink()) continue;
    const before = stat.mode & 0o777;
    if (normalizePayloadMode(target)) normalized.push({ name: record.name, from: before.toString(8).padStart(3, '0'), to: '755' });
  }
  return normalized;
}
