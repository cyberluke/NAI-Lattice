import { isAbortError, type QueryRange } from '../query/client';
import type { LogRecord } from './types';

export interface LogViewFeed {
  readonly ingestedCursor: number;
  readonly oldestSeq: number | null;
  snapshotAfter(afterSeq: number, limit?: number): LogRecord[];
  subscribe(listener: () => void): () => void;
  catchUpRange(params: {
    after: number;
    until: number;
    signal: AbortSignal;
    pageLimit?: number;
    maxPages?: number;
  }): Promise<QueryRange>;
}

export type CatchUpState = 'idle' | 'catching-up' | 'incomplete';

export interface LogViewMeta {
  paused: boolean;
  suspended: boolean;
  catchUp: CatchUpState;
  catchUpError: string | null;
  lastSeq: number;
  /** Records the view had to drop to stay bounded (display only). */
  truncated: number;
}

export interface LogViewOptions {
  capacity?: number;
  onUpdate?: (rows: readonly LogRecord[], meta: LogViewMeta) => void;
}

const DEFAULT_VIEW_CAPACITY = 5000;

/**
 * Per-panel view over a shared feed.
 *
 * - pause freezes the view only; ingestion and the durable cursor continue
 *   into the feed's bounded ring;
 * - resume replays the ring and asks `/lattice/api/query` for the interval
 *   `(last displayed, ingest cursor]`, deduping by runtime_sequence;
 * - clear resets only this view and its display boundary.
 */
export class LogView {
  private readonly feed: LogViewFeed;
  private readonly capacity: number;
  private readonly onUpdate: ((rows: readonly LogRecord[], meta: LogViewMeta) => void) | null;
  private rows: LogRecord[] = [];
  private floor = 0;
  private lastSeq = 0;
  private paused = false;
  private suspended = false;
  private disposed = false;
  private unsubscribe: (() => void) | null = null;
  private catchUpController: AbortController | null = null;
  private catchUp: CatchUpState = 'idle';
  private catchUpError: string | null = null;
  private truncated = 0;

  constructor(feed: LogViewFeed, options: LogViewOptions = {}) {
    this.feed = feed;
    this.capacity = options.capacity ?? DEFAULT_VIEW_CAPACITY;
    this.onUpdate = options.onUpdate ?? null;
  }

  start(): void {
    if (this.disposed || this.unsubscribe !== null) return;
    this.unsubscribe = this.feed.subscribe(() => this.onFeedUpdate());
    this.rows = this.trim(this.feed.snapshotAfter(this.floor, this.capacity));
    this.lastSeq = this.rows.length > 0 ? this.rows[this.rows.length - 1].runtime_sequence : this.floor;
    this.emit();
  }

  pause(): void {
    if (this.disposed || this.paused) return;
    this.paused = true;
    this.abortCatchUp();
    this.emit();
  }

  /** Stops background refresh without showing the paused affordance (query mode). */
  suspend(): void {
    if (this.disposed) return;
    this.suspended = true;
    this.abortCatchUp();
    this.emit();
  }

  /**
   * Re-enters the live view. Replays the ring, then fetches the interval the
   * ring no longer holds from the server query API.
   */
  async resume(): Promise<void> {
    if (this.disposed) return;
    if (!this.paused && !this.suspended) return;
    this.paused = false;
    this.suspended = false;
    this.abortCatchUp();
    this.catchUp = 'catching-up';
    this.catchUpError = null;
    this.emit();

    const until = this.feed.ingestedCursor;
    const from = this.lastSeq;
    const ringRecords = this.feed.snapshotAfter(from);
    this.appendNew(ringRecords);

    let complete = true;
    if (until > from) {
      const controller = new AbortController();
      this.catchUpController = controller;
      try {
        const range = await this.feed.catchUpRange({
          after: from,
          until,
          signal: controller.signal,
          pageLimit: 500,
          maxPages: 10,
        });
        if (this.disposed) return;
        if (!controller.signal.aborted) {
          this.merge(range.records);
          complete = range.complete && range.nextAfter >= until;
        }
      } catch (error) {
        if (this.disposed) return;
        if (!isAbortError(error)) {
          this.catchUp = 'incomplete';
          this.catchUpError = error instanceof Error ? error.message : 'catch-up failed';
        }
        complete = false;
      } finally {
        if (this.catchUpController === controller) this.catchUpController = null;
      }
    }

    // Events that arrived while the request was in flight.
    const tail = this.feed.snapshotAfter(this.lastSeq);
    this.appendNew(tail);
    const stillMissing = this.feed.oldestSeq !== null && this.feed.oldestSeq > this.lastSeq + 1;
    if (!complete || stillMissing) {
      this.catchUp = 'incomplete';
      if (this.catchUpError === null && stillMissing) {
        this.catchUpError = 'the live ring advanced past the displayed cursor';
      }
    } else {
      this.catchUp = 'idle';
    }
    if (this.catchUp === 'idle') this.catchUpError = null;
    this.emit();
  }

  /** Clears this view only; ingestion and persisted history are untouched. */
  clear(): void {
    if (this.disposed) return;
    this.abortCatchUp();
    this.rows = [];
    this.lastSeq = this.feed.ingestedCursor;
    this.floor = this.lastSeq;
    this.catchUp = 'idle';
    this.catchUpError = null;
    this.truncated = 0;
    this.emit();
  }

  dispose(): void {
    this.disposed = true;
    this.abortCatchUp();
    if (this.unsubscribe !== null) {
      this.unsubscribe();
      this.unsubscribe = null;
    }
  }

  private onFeedUpdate(): void {
    if (this.disposed || this.paused || this.suspended) return;
    const fresh = this.feed.snapshotAfter(this.lastSeq);
    if (fresh.length === 0) return;
    this.appendNew(fresh);
    this.emit();
  }

  /** Fast path for monotonically newer records; no map, no full sort. */
  private appendNew(records: readonly LogRecord[]): void {
    if (records.length === 0) return;
    const fresh: LogRecord[] = [];
    let watermark = this.lastSeq;
    for (const row of records) {
      if (row.runtime_sequence <= this.floor || row.runtime_sequence <= watermark) continue;
      watermark = row.runtime_sequence;
      fresh.push(row);
    }
    if (fresh.length === 0) return;
    this.rows = this.trim([...this.rows, ...fresh]);
    this.lastSeq = Math.max(this.lastSeq, watermark);
  }

  /** Merge path for catch-up pages that may overlap the displayed rows. */
  private merge(records: readonly LogRecord[]): void {
    if (records.length === 0) return;
    const bySeq = new Map<number, LogRecord>();
    for (const row of this.rows) bySeq.set(row.runtime_sequence, row);
    for (const row of records) {
      if (row.runtime_sequence <= this.floor) continue;
      bySeq.set(row.runtime_sequence, row);
    }
    const merged = [...bySeq.values()].sort(
      (a, b) => a.runtime_sequence - b.runtime_sequence,
    );
    this.rows = this.trim(merged);
    if (this.rows.length > 0) {
      this.lastSeq = Math.max(this.lastSeq, this.rows[this.rows.length - 1].runtime_sequence);
    }
  }

  private trim(rows: LogRecord[]): LogRecord[] {
    if (rows.length <= this.capacity) return rows;
    const dropped = rows.length - this.capacity;
    this.truncated += dropped;
    return rows.slice(dropped);
  }

  private emit(): void {
    if (this.onUpdate === null) return;
    this.onUpdate([...this.rows], {
      paused: this.paused,
      suspended: this.suspended,
      catchUp: this.catchUp,
      catchUpError: this.catchUpError,
      lastSeq: this.lastSeq,
      truncated: this.truncated,
    });
  }

  private abortCatchUp(): void {
    if (this.catchUpController !== null) {
      this.catchUpController.abort();
      this.catchUpController = null;
    }
    this.catchUp = 'idle';
    this.catchUpError = null;
  }
}
