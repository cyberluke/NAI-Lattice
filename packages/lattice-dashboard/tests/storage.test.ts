import { flushPromises, mount } from '@vue/test-utils';
import { shallowRef } from 'vue';
import { describe, expect, it } from 'vitest';
import { APP_CONTEXT_KEY, type LatticeAppContext } from '../src/app-context';
import storageSource from '../src/modules/storage/StoragePanel.vue?raw';
import StoragePanel from '../src/modules/storage/StoragePanel.vue';
import { parseSummary, type LatticeSummary, type StorageInfo } from '../src/telemetry/summary';

const FULL_STORAGE = {
  schema: 1,
  root: 'C:\\Users\\me\\AppData\\Local\\NAI\\Lattice\\v1',
  base: 'C:\\Users\\me\\AppData\\Local\\NAI\\Lattice',
  budgetBytes: 1_048_576,
  usedBytes: 4_500,
  freeBytes: 901_234_567_890,
  reserveBytes: 33_700_000_000,
  pressure: 'normal',
  classes: {
    ledger: 100,
    cas: 200,
    hot: 300,
    search: 400,
    vector: 500,
    readModels: 600,
    logs: 700,
    backups: 800,
    audit: 900,
  },
  retention: { logsDays: 30, auditDays: 90, backupDays: 14, runtimeKeep: 2 },
  lastPrune: { runUnixMs: 1_764_000_000_000, bytesFreed: 12_345 },
};

function summaryPayload(storage?: unknown): Record<string, unknown> {
  const payload: Record<string, unknown> = {
    schema: 1,
    generatedAt: 1_764_000_000_000,
    window: 10,
    levels: { info: 10 },
    errors: 0,
    components: { test: 10 },
    distinctTraceIds: 1,
    stream: null,
    health: null,
    version: null,
    features: [],
  };
  if (storage !== undefined) payload['storage'] = storage;
  return payload;
}

function contextWith(summary: LatticeSummary): LatticeAppContext {
  return {
    summary: {
      summary: shallowRef(summary),
      error: shallowRef(null),
      stale: shallowRef(false),
      loading: shallowRef(false),
      lastFetchedAt: shallowRef(1_764_000_000_000),
      refresh: async () => undefined,
    },
  } as unknown as LatticeAppContext;
}

function mountPanel(summary: LatticeSummary) {
  return mount(StoragePanel, {
    props: { instanceId: 'storage@test' },
    global: { provide: { [APP_CONTEXT_KEY]: contextWith(summary) } },
  });
}

describe('summary.storage shaping', () => {
  it('normalises the full contract payload without inventing fields', () => {
    const summary = parseSummary(summaryPayload(FULL_STORAGE));
    const storage = summary.storage as StorageInfo;
    expect(storage).not.toBeNull();
    expect(storage.pressure).toBe('normal');
    expect(storage.budgetBytes).toBe(1_048_576);
    expect(storage.usedBytes).toBe(4_500);
    expect(storage.classes?.readModels).toBe(600);
    expect(storage.classes?.audit).toBe(900);
    expect(storage.retention).toEqual({
      logsDays: 30,
      auditDays: 90,
      backupDays: 14,
      runtimeKeep: 2,
    });
    expect(storage.lastPrune).toEqual({ runUnixMs: 1_764_000_000_000, bytesFreed: 12_345 });
  });

  it('keeps storage null when the host does not expose it', () => {
    const summary = parseSummary(summaryPayload());
    expect(summary.storage).toBeNull();
  });

  it('maps an unknown pressure string to the neutral unknown state', () => {
    const summary = parseSummary(
      summaryPayload({ ...FULL_STORAGE, pressure: 'not-a-pressure' }),
    );
    expect(summary.storage?.pressure).toBe('unknown');
  });
});

describe('StoragePanel', () => {
  it('renders budget, class breakdown, retention and last prune from the payload', async () => {
    const wrapper = mountPanel(parseSummary(summaryPayload(FULL_STORAGE)));
    await flushPromises();

    expect(wrapper.find('.pressure--normal').exists()).toBe(true);
    expect(wrapper.text()).toContain('normal');
    expect(wrapper.text()).toContain('4.39 KiB');
    expect(wrapper.text()).toContain('1.00 MiB');
    expect(wrapper.text()).toContain('read models');
    expect(wrapper.text()).toContain('100 B');
    expect(wrapper.text()).toContain('20%');
    expect(wrapper.text()).toContain('30 d');
    expect(wrapper.text()).toContain('90 d');
    expect(wrapper.text()).toContain('14 d');
    expect(wrapper.text()).toContain('2 versions');
    expect(wrapper.text()).toContain('12.1 KiB');
    expect(wrapper.findAll('.class-table tbody tr')).toHaveLength(9);
    wrapper.unmount();
  });

  it('states honestly that storage is unwired instead of showing zeros', async () => {
    const wrapper = mountPanel(parseSummary(summaryPayload()));
    await flushPromises();

    expect(wrapper.find('.storage-unwired').exists()).toBe(true);
    expect(wrapper.text()).toContain('storage not wired by this host');
    expect(wrapper.text()).not.toContain('0 B');
    expect(wrapper.find('.class-table').exists()).toBe(false);
    wrapper.unmount();
  });

  it('renders unknown pressure with a neutral tone', async () => {
    const wrapper = mountPanel(
      parseSummary(summaryPayload({ ...FULL_STORAGE, pressure: 'unknown' })),
    );
    await flushPromises();

    expect(wrapper.find('.pressure--unknown').exists()).toBe(true);
    expect(wrapper.find('.pressure--normal').exists()).toBe(false);
    expect(wrapper.text()).toContain('unknown');
    wrapper.unmount();
  });

  it('keeps RETENTION and the last prune inside the scrollable column', async () => {
    const wrapper = mountPanel(parseSummary(summaryPayload(FULL_STORAGE)));
    await flushPromises();

    const scroll = wrapper.find('.storage-scroll');
    expect(scroll.exists()).toBe(true);
    const headings = scroll.findAll('h2').map((heading) => heading.text().toLowerCase());
    expect(headings).toEqual(['budget', 'classes', 'retention', 'last prune']);
    expect(scroll.text()).toContain('30 d');
    expect(scroll.text()).toContain('2 versions');
    expect(scroll.text()).toContain('12.1 KiB');

    // jsdom has no layout; model a clipped panel and prove the component puts
    // the retention rows in the element that owns the scroll.
    const element = scroll.element as HTMLElement;
    Object.defineProperty(element, 'scrollHeight', { value: 900, configurable: true });
    Object.defineProperty(element, 'clientHeight', { value: 240, configurable: true });
    element.scrollTop = element.scrollHeight;
    expect(element.scrollHeight).toBeGreaterThan(element.clientHeight);
    expect(element.scrollTop).toBe(900);
    wrapper.unmount();
  });

  it('declares a bounded vertical scroller in the shared hover-expanding pane', () => {
    const start = storageSource.indexOf('.storage-scroll {');
    expect(start).toBeGreaterThan(-1);
    const block = storageSource.slice(start, storageSource.indexOf('}', start) + 1);
    expect(block).toContain('flex: 1');
    expect(block).toContain('min-height: 0');
    expect(block).toContain('overflow-y: auto');
    expect(block).toContain('padding-bottom');
    expect(block).toContain('scrollbar-gutter: stable');
    // The shared .nai-scroll pane supplies the visible 12 px / 24 px hover bar.
    expect(storageSource).toContain('storage-scroll nai-scroll');
  });
});
