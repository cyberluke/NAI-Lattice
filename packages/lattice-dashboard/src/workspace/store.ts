import type { SerializedDockview } from 'dockview-core';
import {
  WORKSPACE_CORRUPT_KEY,
  WORKSPACE_STORAGE_KEY,
  decodeWorkspace,
  encodeWorkspace,
  type WorkspaceLoadResult,
} from './schema';

export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

export function defaultStorage(): StorageLike | null {
  try {
    if (typeof window === 'undefined' || window.localStorage === undefined) return null;
    return window.localStorage;
  } catch {
    return null;
  }
}

/**
 * Versioned workspace persistence with debounced saves, validation on load and
 * a preserved corrupt copy for diagnosis.
 */
export class WorkspaceStore {
  private readonly storage: StorageLike | null;
  private readonly now: () => string;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private pending: SerializedDockview | null = null;

  constructor(storage: StorageLike | null = defaultStorage(), now: () => string = () => new Date().toISOString()) {
    this.storage = storage;
    this.now = now;
  }

  load(): WorkspaceLoadResult {
    if (this.storage === null) {
      return { layout: null, reason: 'missing', notice: null, corruptRaw: null };
    }
    let raw: string | null = null;
    try {
      raw = this.storage.getItem(WORKSPACE_STORAGE_KEY);
    } catch {
      return { layout: null, reason: 'missing', notice: null, corruptRaw: null };
    }
    const result = decodeWorkspace(raw);
    if (result.corruptRaw !== null) {
      try {
        this.storage.setItem(WORKSPACE_CORRUPT_KEY, result.corruptRaw);
      } catch {
        // Quota or privacy mode: keep the in-memory notice, drop the archive.
      }
      try {
        this.storage.removeItem(WORKSPACE_STORAGE_KEY);
      } catch {
        // Ignore storage failures; the shell still resets cleanly.
      }
    }
    return result;
  }

  save(layout: SerializedDockview): boolean {
    if (this.storage === null) return false;
    try {
      this.storage.setItem(WORKSPACE_STORAGE_KEY, encodeWorkspace(layout, this.now()));
      return true;
    } catch {
      return false;
    }
  }

  scheduleSave(layout: SerializedDockview, delayMs = 400): void {
    this.pending = layout;
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = setTimeout(() => {
      this.timer = null;
      const pending = this.pending;
      this.pending = null;
      if (pending !== null) this.save(pending);
    }, delayMs);
  }

  /** Flushes a pending debounced layout immediately (call on teardown). */
  flush(): void {
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    const pending = this.pending;
    this.pending = null;
    if (pending !== null) this.save(pending);
  }

  clear(): void {
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    this.pending = null;
    if (this.storage === null) return;
    try {
      this.storage.removeItem(WORKSPACE_STORAGE_KEY);
    } catch {
      // Ignore.
    }
  }

  readCorrupt(): string | null {
    if (this.storage === null) return null;
    try {
      return this.storage.getItem(WORKSPACE_CORRUPT_KEY);
    } catch {
      return null;
    }
  }
}
