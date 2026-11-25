import { onBeforeUnmount, shallowRef, watch, type ShallowRef } from 'vue';
import { isAbortError } from '../../query/client';
import type { TraceGraph, TracesClient } from '../../query/traces';

export type TraceGraphState = 'idle' | 'loading' | 'ready' | 'legacy' | 'error';

export interface TraceGraphLoader {
  graph: ShallowRef<TraceGraph | null>;
  state: ShallowRef<TraceGraphState>;
  legacyReason: ShallowRef<string | null>;
  error: ShallowRef<string | null>;
  load(force?: boolean): Promise<void>;
  dispose(): void;
}

/**
 * One bounded graph read per trace selection with a generation guard.
 *
 * A 404 or a host-reported `coverage: "legacy"` is a truthful legacy state.
 * Other failures are errors; neither state is converted into a graph.
 */
export function useTraceGraph(
  client: TracesClient,
  traceId: () => string,
): TraceGraphLoader {
  const graph = shallowRef<TraceGraph | null>(null);
  const state = shallowRef<TraceGraphState>('idle');
  const legacyReason = shallowRef<string | null>(null);
  const error = shallowRef<string | null>(null);
  let controller: AbortController | null = null;
  let generation = 0;
  let disposed = false;

  function reset(): void {
    graph.value = null;
    legacyReason.value = null;
    error.value = null;
    state.value = 'idle';
  }

  async function load(force = false): Promise<void> {
    if (disposed) return;
    const id = traceId().trim();
    if (id.length === 0) {
      generation += 1;
      controller?.abort();
      controller = null;
      reset();
      return;
    }
    if (!force && state.value === 'loading') return;
    const currentGeneration = (generation += 1);
    controller?.abort();
    const current = new AbortController();
    controller = current;
    state.value = 'loading';
    legacyReason.value = null;
    error.value = null;
    try {
      const result = await client.fetchGraph(id, current.signal);
      if (disposed || current.signal.aborted || currentGeneration !== generation) return;
      if (result.status === 'unavailable') {
        graph.value = null;
        state.value = 'legacy';
        legacyReason.value = result.reason;
        return;
      }
      graph.value = result.graph;
      if (result.graph.coverage === 'legacy') {
        state.value = 'legacy';
        const diagnostic = result.graph.diagnostics.find((entry) =>
          /legacy|no_spans/i.test(entry.code),
        );
        legacyReason.value =
          result.graph.coverageDetail ??
          diagnostic?.message ??
          'The host reported legacy correlation coverage; no causal chain or span durations exist.';
        return;
      }
      state.value = 'ready';
    } catch (caught) {
      if (disposed || current.signal.aborted || isAbortError(caught)) return;
      if (currentGeneration !== generation) return;
      graph.value = null;
      state.value = 'error';
      error.value = caught instanceof Error ? caught.message : 'trace graph request failed';
    } finally {
      if (controller === current) controller = null;
    }
  }

  watch(
    traceId,
    (value) => {
      if (value.trim().length === 0) {
        generation += 1;
        controller?.abort();
        controller = null;
        reset();
        return;
      }
      graph.value = null;
      void load(true);
    },
    { immediate: true },
  );

  onBeforeUnmount(() => {
    disposed = true;
    generation += 1;
    controller?.abort();
    controller = null;
  });

  return {
    graph,
    state,
    legacyReason,
    error,
    load,
    dispose(): void {
      disposed = true;
      generation += 1;
      controller?.abort();
      controller = null;
    },
  };
}
