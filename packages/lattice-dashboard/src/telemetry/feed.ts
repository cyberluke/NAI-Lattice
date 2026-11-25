import { shallowRef, type ShallowRef } from 'vue';
import { QueryClient, isAbortError, type QueryRange } from '../query/client';
import { EventRing } from './ring';
import {
  type ConnectionState,
  type GapInfo,
  type LifecyclePayload,
  type LogRecord,
  type StatusPayload,
  normalizeLogPayload,
} from './types';

export interface EventSourceLike {
  onopen: ((event: Event) => void) | null;
  onerror: ((event: Event) => void) | null;
  addEventListener(type: string, listener: (event: MessageEvent) => void): void;
  close(): void;
}

export interface TelemetryFeedOptions {
  eventsUrl?: string;
  queryClient?: QueryClient;
  eventSourceFactory?: (url: string) => EventSourceLike;
  capacity?: number;
  /** Batches view notifications on a render cadence. Tests inject a synchronous one. */
  flushScheduler?: (callback: () => void) => void;
  now?: () => number;
  retryBaseMs?: number;
  retryMaxMs?: number;
  maxAttempts?: number;
  isOnline?: () => boolean;
}

const DEFAULT_CAPACITY = 10_000;

function defaultScheduler(callback: () => void): void {
  if (typeof requestAnimationFrame === 'function') {
    requestAnimationFrame(() => callback());
    return;
  }
  setTimeout(callback, 16);
}

function defaultEventSourceFactory(url: string): EventSourceLike {
  if (typeof EventSource === 'undefined') {
    throw new Error('EventSource is unavailable in this runtime');
  }
  return new EventSource(url) as unknown as EventSourceLike;
}

function safeJson(data: string): unknown {
  try {
    return JSON.parse(data) as unknown;
  } catch {
    return null;
  }
}

/**
 * One upstream SSE lease for the whole dashboard document.
 *
 * Responsibilities: cursor resume, dedupe by `runtime_sequence`, bounded ring,
 * truthful gap/drop reporting and connection state. It does not own any panel's
 * frozen view; those are `LogView` instances over the same feed.
 */
export class TelemetryFeed {
  readonly connection: ShallowRef<ConnectionState> = shallowRef('connecting');
  readonly status: ShallowRef<StatusPayload | null> = shallowRef(null);
  readonly stats: ShallowRef<Record<string, unknown> | null> = shallowRef(null);
  readonly lifecycle: ShallowRef<LifecyclePayload | null> = shallowRef(null);
  readonly lastError: ShallowRef<string | null> = shallowRef(null);
  readonly lastEventAt: ShallowRef<number | null> = shallowRef(null);
  /** Reactive cursor for header display, advanced only on the render cadence. */
  readonly cursor: ShallowRef<number> = shallowRef(0);
  readonly gaps: ShallowRef<readonly GapInfo[]> = shallowRef([]);
  readonly malformedCount: ShallowRef<number> = shallowRef(0);

  private readonly eventsUrl: string;
  private readonly queryClient: QueryClient;
  private readonly eventSourceFactory: (url: string) => EventSourceLike;
  private readonly flushScheduler: (callback: () => void) => void;
  private readonly now: () => number;
  private readonly retryBaseMs: number;
  private readonly retryMaxMs: number;
  private readonly maxAttempts: number;
  private readonly isOnline: () => boolean;
  private readonly ring: EventRing<LogRecord>;
  private readonly listeners = new Set<() => void>();
  private gapList: GapInfo[] = [];
  private source: EventSourceLike | null = null;
  private started = false;
  private disposed = false;
  private flushScheduled = false;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private attempts = 0;
  private cursorValue = 0;
  private lastEventAtValue: number | null = null;
  private droppedTotal: number | null = null;
  private removeGlobalListeners: Array<() => void> = [];

  constructor(options: TelemetryFeedOptions = {}) {
    this.eventsUrl = options.eventsUrl ?? '/lattice/events';
    this.queryClient = options.queryClient ?? new QueryClient();
    this.eventSourceFactory = options.eventSourceFactory ?? defaultEventSourceFactory;
    this.flushScheduler = options.flushScheduler ?? defaultScheduler;
    this.now = options.now ?? (() => Date.now());
    this.retryBaseMs = options.retryBaseMs ?? 500;
    this.retryMaxMs = options.retryMaxMs ?? 8_000;
    this.maxAttempts = options.maxAttempts ?? 8;
    this.isOnline =
      options.isOnline ??
      (() => (typeof navigator === 'undefined' ? true : navigator.onLine !== false));
    this.ring = new EventRing<LogRecord>(options.capacity ?? DEFAULT_CAPACITY);
  }

  get ingestedCursor(): number {
    return this.cursorValue;
  }

  get oldestSeq(): number | null {
    return this.ring.oldest?.runtime_sequence ?? null;
  }

  get size(): number {
    return this.ring.size;
  }

  start(): void {
    if (this.started || this.disposed) return;
    this.started = true;
    this.installGlobalListeners();
    if (!this.isOnline()) {
      this.connection.value = 'offline';
      return;
    }
    this.connect();
  }

  stop(): void {
    this.started = false;
    this.clearReconnectTimer();
    this.closeSource();
    if (this.connection.value !== 'offline') this.connection.value = 'error';
  }

  dispose(): void {
    this.disposed = true;
    this.started = false;
    this.clearReconnectTimer();
    this.closeSource();
    for (const remove of this.removeGlobalListeners.splice(0)) remove();
    this.listeners.clear();
  }

  /** Manual recovery from the terminal `error` state. */
  retry(): void {
    if (this.disposed) return;
    this.attempts = 0;
    this.lastError.value = null;
    this.started = true;
    if (!this.isOnline()) {
      this.connection.value = 'offline';
      return;
    }
    this.connect();
  }

  /**
   * Registers a render-cadence callback. The callback fires at most once per
   * animation frame regardless of ingest rate.
   */
  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  snapshotAfter(afterSeq: number, limit = Number.POSITIVE_INFINITY): LogRecord[] {
    const matching = this.ring.toArray().filter((record) => record.runtime_sequence > afterSeq);
    if (Number.isFinite(limit) && matching.length > limit) {
      return matching.slice(matching.length - Math.max(0, Math.trunc(limit)));
    }
    return matching;
  }

  /** Bounded historical catch-up; used by a frozen view on resume. */
  catchUpRange(params: {
    after: number;
    until: number;
    signal: AbortSignal;
    pageLimit?: number;
    maxPages?: number;
  }): Promise<QueryRange> {
    return this.queryClient.fetchRange({
      after: params.after,
      until: params.until,
      pageLimit: params.pageLimit ?? 500,
      maxPages: params.maxPages ?? 5,
      signal: params.signal,
    });
  }

  private connect(): void {
    if (this.disposed || !this.started) return;
    this.closeSource();
    if (!this.isOnline()) {
      this.connection.value = 'offline';
      return;
    }
    this.connection.value = this.attempts > 0 ? 'retrying' : 'connecting';
    const url = this.resumeUrl();
    let source: EventSourceLike;
    try {
      source = this.eventSourceFactory(url);
    } catch (error) {
      this.handleFailure(error instanceof Error ? error.message : 'SSE is unavailable');
      return;
    }
    this.source = source;
    source.onopen = () => {
      if (this.source !== source || this.disposed) return;
      this.attempts = 0;
      this.lastError.value = null;
      this.connection.value = 'live';
    };
    source.onerror = () => {
      if (this.source !== source || this.disposed) return;
      this.handleFailure('stream disconnected');
    };
    source.addEventListener('search', (event) => {
      if (this.source !== source) return;
      this.onDataEvent('search', event);
    });
    source.addEventListener('log', (event) => {
      if (this.source !== source) return;
      this.onDataEvent('log', event);
    });
    source.addEventListener('status', (event) => {
      if (this.source !== source) return;
      this.onStatusEvent(event);
    });
    source.addEventListener('stats', (event) => {
      if (this.source !== source) return;
      this.onStatsEvent(event);
    });
    source.addEventListener('state', (event) => {
      if (this.source !== source) return;
      this.onLifecycleEvent(event);
    });
  }

  private resumeUrl(): string {
    if (this.cursorValue <= 0) return this.eventsUrl;
    const separator = this.eventsUrl.includes('?') ? '&' : '?';
    return `${this.eventsUrl}${separator}after=${this.cursorValue}`;
  }

  private onDataEvent(kind: 'search' | 'log', event: MessageEvent): void {
    const payload = safeJson(typeof event.data === 'string' ? event.data : '');
    if (payload === null) {
      this.malformedCount.value += 1;
      return;
    }
    const records = normalizeLogPayload(payload);
    if (records.length === 0 && kind === 'log') {
      // An empty delta is legal; do not manufacture a gap.
      return;
    }
    this.ingest(records);
  }

  private ingest(records: readonly LogRecord[]): void {
    let changed = false;
    for (const record of records) {
      if (record.runtime_sequence <= this.cursorValue) continue;
      // `runtime_sequence` is ledger-global, but this stream carries only the
      // namespaces a dashboard subscribes to. A numeric jump therefore says
      // nothing about missing rows here: other namespaces own those ids.
      // Real loss is reported only by the broker's cumulative `droppedTotal`.
      this.cursorValue = record.runtime_sequence;
      this.lastEventAtValue = this.now();
      this.ring.push(record);
      changed = true;
    }
    if (!changed) return;
    this.scheduleFlush();
  }

  private onStatusEvent(event: MessageEvent): void {
    const payload = safeJson(typeof event.data === 'string' ? event.data : '');
    if (payload === null || typeof payload !== 'object' || Array.isArray(payload)) {
      this.malformedCount.value += 1;
      return;
    }
    const status = payload as StatusPayload;
    const dropped = typeof status.droppedTotal === 'number' ? status.droppedTotal : null;
    if (dropped !== null) {
      const previous = this.droppedTotal;
      if (dropped > 0 && (previous === null || dropped > previous)) {
        this.noteGap({
          from: this.cursorValue > 0 ? this.cursorValue + 1 : 0,
          to: null,
          reason: 'dropped',
          count: previous === null ? dropped : dropped - previous,
        });
      }
      // Track resets too: a broker restart legitimately lowers the cumulative
      // counter, and the next increase must be measured from the new base.
      this.droppedTotal = dropped;
    }
    this.status.value = status;
    this.lastEventAtValue = this.now();
    this.scheduleFlush();
  }

  private onStatsEvent(event: MessageEvent): void {
    const payload = safeJson(typeof event.data === 'string' ? event.data : '');
    if (payload === null || typeof payload !== 'object' || Array.isArray(payload)) {
      this.malformedCount.value += 1;
      return;
    }
    this.stats.value = payload as Record<string, unknown>;
    this.lastEventAtValue = this.now();
    this.scheduleFlush();
  }

  private onLifecycleEvent(event: MessageEvent): void {
    const payload = safeJson(typeof event.data === 'string' ? event.data : '');
    if (payload === null || typeof payload !== 'object' || Array.isArray(payload)) {
      this.malformedCount.value += 1;
      return;
    }
    this.lifecycle.value = payload as LifecyclePayload;
    this.lastEventAtValue = this.now();
    this.scheduleFlush();
  }

  private noteGap(gap: GapInfo): void {
    const last = this.gapList[this.gapList.length - 1];
    if (
      last !== undefined &&
      last.reason === gap.reason &&
      last.to !== null &&
      gap.from === last.to + 1
    ) {
      last.to = gap.to;
      if (gap.count !== undefined) last.count = (last.count ?? 0) + gap.count;
    } else {
      this.gapList.push(gap);
      if (this.gapList.length > 20) this.gapList.shift();
    }
    this.gaps.value = this.gapList.map((item) => ({ ...item }));
  }

  private handleFailure(message: string): void {
    this.closeSource();
    if (this.disposed) return;
    if (!this.isOnline()) {
      this.connection.value = 'offline';
      this.lastError.value = null;
      return;
    }
    this.attempts += 1;
    this.lastError.value = message;
    if (this.attempts > this.maxAttempts) {
      this.connection.value = 'error';
      return;
    }
    this.connection.value = 'retrying';
    const delay = Math.min(
      this.retryMaxMs,
      this.retryBaseMs * 2 ** Math.max(0, this.attempts - 1),
    );
    const jitter = Math.floor(Math.random() * Math.min(250, this.retryBaseMs));
    this.clearReconnectTimer();
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, delay + jitter);
  }

  private closeSource(): void {
    if (this.source !== null) {
      const source = this.source;
      this.source = null;
      source.onopen = null;
      source.onerror = null;
      source.close();
    }
  }

  private clearReconnectTimer(): void {
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  private installGlobalListeners(): void {
    if (typeof window === 'undefined' || typeof window.addEventListener !== 'function') return;
    const onOnline = () => {
      if (this.disposed) return;
      this.attempts = 0;
      this.lastError.value = null;
      this.connect();
    };
    const onOffline = () => {
      if (this.disposed) return;
      this.clearReconnectTimer();
      this.closeSource();
      this.connection.value = 'offline';
      this.lastError.value = null;
    };
    window.addEventListener('online', onOnline);
    window.addEventListener('offline', onOffline);
    this.removeGlobalListeners.push(() => {
      window.removeEventListener('online', onOnline);
      window.removeEventListener('offline', onOffline);
    });
  }

  private scheduleFlush(): void {
    if (this.flushScheduled || this.disposed) return;
    this.flushScheduled = true;
    this.flushScheduler(() => {
      this.flushScheduled = false;
      if (this.disposed) return;
      this.cursor.value = this.cursorValue;
      this.lastEventAt.value = this.lastEventAtValue;
      for (const listener of this.listeners) {
        try {
          listener();
        } catch {
          // One panel must not break the shared transport.
        }
      }
    });
  }
}

/** True when an abort ended a catch-up request. */
export { isAbortError };
