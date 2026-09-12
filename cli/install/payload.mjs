import fs from 'node:fs';
import path from 'node:path';
import { CliError } from '../errors.mjs';
import { sha256 } from '../fs-atomic.mjs';
import {
  NATIVE_PLATFORM, cliVersion, nativePayloadDir, nativePlatform, packageVersion, payloadManifestPath,
} from './layout.mjs';

const MH_MAGIC_64 = 0xfeedfacf;
const CPU_TYPE_ARM64 = 0x0100000c;

export function machoArch(bytes) {
  if (bytes.length < 8) return null;
  if (bytes.readUInt32LE(0) !== MH_MAGIC_64) return null;
  return bytes.readUInt32LE(4) === CPU_TYPE_ARM64 ? 'arm64' : null;
}

export function readPayloadManifest(options = {}) {
  const platform = options.platform || nativePlatform();
  const file = payloadManifestPath(platform);
  if (file === null || !fs.existsSync(file)) {
    throw new CliError('PAYLOAD_MANIFEST_MISSING', `the ${platform || 'current'} platform payload manifest is missing from this package`);
  }
  let manifest;
  try {
    manifest = JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch (error) {
    throw new CliError('PAYLOAD_MANIFEST_INVALID', `payload manifest is not valid JSON: ${error.message}`);
  }
  if (manifest?.schema_version !== 1 || manifest?.product !== 'external-subagent' || !Array.isArray(manifest.files)) {
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
  if (platform === null) {
    throw new CliError('UNSUPPORTED_PAYLOAD_PLATFORM', `no native payload exists for ${process.platform}-${process.arch}; supported platforms: ${NATIVE_PLATFORM}`);
  }
  const manifest = readPayloadManifest({ platform });
  if (manifest.platform !== platform) {
    throw new CliError('PAYLOAD_PLATFORM_MISMATCH', `payload manifest declares ${manifest.platform}, expected ${platform}`);
  }
  const versions = { package: packageVersion(), cli: cliVersion(), payload: manifest.version };
  if (versions.package !== versions.cli || versions.payload !== versions.cli) {
    throw new CliError('PAYLOAD_VERSION_MISMATCH', `payload ${versions.payload}, package ${versions.package}, and CLI ${versions.cli} versions disagree`);
  }
  const dir = nativePayloadDir(platform);
  const files = manifest.files.map((record) => {
    const target = path.join(dir, record.name);
    let stat;
    try { stat = fs.statSync(target); } catch (error) {
      throw new CliError('PAYLOAD_FILE_MISSING', `native payload file is missing: ${record.name} (${error.code})`);
    }
    if (!stat.isFile() || (stat.mode & 0o777) !== 0o755) {
      throw new CliError('PAYLOAD_PERMISSIONS_INVALID', `native payload file ${record.name} must be a regular file with mode 755`);
    }
    const bytes = fs.readFileSync(target);
    if (bytes.length !== record.bytes || sha256(bytes) !== record.sha256) {
      throw new CliError('PAYLOAD_DIGEST_MISMATCH', `native payload file ${record.name} does not match the release digest`);
    }
    const arch = machoArch(bytes);
    if (arch !== 'arm64') {
      throw new CliError('PAYLOAD_ARCH_UNSUPPORTED', `native payload file ${record.name} is not a Mach-O arm64 image`);
    }
    return { name: record.name, bytes: bytes.length, mode: '755', sha256: record.sha256, arch };
  });
  return { status: 'verified', platform, version: manifest.version, files };
}
