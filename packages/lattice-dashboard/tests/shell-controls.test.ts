import { mount } from '@vue/test-utils';
import { shallowRef } from 'vue';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { APP_CONTEXT_KEY, type LatticeAppContext } from '../src/app-context';
import ShellChrome from '../src/shell/ShellChrome.vue';

function shellContext(): LatticeAppContext {
  return {
    summary: {
      summary: shallowRef({
        schema: 1,
        generatedAt: 1_764_000_000_000,
        window: 120,
        levels: { info: 118, warn: 1, error: 1 },
        errors: 2,
        components: { 'nai-os.observability': 118, 'nai-pilot': 2 },
        distinctTraceIds: 3,
        stream: {
          activeSseClients: 1,
          maxSseClients: 24,
          pushTotal: 40,
          subscribers: 1,
          brokerScansTotal: 40,
          lastScanUnixMs: 1_764_000_000_000,
          droppedTotal: 0,
        },
        health: {
          state: 'healthy',
          detail: null,
          lastError: null,
          sinceUnixMs: 1_764_000_000_000,
          uptimeMs: 90_000,
        },
        version: null,
        features: [],
        storage: null,
      }),
      error: shallowRef(null),
      stale: shallowRef(false),
      loading: shallowRef(false),
      lastFetchedAt: shallowRef(1_764_000_000_000),
      refresh: async () => undefined,
    },
    feed: {
      connection: shallowRef('live'),
      cursor: shallowRef(40),
      lastEventAt: shallowRef(1_764_000_000_000),
    },
    interop: {
      status: shallowRef('unavailable'),
      detail: shallowRef('No desktop agent'),
      agentName: shallowRef(null),
      channel: shallowRef(null),
      channelMode: shallowRef('none'),
    },
  } as unknown as LatticeAppContext;
}

function mountShell() {
  return mount(ShellChrome, {
    global: { provide: { [APP_CONTEXT_KEY]: shellContext() } },
  });
}

function buttonByText(wrapper: ReturnType<typeof mountShell>, label: string) {
  return wrapper.findAll('button').find((candidate) => candidate.text() === label);
}

beforeEach(() => {
  window.history.replaceState({}, '', '/lattice/');
});

afterEach(() => {
  window.history.replaceState({}, '', '/');
});

describe('ShellChrome workspace controls', () => {
  it('keeps all six KPI cards and the five header actions', () => {
    const wrapper = mountShell();
    expect(wrapper.findAll('.kpi')).toHaveLength(6);
    expect(wrapper.findAll('.workspace-actions button')).toHaveLength(5);
    expect(buttonByText(wrapper, 'RESET')).toBeDefined();
    expect(buttonByText(wrapper, 'FLOAT')).toBeDefined();
    expect(buttonByText(wrapper, 'POP')).toBeDefined();
    expect(buttonByText(wrapper, 'MAX')).toBeDefined();
    expect(buttonByText(wrapper, 'DUP')).toBeDefined();
  });

  it('requires a second, explicit confirmation before resetting the layout', async () => {
    const wrapper = mountShell();
    const reset = buttonByText(wrapper, 'RESET');
    expect(reset?.attributes('aria-label')).toBe('Reset workspace layout');

    await reset?.trigger('click');
    expect(wrapper.emitted('action')).toBeUndefined();
    expect(wrapper.text()).toContain('Discard layout?');
    const confirm = buttonByText(wrapper, 'CONFIRM');
    const cancel = buttonByText(wrapper, 'CANCEL');
    expect(confirm?.attributes('aria-label')).toBe('Confirm reset workspace layout');
    expect(cancel?.attributes('aria-label')).toBe('Cancel workspace reset');
    expect(wrapper.find('[role="alert"]').text()).toContain('Discard layout?');

    await confirm?.trigger('click');
    expect(wrapper.emitted('action')).toEqual([['reset']]);
    expect(wrapper.text()).not.toContain('Discard layout?');
    expect(buttonByText(wrapper, 'RESET')).toBeDefined();
  });

  it('cancels a reset without emitting anything', async () => {
    const wrapper = mountShell();
    await buttonByText(wrapper, 'RESET')?.trigger('click');
    await buttonByText(wrapper, 'CANCEL')?.trigger('click');

    expect(wrapper.emitted('action')).toBeUndefined();
    expect(wrapper.text()).not.toContain('Discard layout?');
    expect(buttonByText(wrapper, 'RESET')).toBeDefined();
  });

  it('keeps FLOAT/POP/MAX/DUP one-step and clears a pending reset', async () => {
    const wrapper = mountShell();
    await buttonByText(wrapper, 'RESET')?.trigger('click');
    await buttonByText(wrapper, 'FLOAT')?.trigger('click');

    expect(wrapper.text()).not.toContain('Discard layout?');
    expect(wrapper.emitted('action')).toEqual([['float']]);
  });

  it('hides POP in embed mode because NAI OS owns native windows', () => {
    window.history.replaceState({}, '', '/lattice/?embed=1');
    const wrapper = mountShell();

    expect(buttonByText(wrapper, 'POP')).toBeUndefined();
    expect(wrapper.text()).not.toContain('POP');
    expect(buttonByText(wrapper, 'RESET')).toBeDefined();
    expect(buttonByText(wrapper, 'FLOAT')).toBeDefined();
    expect(buttonByText(wrapper, 'MAX')).toBeDefined();
    expect(buttonByText(wrapper, 'DUP')).toBeDefined();
  });
});
