#!/usr/bin/env node
// Seeds an ISOLATED NAI-Lattice QA host with deterministic demo telemetry so the
// dashboard's Log Tail, Components, Storage and Traces panels have real data.
//
// NEVER point this at the production host. Use a scratch NAI_LATTICE_ROOT plus
// NAI_LATTICE_INSTANCE (see docs/DASHBOARD_API.md §6.1) and pass that instance's
// pipe:
//
//   $env:NAI_FABRIC_PIPE = '\\.\pipe\NAI.AmbientFabric.NAF1.<sid6>.qa-dashboard'
//   node scripts/qa/seed-dashboard-demo.mjs 150
//
// The script appends through the public NAF client (event.append), so every row
// is a real canonical ledger record and is visible to SSE, query and traces.

import { readFile } from 'node:fs/promises';
import { NafClient } from '@cyberluke/nai-lattice-client';

/**
 * Explicit span/edge fixtures. Kept in a data file so the dashboard test suite
 * verifies the exact shape the host graph producer consumes without executing
 * the NAF seeder.
 */
const explicitFixtures = JSON.parse(
  await readFile(new URL('./dashboard-demo-fixtures.json', import.meta.url), 'utf8'),
);

const envPipe = process.env.NAI_FABRIC_PIPE;
const pipe = envPipe || process.argv[2];
// With an explicit env pipe, argv[2] is the count; otherwise argv[2] is the pipe
// and argv[3] is the count.
const countArg = envPipe
  ? process.argv[2] ?? process.env.NAI_SEED_COUNT ?? '150'
  : process.argv[3] ?? process.env.NAI_SEED_COUNT ?? '150';
const count = Number.parseInt(countArg, 10);

if (!pipe) {
  console.error('usage: NAI_FABRIC_PIPE=\\\\.\\pipe\\... node scripts/qa/seed-dashboard-demo.mjs [count]');
  process.exit(2);
}
if (!Number.isFinite(count) || count < 1 || count > 5000) {
  console.error('count must be 1..5000');
  process.exit(2);
}

const COMPONENTS = [
  'nai-os.observability',
  'nai-os.shell',
  'nai-lattice.host',
  'nai-os.agent',
  'nai-os.terminal',
];
const LEVELS = ['info', 'info', 'info', 'debug', 'warn', 'error'];
const NAMESPACE = 'observability.log.v1';

const client = new NafClient({
  appId: 'nai-lattice-qa-seed',
  clientVersion: '0.1.3',
  requestedCapabilities: ['event.append'],
  pipePath: pipe,
});

async function append(record, idempotencyKey) {
  const result = await client.eventAppend(
    {
      namespace: NAMESPACE,
      event_type: 'log.record',
      timestamp_utc_ms: record.ts,
      payload: {
        ts_ms: record.ts,
        level: record.level,
        component: record.component,
        trace_id: record.traceId ?? null,
        caller: 'qa-seed',
        message: record.message,
        extras: record.extras ?? {},
      },
    },
    idempotencyKey,
  );
  if (!result.ok || result.status !== 0) {
    throw new Error(result.error || `event.append failed with status ${result.status}`);
  }
}

await client.connect();
let seeded = 0;
try {
  const now = Date.now();
  for (let i = 0; i < count; i += 1) {
    const level = LEVELS[i % LEVELS.length];
    const component = COMPONENTS[i % COMPONENTS.length];
    const traceId = i % 25 === 0 ? `demo-trace-${Math.floor(i / 25)}` : null;
    await append({
      ts: now - (count - i) * 750,
      level,
      component,
      traceId,
      message: `${component}: demo event ${i} (${level})`,
      extras: { scenario: 'dashboard-demo', index: i },
    });
    seeded += 1;
  }

  // One complete request -> tool -> terminal trace across components.
  const traceId = 'demo-trace-full';
  const t0 = Date.now() - 4_000;
  const span = async (offset, component, message, extras, idem) => {
    await append(
      {
        ts: t0 + offset,
        level: 'info',
        component,
        traceId,
        message,
        extras: { ...extras, scenario: 'dashboard-demo' },
      },
      idem,
    );
    seeded += 1;
  };
  await span(0, 'nai-os.shell', 'turn.submitted', { event_type: 'turn.submitted' }, 'qa-demo-span-1');
  await span(650, 'nai-os.agent', 'tool.receipt read_file', { event_type: 'tool.receipt', duration_ms: 650 }, 'qa-demo-span-2');
  await span(
    1_200,
    'nai-os.terminal',
    'turn.terminal completed',
    { event_type: 'turn.terminal', duration_ms: 1_200, outcome: 'completed' },
    'qa-demo-span-3',
  );

  // Multi-component flows so the topology graph has real edges (and one error).
  const flowComponents = ['nai-os.shell', 'nai-os.agent', 'nai-os.terminal', 'nai-os.observability'];
  for (let f = 0; f < 6; f += 1) {
    const flowTrace = `qa-flow-${f}`;
    const flowBase = Date.now() - (f + 1) * 45_000;
    const steps = [
      [0, 0, 'turn.submitted', { event_type: 'turn.submitted' }, 'info'],
      [250, 1, 'tool.receipt read_file', { event_type: 'tool.receipt', duration_ms: 250 }, f === 2 ? 'error' : 'info'],
      [700, 2, 'process.exec', { event_type: 'log' }, 'info'],
      [1_100, 3, 'turn.terminal completed', { event_type: 'turn.terminal', duration_ms: 1_100, outcome: 'completed' }, 'info'],
    ];
    for (const [offset, componentIndex, message, extras, level] of steps) {
      await append(
        {
          ts: flowBase + offset,
          level,
          component: flowComponents[componentIndex],
          traceId: flowTrace,
          message,
          extras: { ...extras, scenario: 'dashboard-demo', flow: f },
        },
        `qa-flow-${f}-${componentIndex}`,
      );
      seeded += 1;
    }
  }

  // Explicit schema-2 evidence: spans with stable span ids, one call observation
  // and one point event, plus a parent-only fan-out trace. New trace ids and new
  // idempotency keys; the records above are unchanged.
  const explicitNow = Date.now();
  for (const fixture of explicitFixtures.traces) {
    for (const entry of fixture.records) {
      await append(
        {
          ts: explicitNow + fixture.baseOffsetMs + entry.offsetMs,
          level: entry.level,
          component: entry.component,
          traceId: fixture.traceId,
          message: entry.message,
          extras: entry.extras,
        },
        entry.key,
      );
      seeded += 1;
    }
  }
} finally {
  await client.close();
}

console.log(JSON.stringify({ seeded, pipe, namespace: NAMESPACE }));
