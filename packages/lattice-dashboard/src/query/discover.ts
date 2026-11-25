import type { QueryClient, QueryFilters, QueryPage } from './client';
import type { LogLevel, LogRecord } from '../telemetry/types';
import { isAbortError } from './client';

export const LEVEL_ORDER: readonly LogLevel[] = ['trace', 'debug', 'info', 'warn', 'error'];

export interface FacetFilters {
  /** Empty means "all levels". Values are exact canonical levels. */
  levels: readonly string[];
  /** Empty means "all components". Values are exact component names. */
  components: readonly string[];
}

export function canonicalFacetLevel(level: string): string {
  switch (level.trim().toLowerCase()) {
    case 'warning':
      return 'warn';
    case 'fatal':
      return 'error';
    default:
      return level.trim().toLowerCase();
  }
}

/** The server accepts a minimum level, so the lowest selected one is the filter. */
export function serverLevelFor(levels: readonly string[]): LogLevel | null {
  if (levels.length === 0) return null;
  const selected = new Set(levels.map((level) => canonicalFacetLevel(level)));
  for (const level of LEVEL_ORDER) {
    if (selected.has(level)) return level;
  }
  return null;
}

export function recordMatchesFacets(record: LogRecord, filters: FacetFilters): boolean {
  if (filters.levels.length > 0) {
    const level = canonicalFacetLevel(record.level);
    if (!filters.levels.some((candidate) => canonicalFacetLevel(candidate) === level)) {
      return false;
    }
  }
  if (filters.components.length > 0) {
    if (!filters.components.includes(record.component)) return false;
  }
  return true;
}

/**
 * Server parameters for one page. A single selected component can be pushed to
 * the server; an OR of components cannot, so it is applied client-side over the
 * bounded page stream. The server level is the minimum selected level.
 */
export function buildDiscoverServerFilters(
  filters: FacetFilters,
  after: number,
  limit: number,
): QueryFilters {
  const level = serverLevelFor(filters.levels);
  const component = filters.components.length === 1 ? filters.components[0] : undefined;
  return {
    ...(level !== null ? { level } : {}),
    ...(component !== undefined ? { component } : {}),
    after,
    limit,
  };
}

/** Union of summary-declared and result-observed components, with real counts. */
export function mergeFacetComponents(
  summaryCounts: Readonly<Record<string, number>>,
  rows: readonly LogRecord[],
): Array<{ name: string; loaded: number; summary: number | null }> {
  const loaded = new Map<string, number>();
  for (const row of rows) {
    if (row.component.length === 0) continue;
    loaded.set(row.component, (loaded.get(row.component) ?? 0) + 1);
  }
  const names = new Set<string>([...Object.keys(summaryCounts), ...loaded.keys()]);
  return [...names]
    .map((name) => ({
      name,
      loaded: loaded.get(name) ?? 0,
      summary: Object.prototype.hasOwnProperty.call(summaryCounts, name)
        ? summaryCounts[name]
        : null,
    }))
    .sort(
      (a, b) =>
        b.loaded - a.loaded ||
        (b.summary ?? -1) - (a.summary ?? -1) ||
        a.name.localeCompare(b.name),
    );
}

export interface DiscoverState {
  rows: readonly LogRecord[];
  loading: boolean;
  error: string | null;
  hasMore: boolean;
  nextAfter: number | null;
  scanned: number;
  pages: number;
  /** Exclusive cursor the whole session started from. */
  scanStart: number;
  /** Last cursor the session examined/delivered; null before any page. */
  scanEnd: number | null;
  truncated: boolean;
  exhausted: boolean;
  cleared: boolean;
}

export interface DiscoverSessionOptions {
  pageLimit?: number;
  maxRows?: number;
  maxPagesPerLoad?: number;
}

const EMPTY_STATE: DiscoverState = {
  rows: [],
  loading: false,
  error: null,
  hasMore: false,
  nextAfter: null,
  scanned: 0,
  pages: 0,
  scanStart: 0,
  scanEnd: null,
  truncated: false,
  exhausted: false,
  cleared: false,
};

function clampInt(value: number | undefined, min: number, max: number, fallback: number): number {
  if (value === undefined || !Number.isFinite(value)) return fallback;
  return Math.min(max, Math.max(min, Math.trunc(value)));
}

/**
 * Facet-filtered, keyset-paged discover session.
 *
 * Server filtering uses the minimum level and, when exactly one component is
 * selected, that component. The exact multi-select predicate is applied to each
 * delivered page. A load continues across pages until it has matched
 * `pageLimit` rows, the ledger ends, or the per-load page cap is reached, so a
 * sparse facet selection still makes visible progress.
 */
export class DiscoverSession {
  private readonly client: Pick<QueryClient, 'fetchPage'>;
  private readonly onUpdate: (state: DiscoverState) => void;
  private readonly pageLimit: number;
  private readonly maxRows: number;
  private readonly maxPagesPerLoad: number;
  private controller: AbortController | null = null;
  private generation = 0;
  private filters: FacetFilters = { levels: [], components: [] };
  private created = false;
  private state: DiscoverState = EMPTY_STATE;

  constructor(
    client: Pick<QueryClient, 'fetchPage'>,
    onUpdate: (state: DiscoverState) => void,
    options: DiscoverSessionOptions = {},
  ) {
    this.client = client;
    this.onUpdate = onUpdate;
    this.pageLimit = clampInt(options.pageLimit, 1, 1000, 500);
    this.maxRows = clampInt(options.maxRows, 100, 20_000, 10_000);
    this.maxPagesPerLoad = clampInt(options.maxPagesPerLoad, 1, 20, 8);
  }

  start(filters: FacetFilters): Promise<void> {
    this.abort();
    this.generation += 1;
    this.filters = { levels: [...filters.levels], components: [...filters.components] };
    this.created = true;
    this.setState({ ...EMPTY_STATE, loading: true });
    return this.loadPages(0, this.generation, 'replace');
  }

  retry(): Promise<void> {
    if (!this.created) return Promise.resolve();
    return this.start(this.filters);
  }

  loadMore(): Promise<void> {
    if (this.state.loading || !this.state.hasMore || this.state.nextAfter === null) {
      return Promise.resolve();
    }
    return this.loadPages(this.state.nextAfter, this.generation, 'append');
  }

  clear(): void {
    this.abort();
    this.generation += 1;
    this.setState({ ...EMPTY_STATE, cleared: true });
  }

  stop(): void {
    this.abort();
    this.generation += 1;
    this.created = false;
    this.setState(EMPTY_STATE);
  }

  dispose(): void {
    this.abort();
    this.generation += 1;
  }

  private async loadPages(
    after: number,
    generation: number,
    mode: 'replace' | 'append',
  ): Promise<void> {
    const controller = new AbortController();
    this.controller = controller;
    if (mode === 'append') {
      this.setState({ ...this.state, loading: true, error: null });
    }
    const seen = new Set<number>(
      mode === 'replace' ? [] : this.state.rows.map((row) => row.runtime_sequence),
    );
    const merged = mode === 'replace' ? [] : [...this.state.rows];
    let cursor = after;
    let pagesThisLoad = 0;
    let collected = 0;
    let scanned = mode === 'replace' ? 0 : this.state.scanned;
    const scanStart = mode === 'replace' ? after : this.state.scanStart;
    let scanEnd = mode === 'replace' ? (after > 0 ? after : null) : this.state.scanEnd;
    let exhausted = false;
    let truncated = mode === 'replace' ? false : this.state.truncated;
    let hasMore = false;

    try {
      while (pagesThisLoad < this.maxPagesPerLoad) {
        const page: QueryPage = await this.client.fetchPage(
          buildDiscoverServerFilters(this.filters, cursor, this.pageLimit),
          controller.signal,
        );
        if (generation !== this.generation || controller.signal.aborted) return;
        pagesThisLoad += 1;
        scanned += page.scanned ?? 0;
        for (const hit of page.hits) {
          if (seen.has(hit.runtime_sequence)) continue;
          seen.add(hit.runtime_sequence);
          if (!recordMatchesFacets(hit, this.filters)) continue;
          merged.push(hit);
          collected += 1;
          if (merged.length >= this.maxRows) {
            truncated = true;
            break;
          }
        }
        merged.sort((a, b) => a.runtime_sequence - b.runtime_sequence);
        if (page.exhausted) {
          exhausted = true;
          hasMore = false;
          scanEnd = page.nextAfter ?? cursor;
          break;
        }
        const next = page.nextAfter ?? cursor;
        if (next <= cursor) {
          hasMore = false;
          scanEnd = next;
          break;
        }
        cursor = next;
        scanEnd = cursor;
        hasMore = true;
        if (merged.length >= this.maxRows) break;
        if (collected >= this.pageLimit) break;
        if (!page.hasMore) {
          hasMore = false;
          break;
        }
      }

      const state: DiscoverState = {
        rows: merged,
        loading: false,
        error: null,
        hasMore,
        nextAfter: hasMore ? cursor : null,
        scanned,
        pages: (mode === 'replace' ? 0 : this.state.pages) + pagesThisLoad,
        scanStart,
        scanEnd,
        truncated,
        exhausted,
        cleared: false,
      };
      this.setState(state);
    } catch (error) {
      if (generation !== this.generation || isAbortError(error)) return;
      this.setState({
        ...this.state,
        loading: false,
        error: error instanceof Error ? error.message : 'discover query failed',
      });
    } finally {
      if (this.controller === controller) this.controller = null;
    }
  }

  private setState(next: DiscoverState): void {
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
