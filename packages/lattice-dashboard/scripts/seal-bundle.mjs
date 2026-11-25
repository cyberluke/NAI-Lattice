#!/usr/bin/env node
/**
 * Seals the Vite output with a deterministic BLAKE3 bundle manifest
 * (docs/DASHBOARD_API.md §7).
 *
 * Fails when index.html is missing or when index.html / built JS / CSS
 * references an asset that is not present in dist. The host rejects any path
 * outside this manifest, so a stale or incomplete bundle must never ship.
 */
import { readFile, readdir, stat, writeFile } from 'node:fs/promises';
import { dirname, join, resolve, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { blake3 } from 'hash-wasm';

const PACKAGE_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DIST = join(PACKAGE_ROOT, 'dist');
const BASE = '/lattice/';
const MANIFEST_NAME = 'bundle-manifest.json';
const TEXT_EXTENSIONS = new Set(['.html', '.js', '.css', '.svg']);
const ASSET_REF = /(?:^|[^A-Za-z0-9._-])(assets\/[A-Za-z0-9._-]+\.(?:js|css|woff2?|png|svg|ico|webp|json))/g;
const RELATIVE_ASSET_REF = /["'`](\.{1,2}\/[A-Za-z0-9._/-]+\.(?:js|css|woff2?|png|svg|ico|webp|json))["'`]/g;

function fail(message) {
  throw new Error(`seal-bundle: ${message}`);
}

async function listFiles(directory, prefix = '') {
  const entries = await readdir(directory, { withFileTypes: true });
  const out = [];
  for (const entry of entries) {
    const childPrefix = prefix.length === 0 ? entry.name : `${prefix}/${entry.name}`;
    const absolute = join(directory, entry.name);
    if (entry.isDirectory()) {
      out.push(...(await listFiles(absolute, childPrefix)));
    } else if (entry.isFile()) {
      out.push({ path: childPrefix, absolute });
    }
  }
  return out;
}

function withinDist(absolute) {
  return absolute === DIST || absolute.startsWith(`${DIST}${sep}`);
}

async function assertFile(absolute, label) {
  let info;
  try {
    info = await stat(absolute);
  } catch {
    fail(`${label} is referenced but missing`);
  }
  if (!info.isFile()) fail(`${label} is referenced but is not a file`);
}

function resolveReference(reference) {
  let value = reference.trim();
  if (value.length === 0) return null;
  if (/^(?:[a-z]+:|\/\/|#|data:)/i.test(value)) return null;
  value = value.split(/[?#]/)[0];
  if (value.startsWith(BASE)) value = value.slice(BASE.length);
  else if (value.startsWith('/')) value = value.slice(1);
  const absolute = resolve(DIST, value);
  if (!withinDist(absolute)) fail(`asset reference escapes dist: ${reference}`);
  return absolute;
}

async function main() {
  const packageJson = JSON.parse(await readFile(join(PACKAGE_ROOT, 'package.json'), 'utf8'));
  const indexPath = join(DIST, 'index.html');
  await assertFile(indexPath, 'index.html');

  const files = (await listFiles(DIST))
    .filter((file) => file.path !== MANIFEST_NAME)
    .sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
  if (!files.some((file) => file.path === 'index.html')) fail('index.html is not part of dist');

  const byPath = new Map(files.map((file) => [file.path, file]));

  const indexHtml = await readFile(indexPath, 'utf8');
  const references = [...indexHtml.matchAll(/(?:src|href)\s*=\s*"([^"]+)"/g)].map(
    (match) => match[1],
  );
  for (const reference of references) {
    if (/^(?:https?:|\/\/)/i.test(reference.trim())) {
      fail(`index.html references an external origin: ${reference}`);
    }
    const absolute = resolveReference(reference);
    if (absolute === null) continue;
    await assertFile(absolute, `index.html reference "${reference}"`);
  }

  for (const file of files) {
    const extension = file.path.slice(file.path.lastIndexOf('.'));
    if (!TEXT_EXTENSIONS.has(extension)) continue;
    const text = await readFile(file.absolute, 'utf8');
    for (const match of text.matchAll(ASSET_REF)) {
      const candidate = match[1];
      if (!byPath.has(candidate)) fail(`${file.path} references missing asset ${candidate}`);
    }
    for (const match of text.matchAll(RELATIVE_ASSET_REF)) {
      const candidate = match[1];
      const absolute = resolve(dirname(file.absolute), candidate);
      if (!withinDist(absolute)) fail(`${file.path} references an asset outside dist: ${candidate}`);
      await assertFile(absolute, `${file.path} relative reference "${candidate}"`);
    }
  }

  const manifestFiles = [];
  for (const file of files) {
    const bytes = await readFile(file.absolute);
    manifestFiles.push({
      path: file.path,
      bytes: bytes.byteLength,
      blake3: await blake3(bytes),
    });
  }

  const epoch = process.env.SOURCE_DATE_EPOCH;
  const builtAt =
    epoch !== undefined && /^\d+$/.test(epoch)
      ? new Date(Number(epoch) * 1000).toISOString()
      : new Date().toISOString();

  const manifest = {
    schema: 1,
    name: packageJson.name,
    version: packageJson.version,
    builtAt,
    entry: 'index.html',
    files: manifestFiles,
  };

  await writeFile(join(DIST, MANIFEST_NAME), `${JSON.stringify(manifest, null, 2)}\n`, 'utf8');
  const entry = manifestFiles.find((file) => file.path === 'index.html');
  if (entry === undefined) fail('index.html missing from manifest');
  process.stdout.write(
    `seal-bundle: ${manifestFiles.length} files, entry index.html (${entry.bytes} bytes) blake3=${entry.blake3}\n`,
  );
}

main().catch((error) => {
  process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
  process.exitCode = 1;
});
