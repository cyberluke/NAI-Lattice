import { describe, expect, it, vi } from 'vitest';
import type { Channel, Context, DesktopAgent, Listener } from '@finos/fdc3';
import { Fdc3Adapter, LATTICE_APP_CHANNEL } from '../src/interop/adapter';
import type { LogRecord } from '../src/telemetry/types';

function listener(): Listener {
  return { unsubscribe: vi.fn() } as unknown as Listener;
}

class FakeChannel {
  readonly id: string;
  readonly type = 'app' as const;
  broadcasts: Context[] = [];
  contextHandlers: Array<(context: Context) => void> = [];
  contextTypes: Array<string | null> = [];

  constructor(id: string) {
    this.id = id;
  }

  async broadcast(context: Context): Promise<void> {
    this.broadcasts.push(context);
  }

  async addContextListener(
    type: string | null,
    handler: (context: Context) => void,
  ): Promise<Listener> {
    this.contextTypes.push(type);
    this.contextHandlers.push(handler);
    return listener();
  }

  async getCurrentContext(): Promise<Context | null> {
    return null;
  }
}

class FakeAgent {
  broadcasts: Context[] = [];
  contextHandlers: Array<(context: Context) => void> = [];
  intentHandlers: Array<(context: Context) => void> = [];
  channelRequests: string[] = [];
  channel = new FakeChannel(LATTICE_APP_CHANNEL);
  channelFailure = false;
  unsubscribed = 0;

  async broadcast(context: Context): Promise<void> {
    this.broadcasts.push(context);
  }

  async addContextListener(
    _type: string | null,
    handler: (context: Context) => void,
  ): Promise<Listener> {
    this.contextHandlers.push(handler);
    return listener();
  }

  async addIntentListener(
    _intent: string,
    handler: (context: Context) => void,
  ): Promise<Listener> {
    this.intentHandlers.push(handler);
    return listener();
  }

  async getOrCreateChannel(channelId: string): Promise<Channel> {
    this.channelRequests.push(channelId);
    if (this.channelFailure) throw new Error('agent has no app channels');
    return this.channel as unknown as Channel;
  }

  async getInfo(): Promise<{ name: string }> {
    return { name: 'Fake Agent' };
  }
}

class NoChannelAgent {
  broadcasts: Context[] = [];
  contextHandlers: Array<(context: Context) => void> = [];
  intentHandlers: Array<(context: Context) => void> = [];

  async broadcast(context: Context): Promise<void> {
    this.broadcasts.push(context);
  }

  async addContextListener(
    _type: string | null,
    handler: (context: Context) => void,
  ): Promise<Listener> {
    this.contextHandlers.push(handler);
    return listener();
  }

  async addIntentListener(
    _intent: string,
    handler: (context: Context) => void,
  ): Promise<Listener> {
    this.intentHandlers.push(handler);
    return listener();
  }

  async getInfo(): Promise<{ name: string }> {
    return { name: 'No Channel Agent' };
  }
}

const record: LogRecord = {
  runtime_sequence: 7,
  timestamp_utc_ms: 1_764_000_000_123,
  level: 'warn',
  component: 'nai-os.test',
  trace_id: 'trace-123',
  message: 'this body must never leave the dashboard',
};

function traceContext(traceId: string): Context {
  return {
    type: 'nai.lattice.trace',
    schema_version: 1,
    id: { trace_id: traceId },
  } as unknown as Context;
}

describe('Fdc3Adapter app channel isolation', () => {
  it('joins nai.lattice and raises minimal contexts there, not on the agent default channel', async () => {
    const fake = new FakeAgent();
    const adapter = new Fdc3Adapter({ agent: fake as unknown as DesktopAgent });
    await adapter.connect();
    expect(adapter.status.value).toBe('ready');
    expect(adapter.agentName.value).toBe('Fake Agent');
    expect(fake.channelRequests).toEqual(['nai.lattice']);
    expect(adapter.channel.value).toBe(LATTICE_APP_CHANNEL);
    expect(adapter.channelMode.value).toBe('app');
    expect(fake.channel.contextTypes).toEqual(['nai.lattice.trace']);
    expect(fake.contextHandlers).toHaveLength(0);

    const sent = await adapter.selectLog(record);
    expect(sent).toBe(true);
    expect(fake.broadcasts).toHaveLength(0);
    expect(fake.channel.broadcasts).toHaveLength(1);
    const context = fake.channel.broadcasts[0] as unknown as Record<string, unknown>;
    expect(context['type']).toBe('nai.lattice.logSelection');
    expect(context['schema_version']).toBe(1);
    expect(context['id']).toEqual({ runtime_sequence: '7', trace_id: 'trace-123' });
    expect(context['sequence']).toBe(7);
    expect(context['component']).toBe('nai-os.test');
    expect(context['message']).toBeUndefined();
    adapter.dispose();
  });

  it('filters channel listeners to nai.lattice.trace and focus intents', async () => {
    const fake = new FakeAgent();
    const adapter = new Fdc3Adapter({ agent: fake as unknown as DesktopAgent });
    await adapter.connect();
    const focused: string[] = [];
    adapter.onFocusTrace((traceId) => focused.push(traceId));

    fake.channel.contextHandlers[0](traceContext('trace-context'));
    fake.channel.contextHandlers[0]({ type: 'fdc3.contact', id: { email: 'x' } } as Context);
    fake.intentHandlers[0](traceContext('trace-intent'));

    expect(focused).toEqual(['trace-context', 'trace-intent']);
    adapter.dispose();
  });

  it('raises a trace context and keeps private contexts on the default channel', async () => {
    const fake = new FakeAgent();
    const adapter = new Fdc3Adapter({ agent: fake as unknown as DesktopAgent });
    await adapter.connect();
    await expect(adapter.raiseTrace('  trace-raised  ')).resolves.toBe(true);
    expect(fake.channel.broadcasts).toHaveLength(1);
    expect((fake.channel.broadcasts[0] as unknown as Record<string, unknown>)['type']).toBe(
      'nai.lattice.trace',
    );
    expect(fake.broadcasts).toHaveLength(0);

    const privateContext = { type: 'nai.lattice.private', schema_version: 1 } as Context;
    await expect(adapter.raisePrivate(privateContext)).resolves.toBe(true);
    expect(fake.broadcasts).toEqual([privateContext]);
    expect(fake.channel.broadcasts).toHaveLength(1);
    adapter.dispose();
  });

  it('falls back to the default channel when the agent lacks app channels', async () => {
    const fake = new NoChannelAgent();
    const adapter = new Fdc3Adapter({ agent: fake as unknown as DesktopAgent });
    await adapter.connect();
    expect(adapter.status.value).toBe('ready');
    expect(adapter.channel.value).toBeNull();
    expect(adapter.channelMode.value).toBe('default');
    expect(adapter.detail.value).toContain('default channel');
    expect(fake.contextHandlers).toHaveLength(1);

    await expect(adapter.selectLog(record)).resolves.toBe(true);
    expect(fake.broadcasts).toHaveLength(1);
    expect((fake.broadcasts[0] as unknown as Record<string, unknown>)['type']).toBe(
      'nai.lattice.logSelection',
    );
    adapter.dispose();
  });

  it('falls back when getOrCreateChannel rejects, without turning readiness into an error', async () => {
    const fake = new FakeAgent();
    fake.channelFailure = true;
    const adapter = new Fdc3Adapter({ agent: fake as unknown as DesktopAgent });
    await adapter.connect();
    expect(adapter.status.value).toBe('ready');
    expect(adapter.channel.value).toBeNull();
    expect(adapter.channelMode.value).toBe('default');
    expect(fake.contextHandlers).toHaveLength(1);
    await expect(adapter.selectLog(record)).resolves.toBe(true);
    expect(fake.broadcasts).toHaveLength(1);
    adapter.dispose();
  });

  it('rejects an invalid selection locally instead of broadcasting it', async () => {
    const fake = new FakeAgent();
    const adapter = new Fdc3Adapter({ agent: fake as unknown as DesktopAgent });
    await adapter.connect();
    await expect(adapter.raiseSelection({ type: 'nai.lattice.logSelection' })).resolves.toBe(false);
    expect(adapter.lastError.value).toContain('validation');
    expect(fake.channel.broadcasts).toHaveLength(0);
    expect(fake.broadcasts).toHaveLength(0);
    adapter.dispose();
  });

  it('reports unavailable when there is no desktop agent, without throwing', async () => {
    const adapter = new Fdc3Adapter({ agent: null });
    await adapter.connect();
    expect(adapter.status.value).toBe('unavailable');
    expect(adapter.detail.value).toBe('no desktop agent');
    expect(adapter.channel.value).toBeNull();
    expect(adapter.channelMode.value).toBeNull();
    await expect(adapter.selectLog(record)).resolves.toBe(false);
    expect(adapter.status.value).toBe('unavailable');
    adapter.dispose();
  });

  it('turns discovery failure into an honest unavailable state', async () => {
    const resolveAgent = vi.fn(async () => {
      throw new Error('no agent on this desktop');
    });
    const adapter = new Fdc3Adapter({ resolveAgent, discoveryTimeoutMs: 50 });
    await adapter.connect();
    expect(adapter.status.value).toBe('unavailable');
    expect(adapter.lastError.value).toContain('no agent on this desktop');
    adapter.dispose();
  });

  it('does not throw when a channel broadcast fails', async () => {
    const fake = new FakeAgent();
    fake.channel.broadcast = async () => {
      throw new Error('agent rejected');
    };
    const adapter = new Fdc3Adapter({ agent: fake as unknown as DesktopAgent });
    await adapter.connect();
    await expect(adapter.selectLog(record)).resolves.toBe(false);
    expect(adapter.lastError.value).toBe('agent rejected');
    adapter.dispose();
  });
});
