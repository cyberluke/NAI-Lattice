#!/usr/bin/env node
// QA transport gate: open N concurrent SSE clients against an ISOLATED Lattice
// host and prove the shared broker does one ledger scan per tick for the whole
// set, not one per client. Never point this at production.
//
//   node scripts/qa/sse-load.mjs http://127.0.0.1:8793 12 6
//
// Prints before/during summary counters and per-client liveness. Exits non-zero
// when a client fails to open or no event data arrived.

import http from 'node:http';

const base = process.argv[2] ?? 'http://127.0.0.1:8793';
const count = Number.parseInt(process.argv[3] ?? '12', 10);
const seconds = Number.parseInt(process.argv[4] ?? '6', 10);

if (!Number.isFinite(count) || count < 1 || count > 23 || !Number.isFinite(seconds) || seconds < 1) {
  console.error('usage: node scripts/qa/sse-load.mjs <baseUrl> [1..23 clients] [seconds]');
  process.exit(2);
}

async function summary() {
  const response = await fetch(`${base}/lattice/api/summary`, { cache: 'no-store' });
  if (!response.ok) throw new Error(`summary HTTP ${response.status}`);
  return response.json();
}

const before = await summary();
const sockets = [];
let errors = 0;
let chunks = 0;
let dataLines = 0;

await Promise.all(
  Array.from({ length: count }, () =>
    new Promise((resolve) => {
      const req = http.get(
        `${base}/lattice/events`,
        { headers: { Accept: 'text/event-stream', 'Cache-Control': 'no-store' } },
        (res) => {
          if (res.statusCode !== 200) {
            errors += 1;
            res.resume();
            resolve();
            return;
          }
          res.setEncoding('utf8');
          res.on('data', (chunk) => {
            chunks += 1;
            dataLines += (chunk.match(/^data:/gm) ?? []).length;
          });
          res.on('error', () => {
            errors += 1;
          });
          sockets.push(req);
          resolve();
        },
      );
      req.on('error', () => {
        errors += 1;
        resolve();
      });
    }),
  ),
);

await new Promise((resolve) => setTimeout(resolve, seconds * 1000));
const during = await summary();
for (const socket of sockets) socket.destroy();

const scansBefore = before.stream?.brokerScansTotal ?? 0;
const scansDuring = during.stream?.brokerScansTotal ?? 0;
const ticks = Math.ceil(seconds / 1.25);
const result = {
  base,
  clients: count,
  seconds,
  errors,
  chunks,
  dataLines,
  subscribers: { before: before.stream?.subscribers ?? null, during: during.stream?.subscribers ?? null },
  activeSseClients: during.stream?.activeSseClients ?? null,
  scans: { before: scansBefore, during: scansDuring, delta: scansDuring - scansBefore, ticks },
  droppedTotal: during.stream?.droppedTotal ?? null,
};

console.log(JSON.stringify(result, null, 2));

const scansOk = scansDuring - scansBefore <= ticks + 2;
const clientsOk = errors === 0 && dataLines > 0;
if (!scansOk || !clientsOk) {
  console.error('SSE gate failed: expected one scan per tick for all clients and live data');
  process.exit(4);
}
