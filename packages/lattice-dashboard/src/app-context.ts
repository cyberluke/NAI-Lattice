import type { InjectionKey } from 'vue';
import type { Fdc3Adapter } from './interop/adapter';
import type { QueryClient } from './query/client';
import type { TracesClient } from './query/traces';
import type { TelemetryFeed } from './telemetry/feed';
import type { SummaryStore } from './telemetry/summary';
import type { InvestigationStore } from './state/investigation';

/** Shared, document-scoped services. One SSE lease per document. */
export interface LatticeAppContext {
  feed: TelemetryFeed;
  summary: SummaryStore;
  query: QueryClient;
  traces: TracesClient;
  interop: Fdc3Adapter;
  investigation: InvestigationStore;
}

export const APP_CONTEXT_KEY: InjectionKey<LatticeAppContext> = Symbol('lattice.app.context');
