#!/usr/bin/env node
/**
 * Recomputes every hash in dist/bundle-manifest.json and verifies that the
 * manifest is sorted, complete and consistent with the bytes on disk.
 */
import { readFile, readdir, stat } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { blake3 } from 'hash-wasm';

const PACKAGE_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DIST = join(PACKAGE_ROOT, 'dist');
const MANIFEST = join(DIST, 'bundle-manifest.json');

function fail(message) {
  throw new Error(`verify-manifest: ${message}`);
}

async function main() {
  const manifest = JSON.parse(await readFile(MANIFEST, 'utf8'));
  if (manifest.schema !== 1) fail(`unsupported schema ${String(manifest.schema)}`);
  if (manifest.entry !== 'index.html') fail(`unexpected entry ${String(manifest.entry)}`);
  if (!Array.isArray(manifest.files) || manifest.files.length === 0) fail('files[] is empty');

  const paths = manifest.files.map((file) => file.path);
  const sorted = [...paths].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  if (JSON.stringify(paths) !== JSON.stringify(sorted)) fail('files[] is not sorted by path');
  if (!paths.includes('index.html')) fail('index.html is not listed');

  const onDisk = [];
  async function walk(directory, prefix = '') {
    const entries = await readdir(directory, { withFileTypes: true });
    for (const entry of entries) {
      const path = prefix.length === 0 ? entry.name : `${prefix}/${entry.name}`;
      const absolute = join(directory, entry.name);
      if (entry.isDirectory()) await walk(absolute, path);
      else if (entry.isFile() && path !== 'bundle-manifest.json') onDisk.push(path);
    }
  }
  await walk(DIST);
  onDisk.sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  if (JSON.stringify(onDisk) !== JSON.stringify(sorted)) {
    fail(`manifest does not match dist contents (dist=${onDisk.join(', ')})`);
  }

  for (const file of manifest.files) {
    const absolute = join(DIST, file.path);
    const info = await stat(absolute);
    if (!info.isFile()) fail(`${file.path} is not a file`);
    const bytes = await readFile(absolute);
    if (bytes.byteLength !== file.bytes) {
      fail(`${file.path} byte count ${bytes.byteLength} != manifest ${file.bytes}`);
    }
    const digest = await blake3(bytes);
    if (digest !== file.blake3) fail(`${file.path} blake3 ${digest} != manifest ${file.blake3}`);
  }

  const entry = manifest.files.find((file) => file.path === 'index.html');
  process.stdout.write(
    `verify-manifest: OK — ${manifest.files.length} files, index.html ${entry.bytes} bytes, blake3 ${entry.blake3}\n`,
  );
}

main().catch((error) => {
  process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
  process.exitCode = 1;
});
