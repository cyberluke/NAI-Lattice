import { shallowRef, type ShallowRef } from 'vue';

export interface StreamInfo {
  activeSseClients: number | null;
  maxSseClients: number | null;
  pushTotal: number | null;
  subscribers: number | null;
  brokerScansTotal: number | null;
  lastScanUnixMs: number | null;
  droppedTotal: number | null;
}

export interface HealthInfo {
  state: string | null;
  detail: string | null;
  lastError: string | null;
  sinceUnixMs: number | null;
  uptimeMs: number | null;
}

export interface VersionInfo {
  host: string | null;
  dashboard: string | null;
  protocolMin: number | null;
  protocolMax: number | null;
  storageSchema: number | null;
}

export const STORAGE_CLASS_KEYS = [
  'ledger',
  'cas',
  'hot',
  'search',
  'vector',
  'readModels',
  'logs',
  'backups',
  'audit',
] as const;

export type StorageClassKey = (typeof STORAGE_CLASS_KEYS)[number];
export type StorageClassSizes = Record<StorageClassKey, number | null>;
export type StoragePressure = 'normal' | 'elevated' | 'critical' | 'unknown';

export interface StorageRetention {
  logsDays: number | null;
  auditDays: number | null;
  backupDays: number | null;
  runtimeKeep: number | null;
}

export interface StoragePrune {
  runUnixMs: number | null;
  bytesFreed: number | null;
}

/** `summary.storage` (frozen contract §1, present once `storage_api` is advertised). */
export interface StorageInfo {
  schema: number | null;
  root: string | null;
  base: string | null;
  budgetBytes: number | null;
  usedBytes: number | null;
  freeBytes: number | null;
  reserveBytes: number | null;
  pressure: StoragePressure;
  /** null when the host did not send a class map at all. */
  classes: StorageClassSizes | null;
  retention: StorageRetention | null;
  /** null means no prune audit record exists. */
  lastPrune: StoragePrune | null;
}

/** `GET /lattice/api/summary` (frozen contract §1). */
export interface LatticeSummary {
  schema: number;
  generatedAt: number | null;
  window: number | null;
  levels: Record<string, number>;
  errors: number | null;
  components: Record<string, number>;
  distinctTraceIds: number | null;
  stream: StreamInfo | null;
  health: HealthInfo | null;
  version: VersionInfo | null;
  features: string[] | null;
  storage: StorageInfo | null;
}

function numberOrNull(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function byteCountOrNull(value: unknown): number | null {
  const valueAsNumber = numberOrNull(value);
  return valueAsNumber !== null && valueAsNumber >= 0 ? valueAsNumber : null;
}

function recordOrNull(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

/**
 * Normalises `summary.storage` without inventing measurements: a field the
 * host did not send is null. `pressure` is a closed set, so anything else is
 * reported as `unknown` and rendered with a neutral tone.
 */
export function parseStorage(value: unknown): StorageInfo | null {
  const raw = recordOrNull(value);
  if (raw === null) return null;

  const classesRaw = recordOrNull(raw['classes']);
  let classes: StorageClassSizes | null = null;
  if (classesRaw !== null) {
    classes = {} as StorageClassSizes;
    for (const key of STORAGE_CLASS_KEYS) {
      // The contract guarantees every class: a missing class is a real zero,
      // while a present but malformed value stays unknown (null).
      classes[key] = key in classesRaw ? byteCountOrNull(classesRaw[key]) : 0;
    }
  }

  const retentionRaw = recordOrNull(raw['retention']);
  const retention =
    retentionRaw === null
      ? null
      : {
          logsDays: numberOrNull(retentionRaw['logsDays']),
          auditDays: numberOrNull(retentionRaw['auditDays']),
          backupDays: numberOrNull(retentionRaw['backupDays']),
          runtimeKeep: numberOrNull(retentionRaw['runtimeKeep']),
        };

  const pruneRaw = recordOrNull(raw['lastPrune']);
  const lastPrune =
    pruneRaw === null
      ? null
      : {
          runUnixMs: numberOrNull(pruneRaw['runUnixMs']),
          bytesFreed: byteCountOrNull(pruneRaw['bytesFreed']),
        };

  const rawPressure = typeof raw['pressure'] === 'string' ? raw['pressure'].toLowerCase() : '';
  const pressure: StoragePressure =
    rawPressure === 'normal' || rawPressure === 'elevated' || rawPressure === 'critical'
      ? rawPressure
      : 'unknown';

  return {
    schema: numberOrNull(raw['schema']),
    root: stringOrNull(raw['root']),
    base: stringOrNull(raw['base']),
    budgetBytes: byteCountOrNull(raw['budgetBytes']),
    usedBytes: byteCountOrNull(raw['usedBytes']),
    freeBytes: byteCountOrNull(raw['freeBytes']),
    reserveBytes: byteCountOrNull(raw['reserveBytes']),
    pressure,
    classes,
    retention,
    lastPrune,
  };
}

function stringOrNull(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}

function countMap(value: unknown): Record<string, number> {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return {};
  const out: Record<string, number> = {};
  for (const [key, raw] of Object.entries(value as Record<string, unknown>)) {
    if (typeof raw === 'number' && Number.isFinite(raw)) out[key] = raw;
  }
  return out;
}

export function parseSummary(payload: unknown): LatticeSummary {
  if (payload === null || typeof payload !== 'object' || Array.isArray(payload)) {
    throw new Error('summary returned an unexpected payload');
  }
  const raw = payload as Record<string, unknown>;
  const schema = numberOrNull(raw['schema']);
  if (schema !== 1) {
    throw new Error(`unsupported summary schema: ${String(raw['schema'])}`);
  }
  const streamRaw = raw['stream'];
  const stream =
    streamRaw !== null && typeof streamRaw === 'object' && !Array.isArray(streamRaw)
      ? {
          activeSseClients: numberOrNull((streamRaw as Record<string, unknown>)['activeSseClients']),
          maxSseClients: numberOrNull((streamRaw as Record<string, unknown>)['maxSseClients']),
          pushTotal: numberOrNull((streamRaw as Record<string, unknown>)['pushTotal']),
          subscribers: numberOrNull((streamRaw as Record<string, unknown>)['subscribers']),
          brokerScansTotal: numberOrNull(
            (streamRaw as Record<string, unknown>)['brokerScansTotal'],
          ),
          lastScanUnixMs: numberOrNull((streamRaw as Record<string, unknown>)['lastScanUnixMs']),
          droppedTotal: numberOrNull((streamRaw as Record<string, unknown>)['droppedTotal']),
        }
      : null;
  const healthRaw = raw['health'];
  const health =
    healthRaw !== null && typeof healthRaw === 'object' && !Array.isArray(healthRaw)
      ? {
          state: stringOrNull((healthRaw as Record<string, unknown>)['state']),
          detail: stringOrNull((healthRaw as Record<string, unknown>)['detail']),
          lastError: stringOrNull((healthRaw as Record<string, unknown>)['lastError']),
          sinceUnixMs: numberOrNull((healthRaw as Record<string, unknown>)['sinceUnixMs']),
          uptimeMs: numberOrNull((healthRaw as Record<string, unknown>)['uptimeMs']),
        }
      : null;
  const versionRaw = raw['version'];
  const version =
    versionRaw !== null && typeof versionRaw === 'object' && !Array.isArray(versionRaw)
      ? {
          host: stringOrNull((versionRaw as Record<string, unknown>)['host']),
          dashboard: stringOrNull((versionRaw as Record<string, unknown>)['dashboard']),
          protocolMin: numberOrNull((versionRaw as Record<string, unknown>)['protocolMin']),
          protocolMax: numberOrNull((versionRaw as Record<string, unknown>)['protocolMax']),
          storageSchema: numberOrNull((versionRaw as Record<string, unknown>)['storageSchema']),
        }
      : null;
  const featuresRaw = raw['features'];
  const features = Array.isArray(featuresRaw)
    ? featuresRaw.filter((feature): feature is string => typeof feature === 'string')
    : null;
  const storage = parseStorage(raw['storage']);

  return {
    schema,
    generatedAt: numberOrNull(raw['generatedAt']),
    window: numberOrNull(raw['window']),
    levels: countMap(raw['levels']),
    errors: numberOrNull(raw['errors']),
    components: countMap(raw['components']),
    distinctTraceIds: numberOrNull(raw['distinctTraceIds']),
    stream,
    health,
    version,
    features,
    storage,
  };
}

export interface SummaryStoreOptions {
  url?: string;
  fetchImpl?: typeof fetch;
  intervalMs?: number;
  now?: () => number;
}

/**
 * Shared summary poller: one request per interval for the shell KPI ribbon,
 * health and component panels. A failed poll keeps the last measured payload
 * and marks it stale instead of fabricating zeros.
 */
export class SummaryStore {
  readonly summary: ShallowRef<LatticeSummary | null> = shallowRef(null);
  readonly error: ShallowRef<string | null> = shallowRef(null);
  readonly stale: ShallowRef<boolean> = shallowRef(false);
  readonly loading: ShallowRef<boolean> = shallowRef(false);
  readonly lastFetchedAt: ShallowRef<number | null> = shallowRef(null);

  private readonly url: string;
  private readonly fetchImpl: typeof fetch | null;
  private readonly intervalMs: number;
  private readonly now: () => number;
  private timer: ReturnType<typeof setInterval> | null = null;
  private controller: AbortController | null = null;
  private inFlight = false;
  private disposed = false;
  private visibilityHandler: (() => void) | null = null;

  constructor(options: SummaryStoreOptions = {}) {
    this.url = options.url ?? '/lattice/api/summary';
    this.fetchImpl =
      options.fetchImpl ??
      // `fetch` must keep its Window receiver. Extracting it unbound throws
      // "Illegal invocation" in a real browser (jsdom does not catch this).
      (typeof globalThis.fetch === 'function' ? globalThis.fetch.bind(globalThis) : null);
    this.intervalMs = options.intervalMs ?? 5000;
    this.now = options.now ?? (() => Date.now());
  }

  start(): void {
    if (this.disposed || this.timer !== null) return;
    void this.refresh(true);
    this.timer = setInterval(() => {
      if (typeof document !== 'undefined' && document.visibilityState === 'hidden') return;
      void this.refresh(false);
    }, this.intervalMs);
    if (typeof document !== 'undefined' && typeof document.addEventListener === 'function') {
      this.visibilityHandler = () => {
        if (document.visibilityState === 'visible') void this.refresh(false);
      };
      document.addEventListener('visibilitychange', this.visibilityHandler);
    }
  }

  async refresh(_force = false): Promise<void> {
    if (this.disposed || this.inFlight) return;
    if (this.fetchImpl === null) {
      this.error.value = 'fetch is unavailable in this runtime';
      return;
    }
    this.inFlight = true;
    this.loading.value = true;
    const controller = new AbortController();
    this.controller = controller;
    try {
      const response = await this.fetchImpl(this.url, {
        method: 'GET',
        headers: { Accept: 'application/json' },
        cache: 'no-store',
        credentials: 'same-origin',
        signal: controller.signal,
      });
      if (!response.ok) throw new Error(`summary failed: HTTP ${response.status}`);
      const payload = await response.json();
      const summary = parseSummary(payload);
      if (this.disposed || controller.signal.aborted) return;
      this.summary.value = summary;
      this.lastFetchedAt.value = this.now();
      this.error.value = null;
      this.stale.value = false;
    } catch (error) {
      if (this.disposed || controller.signal.aborted) return;
      this.error.value = error instanceof Error ? error.message : 'summary failed';
      if (this.summary.value !== null) this.stale.value = true;
    } finally {
      if (this.controller === controller) this.controller = null;
      this.inFlight = false;
      this.loading.value = false;
    }
  }

  stop(): void {
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
    if (this.visibilityHandler !== null && typeof document !== 'undefined') {
      document.removeEventListener('visibilitychange', this.visibilityHandler);
      this.visibilityHandler = null;
    }
    if (this.controller !== null) {
      this.controller.abort();
      this.controller = null;
    }
    this.inFlight = false;
    this.loading.value = false;
  }

  dispose(): void {
    this.disposed = true;
    this.stop();
  }
}
