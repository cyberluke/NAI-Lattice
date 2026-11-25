/**
 * Bounded, non-reactive ring of ingested records.
 *
 * It intentionally lives outside Vue's deep reactive graph: ingest may run at
 * thousands of events per second, while the view reads a bounded snapshot on a
 * render cadence.
 */
export class EventRing<T> {
  private readonly buffer: Array<T | undefined>;
  private start = 0;
  private count = 0;

  constructor(readonly capacity: number) {
    if (!Number.isInteger(capacity) || capacity <= 0) {
      throw new Error(`EventRing capacity must be a positive integer, got ${capacity}`);
    }
    this.buffer = new Array<T | undefined>(capacity);
  }

  get size(): number {
    return this.count;
  }

  get oldest(): T | undefined {
    return this.count === 0 ? undefined : this.buffer[this.start];
  }

  get newest(): T | undefined {
    if (this.count === 0) return undefined;
    return this.buffer[(this.start + this.count - 1) % this.capacity];
  }

  push(value: T): void {
    if (this.count < this.capacity) {
      this.buffer[(this.start + this.count) % this.capacity] = value;
      this.count += 1;
      return;
    }
    this.buffer[this.start] = value;
    this.start = (this.start + 1) % this.capacity;
  }

  clear(): void {
    this.buffer.fill(undefined);
    this.start = 0;
    this.count = 0;
  }

  toArray(): T[] {
    const out: T[] = [];
    for (let i = 0; i < this.count; i += 1) {
      const value = this.buffer[(this.start + i) % this.capacity];
      if (value !== undefined) out.push(value);
    }
    return out;
  }
}
