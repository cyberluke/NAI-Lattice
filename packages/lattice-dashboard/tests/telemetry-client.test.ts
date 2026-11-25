import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { TelemetryFeed, type EventSourceLike } from '../src/telemetry/feed';
import { LogView } from '../src/telemetry/view';
import type { LogRecord } from '../src/telemetry/types';
import type { QueryRange } from '../src/query/client';

class FakeEventSource implements EventSourceLike {
  static instances: FakeEventSource[] = [];
  readonly url: string;
  onopen: ((event: Event) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  closed = false;
  private readonly listeners = new Map<string, (event: MessageEvent) => void>();

  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }

  addEventListener(type: string, listener: (event: MessageEvent) => void): void {
    this.listeners.set(type, listener);
  }

  close(): void {
    this.closed = true;
  }

  open(): void {
    this.onopen?.(new Event('open'));
  }

  fail(): void {
    this.onerror?.(new Event('error'));
  }

  emit(type: string, payload: unknown): void {
    const listener = this.listeners.get(type);
    if (listener === undefined) return;
    listener({ data: JSON.stringify(payload), lastEventId: '' } as MessageEvent);
  }
}

function record(sequence: number, message = `row-${sequence}`): LogRecord {
  return {
    runtime_sequence: sequence,
    timestamp_utc_ms: 1_764_000_000_000 + sequence,
    level: 'info',
    component: 'test.component',
    trace_id: null,
    message,
  };
}

function makeFeed(): TelemetryFeed {
  FakeEventSource.instances = [];
  return new TelemetryFeed({
    eventSourceFactory: (url) => new FakeEventSource(url),
    flushScheduler: (callback) => callback(),
    retryBaseMs: 10,
    retryMaxMs: 20,
    maxAttempts: 3,
    capacity: 16,
    isOnline: () => true,
    now: () => 1_764_000_000_000,
  });
}

class FakeLogFeed {
  records: LogRecord[] = [];
  ingestedCursor = 0;
  oldestSeq: number | null = null;
  catchUpCalls: Array<{ after: number; until: number }> = [];
  catchUpRecords: LogRecord[] = [];
  private listeners = new Set<() => void>();

  push(...records: LogRecord[]): void {
    for (const next of records) {
      this.records.push(next);
      this.ingestedCursor = Math.max(this.ingestedCursor, next.runtime_sequence);
      this.oldestSeq = this.records[0]?.runtime_sequence ?? null;
    }
    for (const listener of this.listeners) listener();
  }

  snapshotAfter(afterSeq: number, limit = Number.POSITIVE_INFINITY): LogRecord[] {
    const matching = this.records.filter((row) => row.runtime_sequence > afterSeq);
    return Number.isFinite(limit) ? matching.slice(-Math.trunc(limit)) : matching;
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  catchUpRange(params: { after: number; until: number }): Promise<QueryRange> {
    this.catchUpCalls.push({ after: params.after, until: params.until });
    return Promise.resolve({
      records: this.catchUpRecords.filter(
        (row) => row.runtime_sequence > params.after && row.runtime_sequence <= params.until,
      ),
      complete: true,
      nextAfter: params.until,
      pages: 1,
    });
  }
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.spyOn(Math, 'random').mockReturnValue(0);
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe('TelemetryFeed', () => {
  it('resumes from the cursor and dedupes by runtime_sequence', () => {
    const feed = makeFeed();
    feed.start();
    const first = FakeEventSource.instances[0];
    expect(first.url).toBe('/lattice/events');
    first.open();
    expect(feed.connection.value).toBe('live');

    first.emit('search', [record(1), record(2)]);
    expect(feed.ingestedCursor).toBe(2);
    expect(feed.snapshotAfter(0).map((row) => row.runtime_sequence)).toEqual([1, 2]);

    first.emit('log', { hits: [record(2), record(3)] });
    expect(feed.snapshotAfter(0).map((row) => row.runtime_sequence)).toEqual([1, 2, 3]);

    first.fail();
    expect(feed.connection.value).toBe('retrying');
    expect(first.closed).toBe(true);
    vi.advanceTimersByTime(10);

    const second = FakeEventSource.instances[1];
    expect(second.url).toBe('/lattice/events?after=3');
    second.open();
    second.emit('log', { hits: [record(3), record(4)] });
    expect(feed.snapshotAfter(0).map((row) => row.runtime_sequence)).toEqual([1, 2, 3, 4]);
    feed.dispose();
  });

  it('does not invent gaps on ledger-global sequence jumps but reports broker drops', () => {
    const feed = makeFeed();
    feed.start();
    const source = FakeEventSource.instances[0];
    source.open();

    source.emit('log', { hits: [record(1), record(2)] });
    // runtime_sequence is ledger-global: sequences 3 and 4 belong to other
    // namespaces and this stream is not missing anything.
    source.emit('log', { hits: [record(5)] });
    expect(feed.ingestedCursor).toBe(5);
    expect(feed.gaps.value).toEqual([]);

    source.emit('status', { droppedTotal: 2, activeSseClients: 1, maxSseClients: 6 });
    expect(feed.status.value?.droppedTotal).toBe(2);
    const dropped = feed.gaps.value.find((gap) => gap.reason === 'dropped');
    expect(dropped).toMatchObject({ from: 6, to: null, count: 2 });

    source.emit('status', { droppedTotal: 2 });
    expect(feed.gaps.value.filter((gap) => gap.reason === 'dropped')).toHaveLength(1);
    feed.dispose();
  });

  it('marks a terminal error after the retry budget and recovers manually', () => {
    const feed = makeFeed();
    feed.start();
    for (let attempt = 0; attempt < 4; attempt += 1) {
      const source = FakeEventSource.instances[FakeEventSource.instances.length - 1];
      source.fail();
      vi.advanceTimersByTime(50);
    }
    expect(feed.connection.value).toBe('error');
    const before = FakeEventSource.instances.length;
    feed.retry();
    expect(FakeEventSource.instances.length).toBe(before + 1);
    feed.dispose();
  });
});

describe('LogView pause / resume / clear', () => {
  it('freezes the view, retains ingestion and catches up from the displayed cursor', async () => {
    const feed = new FakeLogFeed();
    const updates: Array<{ rows: readonly LogRecord[]; paused: boolean; lastSeq: number }> = [];
    const view = new LogView(feed, {
      capacity: 10,
      onUpdate: (rows, meta) => updates.push({ rows, paused: meta.paused, lastSeq: meta.lastSeq }),
    });
    view.start();
    feed.push(record(1), record(2), record(3));
    expect(updates.at(-1)?.rows.map((row) => row.runtime_sequence)).toEqual([1, 2, 3]);

    view.pause();
    feed.push(record(4), record(5));
    expect(feed.ingestedCursor).toBe(5);
    expect(updates.at(-1)?.rows.map((row) => row.runtime_sequence)).toEqual([1, 2, 3]);
    expect(updates.at(-1)?.paused).toBe(true);

    // Simulate ring eviction while paused: only the query can recover the interval.
    feed.records = [];
    feed.oldestSeq = 5;
    feed.catchUpRecords = [record(4), record(5)];
    await view.resume();

    expect(feed.catchUpCalls).toEqual([{ after: 3, until: 5 }]);
    expect(updates.at(-1)?.rows.map((row) => row.runtime_sequence)).toEqual([1, 2, 3, 4, 5]);
    expect(updates.at(-1)?.paused).toBe(false);
    view.dispose();
  });

  it('clears only the view and keeps the ingestion cursor', () => {
    const feed = new FakeLogFeed();
    let latest: readonly LogRecord[] = [];
    const view = new LogView(feed, { capacity: 10, onUpdate: (rows) => (latest = rows) });
    view.start();
    feed.push(record(10), record(11));
    expect(latest.map((row) => row.runtime_sequence)).toEqual([10, 11]);

    view.clear();
    expect(latest).toHaveLength(0);
    expect(feed.ingestedCursor).toBe(11);
    expect(feed.records).toHaveLength(2);

    feed.push(record(12));
    expect(latest.map((row) => row.runtime_sequence)).toEqual([12]);
    view.dispose();
  });
});
