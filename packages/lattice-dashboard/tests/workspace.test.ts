import { describe, expect, it } from 'vitest';
import type { SerializedDockview } from 'dockview-core';
import {
  WORKSPACE_CORRUPT_KEY,
  WORKSPACE_STORAGE_KEY,
  WORKSPACE_VERSION,
  nextInstanceId,
  validateLayout,
} from '../src/workspace/schema';
import { WorkspaceStore, type StorageLike } from '../src/workspace/store';

class MemoryStorage implements StorageLike {
  private values = new Map<string, string>();

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string): void {
    this.values.set(key, value);
  }

  removeItem(key: string): void {
    this.values.delete(key);
  }

  has(key: string): boolean {
    return this.values.has(key);
  }
}

function layout(moduleType = 'log-tail', instanceId = 'log-tail@1'): SerializedDockview {
  return {
    grid: {
      root: { type: 'branch', data: [] },
      height: 800,
      width: 1400,
      orientation: 'HORIZONTAL',
    },
    panels: {
      [instanceId]: {
        id: instanceId,
        contentComponent: 'module',
        title: 'Log Tail',
        params: { moduleType, instanceId },
      },
    },
  } as unknown as SerializedDockview;
}

describe('workspace persistence', () => {
  it('persists and restores a versioned layout', () => {
    const storage = new MemoryStorage();
    const store = new WorkspaceStore(storage, () => '2026-11-24T20:00:00.000Z');
    expect(store.save(layout())).toBe(true);

    const raw = storage.getItem(WORKSPACE_STORAGE_KEY);
    expect(raw).not.toBeNull();
    const parsed = JSON.parse(raw ?? '{}');
    expect(parsed.version).toBe(WORKSPACE_VERSION);
    expect(parsed.savedAt).toBe('2026-11-24T20:00:00.000Z');

    const restored = new WorkspaceStore(storage).load();
    expect(restored.reason).toBe('ok');
    expect(restored.notice).toBeNull();
    expect(restored.layout?.panels['log-tail@1']).toBeDefined();
  });

  it('resets on a version bump and preserves the corrupt copy', () => {
    const storage = new MemoryStorage();
    storage.setItem(
      WORKSPACE_STORAGE_KEY,
      JSON.stringify({ version: 99, savedAt: 'x', layout: layout() }),
    );
    const store = new WorkspaceStore(storage);
    const result = store.load();
    expect(result.reason).toBe('version-mismatch');
    expect(result.notice).toContain('version 99');
    expect(result.layout).toBeNull();
    expect(storage.has(WORKSPACE_STORAGE_KEY)).toBe(false);
    expect(storage.getItem(WORKSPACE_CORRUPT_KEY)).toContain('"version":99');
  });

  it('resets malformed JSON cleanly and keeps the raw copy', () => {
    const storage = new MemoryStorage();
    storage.setItem(WORKSPACE_STORAGE_KEY, '{not-json');
    const store = new WorkspaceStore(storage);
    const result = store.load();
    expect(result.reason).toBe('malformed-json');
    expect(result.layout).toBeNull();
    expect(result.notice).not.toBeNull();
    expect(storage.has(WORKSPACE_STORAGE_KEY)).toBe(false);
    expect(storage.getItem(WORKSPACE_CORRUPT_KEY)).toBe('{not-json');
  });

  it('rejects layouts that reference an unknown module type', () => {
    const reason = validateLayout(layout('made-up', 'made-up@1'));
    expect(reason).toBe('unknown-module');
  });

  it('allocates unique instance ids per module type', () => {
    expect(nextInstanceId('log-tail', [])).toBe('log-tail@2');
    expect(nextInstanceId('log-tail', ['log-tail@2', 'log-tail@3'])).toBe('log-tail@4');
    expect(nextInstanceId('health', ['log-tail@2'])).toBe('health@2');
  });
});
