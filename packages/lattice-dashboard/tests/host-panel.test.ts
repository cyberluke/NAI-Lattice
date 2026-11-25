import { flushPromises, mount } from '@vue/test-utils';
import { afterEach, describe, expect, it, vi } from 'vitest';
import hostPanelSource from '../src/modules/host/HostPanel.vue?raw';
import HostPanel from '../src/modules/host/HostPanel.vue';

const NVIDIA = {
  id: 'nvidia0',
  vendor: 'nvidia',
  source: 'nvidia-smi',
  name: 'NVIDIA GeForce RTX 4090',
  utilizationPct: 5,
  memoryUsedBytes: 2_807_037_952,
  memoryTotalBytes: 24_142_446_592,
  temperatureC: 54,
  detail: null,
};

const INTEL = {
  id: 'intel0',
  vendor: 'intel',
  source: 'dxgi',
  name: 'Intel(R) Graphics',
  utilizationPct: null,
  memoryUsedBytes: null,
  memoryTotalBytes: null,
  temperatureC: null,
  detail: 'adapter present; DXGI does not expose utilization on this host',
};

const NPU = {
  id: 'npu0',
  vendor: 'intel',
  source: 'windows-device',
  name: 'Intel(R) AI Boost',
  status: 'present',
  utilizationPct: null,
  detail: 'NPU presence detected; no utilization sampler is wired',
};

function jsonResponse(payload: unknown, status = 200): Response {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

function payload(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    schema: 1,
    generatedAt: 1_764_000_000_000,
    source: 'nvidia-smi',
    cpu: { utilizationPct: 12.5 },
    memory: { totalBytes: 68_559_708_160, availableBytes: 41_234_567_890 },
    gpu: NVIDIA,
    gpus: [NVIDIA, INTEL],
    npu: NPU,
    ...overrides,
  };
}

function stubPayload(body: Record<string, unknown>): void {
  vi.stubGlobal('fetch', async () => jsonResponse(body));
}

afterEach(() => {
  vi.unstubAllGlobals();
  document.body.innerHTML = '';
});

describe('HostPanel multi-device rendering', () => {
  it('renders one block per GPU and a separate NPU block with honest gaps', async () => {
    stubPayload(payload());
    const wrapper = mount(HostPanel, { attachTo: document.body });
    await flushPromises();

    const nvidia = wrapper.find('[data-device-id="nvidia0"]');
    const intel = wrapper.find('[data-device-id="intel0"]');
    const npu = wrapper.find('.device-block--npu');
    expect(nvidia.exists()).toBe(true);
    expect(intel.exists()).toBe(true);
    expect(npu.exists()).toBe(true);

    expect(nvidia.text()).toContain('NVIDIA GeForce RTX 4090');
    expect(nvidia.text()).toContain('5.0%');
    expect(nvidia.text()).toContain('54 °C');
    expect(nvidia.findAll('.metric-card')).toHaveLength(3);

    expect(intel.text()).toContain('Intel(R) Graphics');
    expect(intel.text()).toContain('not exposed by this host API');
    expect(intel.text()).toContain('DXGI does not expose utilization');
    expect(intel.findAll('.metric-card')).toHaveLength(0);

    expect(npu.text()).toContain('Intel(R) AI Boost');
    expect(npu.text()).toContain('present');
    expect(npu.text()).toContain('NPU utilization is not exposed by this host API');
    expect(npu.text()).toContain('no utilization sampler is wired');

    expect(wrapper.find('.source-chip').text()).toContain('source nvidia-smi');
    expect(wrapper.text()).toContain('2 adapters');
    wrapper.unmount();
  });

  it('renders only the cards whose real measurements exist', async () => {
    stubPayload(
      payload({
        gpus: [
          {
            ...INTEL,
            id: 'partial0',
            name: 'Partial adapter',
            memoryTotalBytes: 8_589_934_592,
            temperatureC: 41,
            detail: null,
          },
        ],
        gpu: null,
        npu: null,
      }),
    );
    const wrapper = mount(HostPanel, { attachTo: document.body });
    await flushPromises();

    const device = wrapper.find('[data-device-id="partial0"]');
    expect(device.exists()).toBe(true);
    const labels = device
      .findAll('.metric-label')
      .map((label) => label.text().toLowerCase());
    expect(labels).toEqual(['memory', 'temperature']);
    expect(device.text()).not.toContain('utilization');
    expect(device.text()).not.toContain('0%');
    expect(device.text()).not.toContain('0 B');
    wrapper.unmount();
  });

  it('falls back to the legacy gpu field when the host sends no device list', async () => {
    stubPayload({
      schema: 1,
      generatedAt: 1_764_000_000_000,
      source: 'nvidia-smi',
      cpu: { utilizationPct: 10 },
      memory: null,
      gpu: NVIDIA,
      npu: null,
    });
    const wrapper = mount(HostPanel, { attachTo: document.body });
    await flushPromises();

    const devices = wrapper.findAll('.device-block');
    expect(devices).toHaveLength(1);
    expect(devices[0].text()).toContain('NVIDIA GeForce RTX 4090');
    expect(devices[0].text()).toContain('nvidia-smi');
    wrapper.unmount();
  });

  it('falls back to a real legacy gpu when the device list is empty', async () => {
    stubPayload(payload({ gpus: [], gpu: NVIDIA, npu: null }));
    const wrapper = mount(HostPanel, { attachTo: document.body });
    await flushPromises();

    const devices = wrapper.findAll('.device-block');
    expect(devices).toHaveLength(1);
    expect(devices[0].text()).toContain('NVIDIA GeForce RTX 4090');
    wrapper.unmount();
  });

  it('never invents values for a device whose metrics are all null', async () => {
    stubPayload(
      payload({ gpus: [{ ...INTEL, detail: null }], gpu: null, npu: null }),
    );
    const wrapper = mount(HostPanel, { attachTo: document.body });
    await flushPromises();

    const intel = wrapper.find('[data-device-id="intel0"]');
    expect(intel.text()).toContain('not exposed by this host API');
    expect(intel.text()).not.toContain('0%');
    expect(intel.text()).not.toContain('0 B');
    expect(wrapper.findAll('.device-block--npu')).toHaveLength(0);
    wrapper.unmount();
  });
});

describe('HostPanel scroll contract', () => {
  it('makes the host column a real vertical scroller above the chart', () => {
    const block = hostPanelSource.slice(
      hostPanelSource.indexOf('.host-scroll {'),
      hostPanelSource.indexOf('}', hostPanelSource.indexOf('.host-scroll {')) + 1,
    );
    expect(block).toContain('min-height: 0');
    expect(block).toContain('overflow-y: auto');
    expect(block).toContain('flex: 1 1 auto');
  });
});
