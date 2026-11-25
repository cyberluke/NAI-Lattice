import {
  type LogLevel,
  type LogRecord,
  normalizeLogPayload,
} from '../telemetry/types';

export interface QueryFilters {
  level?: LogLevel | '';
  component?: string;
  trace?: string;
  q?: string;
  since_ms?: number;
  until_ms?: number;
  after?: number;
  limit?: number;
  scan_budget?: number;
}

export interface QueryPage {
  schema: number;
  hits: LogRecord[];
  count: number;
  nextAfter: number | null;
  hasMore: boolean;
  exhausted: boolean;
  scanned: number | null;
  budgetHit: boolean;
  order: string;
  generatedAt: number | null;
}

export interface QueryRange {
  records: LogRecord[];
  complete: boolean;
  nextAfter: number;
  pages: number;
}

export interface QueryClientOptions {
  baseUrl?: string;
  path?: string;
  fetchImpl?: typeof fetch;
}

export class QueryError extends Error {
  readonly status: number | null;

  constructor(message: string, status: number | null = null) {
    super(message);
    this.name = 'QueryError';
    this.status = status;
  }
}

const DEFAULT_PAGE_LIMIT = 200;
const MAX_PAGE_LIMIT = 1000;
const DEFAULT_SCAN_BUDGET = 200_000;

function clampInt(value: number | undefined, min: number, max: number, fallback: number): number {
  if (value === undefined || !Number.isFinite(value)) return fallback;
  const n = Math.trunc(value);
  return Math.min(max, Math.max(min, n));
}

function optionalInt(value: number | undefined): number | null {
  if (value === undefined || !Number.isFinite(value)) return null;
  return Math.trunc(value);
}

function optionalString(value: string | undefined): string | null {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
}

function numberOrNull(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

/**
 * Client for `GET /lattice/api/query`. Paging is keyset-based on
 * `runtime_sequence`; pages are ascending so callers append without sorting.
 */
export class QueryClient {
  private readonly baseUrl: string;
  private readonly path: string;
  private readonly fetchImpl: typeof fetch | null;

  constructor(options: QueryClientOptions = {}) {
    this.baseUrl = (options.baseUrl ?? '').replace(/\/+$/, '');
    this.path = options.path ?? '/lattice/api/query';
    this.fetchImpl =
      options.fetchImpl ??
      // `fetch` must keep its Window receiver. Extracting it unbound throws
      // "Illegal invocation" in a real browser (jsdom does not catch this).
      (typeof globalThis.fetch === 'function' ? globalThis.fetch.bind(globalThis) : null);
  }

  buildUrl(filters: QueryFilters): string {
    const params = new URLSearchParams();
    const level = optionalString(filters.level ?? undefined);
    if (level !== null) params.set('level', level);
    const component = optionalString(filters.component);
    if (component !== null) params.set('component', component);
    const trace = optionalString(filters.trace);
    if (trace !== null) params.set('trace', trace);
    const q = optionalString(filters.q);
    if (q !== null) params.set('q', q);
    const since = optionalInt(filters.since_ms);
    if (since !== null) params.set('since_ms', String(since));
    const until = optionalInt(filters.until_ms);
    if (until !== null) params.set('until_ms', String(until));
    const after = optionalInt(filters.after);
    if (after !== null && after > 0) params.set('after', String(after));
    params.set('limit', String(clampInt(filters.limit, 1, MAX_PAGE_LIMIT, DEFAULT_PAGE_LIMIT)));
    if (filters.scan_budget !== undefined) {
      params.set(
        'scan_budget',
        String(clampInt(filters.scan_budget, 1, 500_000, DEFAULT_SCAN_BUDGET)),
      );
    }
    return `${this.baseUrl}${this.path}?${params.toString()}`;
  }

  async fetchPage(filters: QueryFilters, signal?: AbortSignal): Promise<QueryPage> {
    if (this.fetchImpl === null) {
      throw new QueryError('fetch is unavailable in this runtime');
    }
    const url = this.buildUrl(filters);
    let response: Response;
    try {
      response = await this.fetchImpl(url, {
        method: 'GET',
        headers: { Accept: 'application/json' },
        cache: 'no-store',
        credentials: 'same-origin',
        signal,
      });
    } catch (error) {
      if (isAbortError(error)) throw error;
      throw new QueryError(error instanceof Error ? error.message : 'query request failed');
    }
    if (!response.ok) {
      throw new QueryError(`query failed: HTTP ${response.status}`, response.status);
    }
    let payload: unknown;
    try {
      payload = await response.json();
    } catch {
      throw new QueryError('query returned malformed JSON');
    }
    if (payload === null || typeof payload !== 'object' || Array.isArray(payload)) {
      throw new QueryError('query returned an unexpected payload');
    }
    const raw = payload as Record<string, unknown>;
    const hits = normalizeLogPayload(raw['hits']);
    const nextAfterRaw = numberOrNull(raw['nextAfter']);
    const nextAfter =
      nextAfterRaw ?? (hits.length > 0 ? hits[hits.length - 1].runtime_sequence : null);
    return {
      schema: numberOrNull(raw['schema']) ?? 1,
      hits,
      count: numberOrNull(raw['count']) ?? hits.length,
      nextAfter,
      hasMore: raw['hasMore'] === true,
      exhausted: raw['exhausted'] === true,
      scanned: numberOrNull(raw['scanned']),
      budgetHit: raw['budgetHit'] === true,
      order: typeof raw['order'] === 'string' ? raw['order'] : 'asc',
      generatedAt: numberOrNull(raw['generatedAt']),
    };
  }

  /**
   * Fetches every matching page from `after` (exclusive) up to `until`
   * (inclusive), bounded by `maxPages`. Used for pause/resume catch-up and to
   * fill a cursor gap. Throws on transport errors; aborts propagate.
   */
  async fetchRange(params: {
    after: number;
    until: number;
    filters?: Omit<QueryFilters, 'after' | 'limit'>;
    pageLimit?: number;
    maxPages?: number;
    signal?: AbortSignal;
  }): Promise<QueryRange> {
    const pageLimit = clampInt(params.pageLimit, 1, MAX_PAGE_LIMIT, 500);
    const maxPages = clampInt(params.maxPages, 1, 20, 6);
    let cursor = Math.max(0, Math.trunc(params.after));
    const until = Math.max(cursor, Math.trunc(params.until));
    const bySeq = new Map<number, LogRecord>();
    let pages = 0;
    let complete = cursor >= until;

    while (!complete && pages < maxPages) {
      const page = await this.fetchPage(
        { ...(params.filters ?? {}), after: cursor, limit: pageLimit },
        params.signal,
      );
      pages += 1;
      for (const record of page.hits) {
        if (record.runtime_sequence > params.after) bySeq.set(record.runtime_sequence, record);
      }
      const next = page.nextAfter ?? cursor;
      if (next <= cursor) break;
      cursor = next;
      if (cursor >= until) {
        complete = true;
        break;
      }
      if (!page.hasMore || page.exhausted || page.hits.length === 0) {
        complete = true;
        break;
      }
    }

    const records = [...bySeq.values()].sort(
      (a, b) => a.runtime_sequence - b.runtime_sequence,
    );
    return { records, complete, nextAfter: cursor, pages };
  }
}

export function isAbortError(error: unknown): boolean {
  return (
    (error instanceof DOMException && error.name === 'AbortError') ||
    (error instanceof Error && error.name === 'AbortError')
  );
}

export interface QuerySessionState {
  rows: readonly LogRecord[];
  loading: boolean;
  error: string | null;
  hasMore: boolean;
  nextAfter: number | null;
  truncated: boolean;
  cleared: boolean;
}

export interface QuerySessionOptions {
  pageLimit?: number;
  maxRows?: number;
}

const DEFAULT_MAX_QUERY_ROWS = 2000;

/**
 * Per-panel query lifecycle: one in-flight request, abort on every filter
 * change, generation guard against stale responses, bounded row cache, no
 * duplicate rows.
 */
export class QuerySession {
  private readonly client: QueryClient;
  private readonly onUpdate: (state: QuerySessionState) => void;
  private readonly pageLimit: number;
  private readonly maxRows: number;
  private controller: AbortController | null = null;
  private generation = 0;
  private filters: QueryFilters = {};
  private created = false;
  private state: QuerySessionState = {
    rows: [],
    loading: false,
    error: null,
    hasMore: false,
    nextAfter: null,
    truncated: false,
    cleared: false,
  };

  constructor(
    client: QueryClient,
    onUpdate: (state: QuerySessionState) => void,
    options: QuerySessionOptions = {},
  ) {
    this.client = client;
    this.onUpdate = onUpdate;
    this.pageLimit = clampInt(options.pageLimit, 1, MAX_PAGE_LIMIT, DEFAULT_PAGE_LIMIT);
    this.maxRows = clampInt(options.maxRows, 50, 20_000, DEFAULT_MAX_QUERY_ROWS);
  }

  /** Starts a new query. Every previous request is aborted immediately. */
  start(filters: QueryFilters): Promise<void> {
    this.abort();
    this.generation += 1;
    this.filters = { ...filters };
    this.created = true;
    this.setState({
      rows: [],
      loading: true,
      error: null,
      hasMore: false,
      nextAfter: null,
      truncated: false,
      cleared: false,
    });
    return this.fetchPage(0, this.generation, 'replace');
  }

  /** Re-runs the current filters from the beginning. */
  retry(): Promise<void> {
    if (!this.created) return Promise.resolve();
    return this.start(this.filters).then(() => undefined);
  }

  /** Aborts an in-flight request without discarding the current rows. */
  cancel(): void {
    this.abort();
    this.generation += 1;
    if (this.state.loading) {
      this.setState({ ...this.state, loading: false });
    }
  }

  loadMore(): Promise<void> {
    if (this.state.loading || !this.state.hasMore || this.state.nextAfter === null) {
      return Promise.resolve();
    }
    return this.fetchPage(this.state.nextAfter, this.generation, 'append');
  }

  /** Clears the displayed result only; the server history is untouched. */
  clearRows(): void {
    this.abort();
    this.generation += 1;
    this.setState({
      ...this.state,
      rows: [],
      loading: false,
      hasMore: false,
      nextAfter: null,
      error: null,
      cleared: true,
    });
  }

  stop(): void {
    this.abort();
    this.generation += 1;
    this.created = false;
    this.setState({
      rows: [],
      loading: false,
      error: null,
      hasMore: false,
      nextAfter: null,
      truncated: false,
      cleared: false,
    });
  }

  dispose(): void {
    this.abort();
    this.generation += 1;
  }

  private async fetchPage(
    after: number,
    generation: number,
    mode: 'replace' | 'append',
  ): Promise<void> {
    const controller = new AbortController();
    this.controller = controller;
    try {
      const page = await this.client.fetchPage(
        { ...this.filters, after, limit: this.pageLimit },
        controller.signal,
      );
      if (generation !== this.generation || controller.signal.aborted) return;
      const merged = mode === 'replace' ? [] : [...this.state.rows];
      const seen = new Set<number>(merged.map((row) => row.runtime_sequence));
      for (const row of page.hits) {
        if (!seen.has(row.runtime_sequence)) {
          seen.add(row.runtime_sequence);
          merged.push(row);
        }
      }
      merged.sort((a, b) => a.runtime_sequence - b.runtime_sequence);
      let truncated = this.state.truncated || page.budgetHit;
      if (merged.length > this.maxRows) {
        merged.splice(0, merged.length - this.maxRows);
        truncated = true;
      }
      this.setState({
        rows: merged,
        loading: false,
        error: null,
        hasMore: page.hasMore,
        nextAfter: page.nextAfter,
        truncated,
        cleared: false,
      });
    } catch (error) {
      if (generation !== this.generation || isAbortError(error)) return;
      this.setState({
        ...this.state,
        loading: false,
        error: error instanceof Error ? error.message : 'query failed',
      });
    } finally {
      if (this.controller === controller) this.controller = null;
    }
  }

  private setState(next: QuerySessionState): void {
    this.state = next;
    this.onUpdate(next);
  }

  private abort(): void {
    if (this.controller !== null) {
      this.controller.abort();
      this.controller = null;
    }
  }
}
