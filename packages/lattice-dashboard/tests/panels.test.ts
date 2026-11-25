import { flushPromises, mount } from '@vue/test-utils';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { APP_CONTEXT_KEY } from '../src/app-context';
import HostPanel from '../src/modules/host/HostPanel.vue';
import TopologyPanel from '../src/modules/topology/TopologyPanel.vue';
import { InvestigationStore } from '../src/state/investigation';

function jsonResponse(payload: unknown, status = 200): Response {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
  document.body.innerHTML = '';
});

describe('host panel honest states', () => {
  it('says host metrics are not wired on a 404', async () => {
    vi.stubGlobal('fetch', async () => jsonResponse({}, 404));
    const wrapper = mount(HostPanel, { attachTo: document.body });
    await flushPromises();
    expect(wrapper.text()).toContain('host metrics not wired');
    expect(wrapper.text()).toContain('No CPU, memory');
    wrapper.unmount();
  });

  it('renders gpu:null as "no NVIDIA sampler" with a neutral source none', async () => {
    vi.stubGlobal('fetch', async () =>
      jsonResponse({
        schema: 1,
        generatedAt: 1_764_000_000_000,
        source: 'none',
        cpu: { utilizationPct: null },
        memory: null,
        gpu: null,
      }),
    );
    const wrapper = mount(HostPanel, { attachTo: document.body });
    await flushPromises();
    expect(wrapper.text()).toContain('no NVIDIA sampler on this host');
    expect(wrapper.text()).toContain('source none');
    expect(wrapper.find('.source-chip').classes()).toContain('tone--muted');
    expect(wrapper.text()).toContain('GPU utilization, memory and temperature stay unknown');
    wrapper.unmount();
  });
});

describe('topology panel honest states', () => {
  it('renders the exact empty state for a window without components', async () => {
    vi.stubGlobal('fetch', async () =>
      jsonResponse({
        schema: 1,
        generatedAt: 1_764_000_000_000,
        window: 4000,
        truncated: false,
        nodes: [],
        edges: [],
      }),
    );
    const investigation = new InvestigationStore();
    const wrapper = mount(TopologyPanel, {
      attachTo: document.body,
      global: {
        provide: { [APP_CONTEXT_KEY]: { investigation } },
      },
    });
    await flushPromises();
    expect(wrapper.text()).toContain('no component-bearing traces in the window');
    expect(wrapper.text()).toContain('window 4,000');
    expect(wrapper.text()).not.toContain('truncated at the host cap');
    wrapper.unmount();
  });
});
