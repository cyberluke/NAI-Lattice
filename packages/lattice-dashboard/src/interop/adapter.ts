import type { Channel, Context, DesktopAgent, Listener } from '@finos/fdc3';
import { shallowRef, type ShallowRef } from 'vue';
import type { LogRecord } from '../telemetry/types';
import {
  INSPECT_TRACE_INTENT,
  TRACE_CONTEXT,
  buildLogSelectionContext,
  buildTraceContext,
  isLogSelectionContext,
  readTraceId,
} from './contexts';

export type Fdc3Status = 'initializing' | 'ready' | 'unavailable' | 'error';

/** Dedicated app channel for Lattice contexts (frozen contract §10). */
export const LATTICE_APP_CHANNEL = 'nai.lattice';

/**
 * `app` means the adapter broadcasts/listens on the `nai.lattice` app channel.
 * `default` is the honest fallback for a Desktop Agent without channel
 * support: contexts go through the agent's default channel.
 */
export type Fdc3ChannelMode = 'app' | 'default' | null;

type FocusTraceHandler = (traceId: string) => void;

export interface Fdc3AdapterOptions {
  /** Injected agent for tests or an embedding host. `null` means "no agent". */
  agent?: DesktopAgent | null;
  /** Test/host override for discovery. Production uses `window.fdc3`/`getAgent()`. */
  resolveAgent?: () => Promise<DesktopAgent | null>;
  discoveryTimeoutMs?: number;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

async function discoverAgent(): Promise<DesktopAgent | null> {
  const injected = (globalThis as { fdc3?: DesktopAgent }).fdc3;
  if (injected !== undefined && injected !== null) return injected;
  const module = await import('@finos/fdc3');
  // The default FDC3 reference UI loads remote iframes (channel selector /
  // intent resolver). This dashboard is fully self-hosted, so both are off.
  const agent = await module.getAgent({ channelSelector: false, intentResolver: false });
  return agent ?? null;
}

function withTimeout<T>(promise: Promise<T>, timeoutMs: number): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(`desktop agent discovery timed out after ${timeoutMs}ms`));
    }, timeoutMs);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (error: unknown) => {
        clearTimeout(timer);
        reject(error);
      },
    );
  });
}

/**
 * Thin typed FDC3 adapter. An absent Desktop Agent is a normal state, never an
 * exception and never a fabricated agent: the dashboard stays fully usable.
 */
export class Fdc3Adapter {
  readonly status: ShallowRef<Fdc3Status> = shallowRef('initializing');
  readonly detail: ShallowRef<string | null> = shallowRef(null);
  readonly lastError: ShallowRef<string | null> = shallowRef(null);
  readonly agentName: ShallowRef<string | null> = shallowRef(null);
  /** App channel id once joined; null on the default-channel fallback. */
  readonly channel: ShallowRef<string | null> = shallowRef(null);
  readonly channelMode: ShallowRef<Fdc3ChannelMode> = shallowRef(null);

  private readonly injectedAgent: DesktopAgent | null | undefined;
  private readonly resolveAgent: () => Promise<DesktopAgent | null>;
  private readonly discoveryTimeoutMs: number;
  private agent: DesktopAgent | null = null;
  private channelObject: Channel | null = null;
  private listeners: Listener[] = [];
  private focusHandlers = new Set<FocusTraceHandler>();
  private disposed = false;
  private connecting: Promise<void> | null = null;

  constructor(options: Fdc3AdapterOptions = {}) {
    this.injectedAgent = options.agent;
    this.resolveAgent = options.resolveAgent ?? discoverAgent;
    this.discoveryTimeoutMs = options.discoveryTimeoutMs ?? 3000;
  }

  connect(): Promise<void> {
    if (this.disposed || this.status.value === 'ready') return Promise.resolve();
    if (this.connecting !== null) return this.connecting;
    this.connecting = this.doConnect().finally(() => {
      this.connecting = null;
    });
    return this.connecting;
  }

  async selectLog(record: LogRecord): Promise<boolean> {
    return this.raiseSelection(buildLogSelectionContext(record));
  }

  /**
   * Raises a `nai.lattice.logSelection` v1 context on the app channel. An
   * invalid context is rejected locally and never leaves the dashboard.
   */
  async raiseSelection(context: unknown): Promise<boolean> {
    if (!isLogSelectionContext(context)) {
      this.lastError.value = 'selection context failed validation';
      return false;
    }
    return this.raiseContext(context);
  }

  /** Raises a `nai.lattice.trace` context on the app channel. */
  async raiseTrace(traceId: string): Promise<boolean> {
    const id = traceId.trim();
    if (id.length === 0) return false;
    return this.raiseContext(buildTraceContext(id));
  }

  /**
   * Private contexts deliberately stay on the agent's default channel and are
   * never published on the shared `nai.lattice` app channel.
   */
  async raisePrivate(context: Context): Promise<boolean> {
    if (this.disposed || this.agent === null || this.status.value !== 'ready') return false;
    try {
      await this.agent.broadcast(context);
      this.lastError.value = null;
      return true;
    } catch (error) {
      this.lastError.value = error instanceof Error ? error.message : 'private broadcast failed';
      return false;
    }
  }

  private async raiseContext(context: Context): Promise<boolean> {
    if (this.disposed || this.agent === null || this.status.value !== 'ready') return false;
    try {
      if (this.channelObject !== null) {
        await this.channelObject.broadcast(context);
      } else {
        // Honest fallback for an agent without Channel support.
        await this.agent.broadcast(context);
      }
      this.lastError.value = null;
      return true;
    } catch (error) {
      this.lastError.value = error instanceof Error ? error.message : 'broadcast failed';
      return false;
    }
  }

  onFocusTrace(handler: FocusTraceHandler): () => void {
    this.focusHandlers.add(handler);
    return () => {
      this.focusHandlers.delete(handler);
    };
  }

  dispose(): void {
    this.disposed = true;
    for (const listener of this.listeners.splice(0)) {
      this.unsubscribeQuietly(listener);
    }
    this.focusHandlers.clear();
    this.agent = null;
    this.channelObject = null;
    this.channel.value = null;
    this.channelMode.value = null;
  }

  private async doConnect(): Promise<void> {
    try {
      let agent: DesktopAgent | null;
      if (this.injectedAgent !== undefined) {
        agent = this.injectedAgent;
      } else {
        agent = await withTimeout(this.resolveAgent(), this.discoveryTimeoutMs);
      }
      if (this.disposed) return;
      if (agent === null) {
        this.status.value = 'unavailable';
        this.detail.value = 'no desktop agent';
        this.agent = null;
        return;
      }
      try {
        await this.bind(agent);
      } catch (error) {
        if (this.disposed) return;
        this.status.value = 'error';
        this.detail.value = 'desktop agent listener registration failed';
        this.lastError.value = error instanceof Error ? error.message : 'listener registration failed';
      }
    } catch (error) {
      if (this.disposed) return;
      this.status.value = 'unavailable';
      this.detail.value = 'no desktop agent';
      this.lastError.value = error instanceof Error ? error.message : 'discovery failed';
    }
  }

  private async bind(agent: DesktopAgent): Promise<void> {
    this.agent = agent;
    const channel = await this.resolveAppChannel(agent);
    this.channelObject = channel;
    this.channel.value = channel?.id ?? null;
    this.channelMode.value = channel !== null ? 'app' : 'default';

    const onTraceContext = (context: Context): void => {
      // Listeners only accept the Lattice trace type; other channels/types are
      // ignored here even when an agent forwards a broader stream.
      const traceId = readTraceId(context);
      if (traceId !== null) this.dispatchFocusTrace(traceId);
    };

    if (channel !== null) {
      const contextListener = await channel.addContextListener(TRACE_CONTEXT, onTraceContext);
      this.listeners.push(contextListener);
    } else {
      const contextListener = await agent.addContextListener(TRACE_CONTEXT, onTraceContext);
      this.listeners.push(contextListener);
    }
    const intentListener = await agent.addIntentListener(INSPECT_TRACE_INTENT, (context) => {
      const traceId = readTraceId(context);
      if (traceId !== null) this.dispatchFocusTrace(traceId);
    });
    this.listeners.push(intentListener);
    if (this.disposed) {
      for (const listener of this.listeners.splice(0)) {
        this.unsubscribeQuietly(listener);
      }
      return;
    }
    // Identity is cosmetic; never let a slow getInfo() block readiness.
    this.agentName.value = await withTimeout(this.readAgentIdentity(agent), 750).catch(
      () => null,
    );
    this.status.value = 'ready';
    this.detail.value =
      channel !== null
        ? `desktop agent connected · app channel ${LATTICE_APP_CHANNEL}`
        : 'desktop agent connected · default channel (agent has no app channels)';
    this.lastError.value = null;
  }

  private async resolveAppChannel(agent: DesktopAgent): Promise<Channel | null> {
    if (typeof agent.getOrCreateChannel !== 'function') return null;
    try {
      const channel = await withTimeout(agent.getOrCreateChannel(LATTICE_APP_CHANNEL), 1500);
      if (channel === null || typeof channel.id !== 'string' || channel.id.length === 0) return null;
      return channel;
    } catch {
      // Channel support is optional: fall back to the agent's default channel
      // instead of claiming an app channel that does not exist.
      return null;
    }
  }

  private async readAgentIdentity(agent: DesktopAgent): Promise<string | null> {
    try {
      const metadata = await agent.getInfo();
      if (isRecord(metadata)) {
        const name = metadata['name'] ?? metadata['provider'];
        if (typeof name === 'string' && name.length > 0) return name;
      }
    } catch {
      // Info is optional; readiness does not depend on it.
    }
    return null;
  }

  private dispatchFocusTrace(traceId: string): void {
    if (this.disposed) return;
    for (const handler of this.focusHandlers) {
      try {
        handler(traceId);
      } catch {
        // One panel must not break interop delivery.
      }
    }
  }

  private unsubscribeQuietly(listener: Listener): void {
    try {
      const result: unknown = listener.unsubscribe();
      if (
        result !== null &&
        typeof result === 'object' &&
        typeof (result as Promise<void>).catch === 'function'
      ) {
        void (result as Promise<void>).catch(() => {
          // A dead agent must not break teardown.
        });
      }
    } catch {
      // Synchronous throw from a non-conforming agent.
    }
  }
}
