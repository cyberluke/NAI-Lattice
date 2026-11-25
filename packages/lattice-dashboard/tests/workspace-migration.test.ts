import { describe, expect, it } from 'vitest';
import type { SerializedDockview } from 'dockview-core';
import {
  MODULE_TITLES,
  MODULE_TYPES,
  WORKSPACE_CORRUPT_KEY,
  WORKSPACE_STORAGE_KEY,
  WORKSPACE_VERSION,
  decodeWorkspace,
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

function layout(
  moduleType: string = 'overview',
  instanceId = `${moduleType}@1`,
): SerializedDockview {
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
        title: 'Module',
        params: { moduleType, instanceId },
      },
    },
  } as unknown as SerializedDockview;
}

describe('workspace schema v4 migration', () => {
  it('registers Overview, Error Intelligence and Discover as known modules', () => {
    expect(WORKSPACE_VERSION).toBe(4);
    expect(MODULE_TYPES).toContain('overview');
    expect(MODULE_TYPES).toContain('errors');
    expect(MODULE_TYPES).toContain('discover');
    expect(MODULE_TITLES.overview).toBe('Overview');
    expect(MODULE_TITLES.errors).toBe('Error Intelligence');
    expect(MODULE_TITLES.discover).toBe('Discover');
    expect(MODULE_TITLES.host).toBe('Host & GPU');
    expect(MODULE_TITLES.topology).toBe('Topology');
    expect(MODULE_TITLES['execution-chain']).toBe('Execution Chain');
    expect(validateLayout(layout('host'))).toBeNull();
    expect(validateLayout(layout('topology'))).toBeNull();
    expect(validateLayout(layout('overview'))).toBeNull();
    expect(validateLayout(layout('errors'))).toBeNull();
    expect(validateLayout(layout('discover'))).toBeNull();
    // Execution Chain is additive inside v4: no schema bump, old layouts stay valid.
    expect(WORKSPACE_VERSION).toBe(4);
    expect(validateLayout(layout('execution-chain'))).toBeNull();
  });

  it('resets a v1 workspace with a visible migration notice and archives the raw layout', () => {
    const storage = new MemoryStorage();
    storage.setItem(
      WORKSPACE_STORAGE_KEY,
      JSON.stringify({ version: 1, savedAt: '2026-11-24T20:00:00.000Z', layout: layout('log-tail') }),
    );
    const store = new WorkspaceStore(storage);
    const result = store.load();

    expect(result.reason).toBe('version-mismatch');
    expect(result.layout).toBeNull();
    expect(result.notice).toContain('version 1');
    expect(result.notice).toContain(`version ${WORKSPACE_VERSION}`);
    expect(result.notice).toContain('migrated');
    expect(storage.has(WORKSPACE_STORAGE_KEY)).toBe(false);
    expect(storage.getItem(WORKSPACE_CORRUPT_KEY)).toContain('"version":1');
  });

  it('persists the new schema version and restores it without a notice', () => {
    const storage = new MemoryStorage();
    const store = new WorkspaceStore(storage, () => '2026-11-24T20:00:00.000Z');
    expect(store.save(layout('overview'))).toBe(true);
    const raw = storage.getItem(WORKSPACE_STORAGE_KEY) ?? '';
    expect(raw).toContain(`"version":${WORKSPACE_VERSION}`);

    const restored = new WorkspaceStore(storage).load();
    expect(restored.reason).toBe('ok');
    expect(restored.notice).toBeNull();
    expect(restored.layout?.panels['overview@1']).toBeDefined();
  });

  it('keeps a serialized Dockview popout group across save/restore', () => {
    const layoutWithPopout = {
      ...layout('host'),
      popoutGroups: [
        {
          data: { id: 'popout-group', views: ['host@1'], activeView: 'host@1' },
          position: { left: 20, top: 20, width: 640, height: 420 },
        },
      ],
    } as unknown as SerializedDockview;
    const result = decodeWorkspace(
      JSON.stringify({
        version: WORKSPACE_VERSION,
        savedAt: '2026-11-24T20:00:00.000Z',
        layout: layoutWithPopout,
      }),
    );
    expect(result.reason).toBe('ok');
    expect(result.notice).toBeNull();
    const restored = result.layout as unknown as { popoutGroups: unknown[]; panels: Record<string, unknown> };
    expect(restored.popoutGroups).toHaveLength(1);
    expect(restored.panels['host@1']).toBeDefined();
  });
});
