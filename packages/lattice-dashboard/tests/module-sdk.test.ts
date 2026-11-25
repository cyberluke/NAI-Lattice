import { flushPromises } from '@vue/test-utils';
import { afterEach, describe, expect, it } from 'vitest';
import type { Channel, Context, DesktopAgent, Listener } from '@finos/fdc3';
import { Fdc3Adapter } from '../src/interop/adapter';
import {
  MODULE_SDK_VERSION,
  installModuleSdk,
  uninstallModuleSdk,
  type NaiLatticeSdkV1,
} from '../src/sdk/module-sdk';
import { InvestigationStore } from '../src/state/investigation';

function listener(): Listener {
  return { unsubscribe: () => {} } as unknown as Listener;
}

class FakeChannel {
  readonly id = 'nai.lattice';
  broadcasts: Context[] = [];

  async broadcast(context: Context): Promise<void> {
    this.broadcasts.push(context);
  }

  async addContextListener(): Promise<Listener> {
    return listener();
  }
}

class FakeAgent {
  readonly channel = new FakeChannel();

  async getOrCreateChannel(): Promise<Channel> {
    return this.channel as unknown as Channel;
  }

  async addContextListener(): Promise<Listener> {
    return listener();
  }

  async addIntentListener(): Promise<Listener> {
    return listener();
  }

  async getInfo(): Promise<{ name: string }> {
    return { name: 'SDK Agent' };
  }
}

function validSelection(): Record<string, unknown> {
  return {
    type: 'nai.lattice.logSelection',
    schema_version: 1,
    id: { runtime_sequence: '42', trace_id: 'trace-sdk' },
    sequence: 42,
    level: 'warn',
    component: 'nai-os.agent',
    timestamp_utc_ms: 1_764_000_000_000,
  };
}

afterEach(() => {
  uninstallModuleSdk();
});

describe('module SDK v1', () => {
  it('installs window.naiLattice wired to the real store and adapter', async () => {
    const fake = new FakeAgent();
    const interop = new Fdc3Adapter({ agent: fake as unknown as DesktopAgent });
    await interop.connect();
    const investigation = new InvestigationStore();
    const sdk = installModuleSdk({ investigation, interop });

    expect(MODULE_SDK_VERSION).toBe(1);
    expect(window.naiLattice).toBe(sdk);
    expect(sdk.version).toBe(1);
    expect(interop.status.value).toBe('ready');
    expect(interop.lastError.value).toBeNull();
    expect(sdk.channel).toBe('nai.lattice');

    sdk.focusTrace('  trace-sdk  ');
    expect(investigation.trace.value).toBe('trace-sdk');
    expect(investigation.panelRequest.value?.panel).toBe('traces');
    await flushPromises();
    expect(fake.channel.broadcasts).toHaveLength(1);
    expect((fake.channel.broadcasts[0] as unknown as Record<string, unknown>)['type']).toBe(
      'nai.lattice.trace',
    );

    await expect(sdk.raiseSelection(validSelection())).resolves.toBe(true);
    expect(fake.channel.broadcasts).toHaveLength(2);
    expect((fake.channel.broadcasts[1] as unknown as Record<string, unknown>)['type']).toBe(
      'nai.lattice.logSelection',
    );
    interop.dispose();
  });

  it('does not fabricate an agent or a channel when none exists', async () => {
    const interop = new Fdc3Adapter({ agent: null });
    await interop.connect();
    const investigation = new InvestigationStore();
    const sdk = installModuleSdk({ investigation, interop });
    expect(sdk.channel).toBeNull();
    sdk.focusTrace('trace-offline');
    expect(investigation.trace.value).toBe('trace-offline');
    await expect(sdk.raiseSelection(validSelection())).resolves.toBe(false);
    interop.dispose();
  });

  it('rejects an invalid context and removes only its own installation', async () => {
    const fake = new FakeAgent();
    const interop = new Fdc3Adapter({ agent: fake as unknown as DesktopAgent });
    await interop.connect();
    const sdk = installModuleSdk({ investigation: new InvestigationStore(), interop });
    await expect(sdk.raiseSelection({ type: 'nai.lattice.logSelection' })).resolves.toBe(false);
    expect(fake.channel.broadcasts).toHaveLength(0);

    const foreign = { version: 1 } as NaiLatticeSdkV1;
    window.naiLattice = foreign;
    uninstallModuleSdk(undefined, sdk);
    expect(window.naiLattice).toBe(foreign);
    uninstallModuleSdk();
    expect(window.naiLattice).toBeUndefined();
    interop.dispose();
  });
});
