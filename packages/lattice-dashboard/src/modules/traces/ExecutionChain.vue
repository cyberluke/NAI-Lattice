<script setup lang="ts">
import { computed, inject, onBeforeUnmount, shallowRef, watch } from 'vue';
import { APP_CONTEXT_KEY } from '../../app-context';
import { isAbortError } from '../../query/client';
import type { TraceSummary } from '../../query/traces';
import { formatCount } from '../../ui/format';
import { legacyEventCount, traceGraphCounts } from './chain';
import TraceChainGraph from './TraceChainGraph.vue';
import { useTraceGraph } from './useTraceGraph';

defineProps<{ instanceId?: string }>();

const context = inject(APP_CONTEXT_KEY);
if (context === undefined) throw new Error('execution chain context is missing');

const investigation = context.investigation;
const traceId = computed(() => investigation.trace.value.trim());
const loader = useTraceGraph(context.traces, () => traceId.value);

const trace = shallowRef<TraceSummary | null>(null);
const traceState = shallowRef<'idle' | 'loading' | 'ready' | 'not-found' | 'error'>('idle');
let traceController: AbortController | null = null;
let traceGeneration = 0;
let disposed = false;

const usableGraph = computed(() =>
  loader.graph.value !== null && loader.graph.value.coverage !== 'legacy'
    ? loader.graph.value
    : null,
);
const counts = computed(() =>
  traceGraphCounts(usableGraph.value, legacyEventCount(trace.value, loader.graph.value)),
);

async function loadTrace(): Promise<void> {
  if (disposed) return;
  const id = traceId.value;
  if (id.length === 0) {
    traceGeneration += 1;
    traceController?.abort();
    traceController = null;
    trace.value = null;
    traceState.value = 'idle';
    return;
  }
  const currentGeneration = (traceGeneration += 1);
  traceController?.abort();
  const current = new AbortController();
  traceController = current;
  traceState.value = 'loading';
  trace.value = null;
  try {
    const result = await context!.traces.fetchTrace(id, current.signal);
    if (disposed || current.signal.aborted || currentGeneration !== traceGeneration) return;
    const found = result.traces.find((candidate) => candidate.traceId === id) ?? null;
    trace.value = found;
    traceState.value = found === null ? 'not-found' : 'ready';
  } catch (caught) {
    if (disposed || current.signal.aborted || isAbortError(caught)) return;
    if (currentGeneration !== traceGeneration) return;
    trace.value = null;
    traceState.value = 'error';
  } finally {
    if (traceController === current) traceController = null;
  }
}

function refresh(): void {
  void loader.load(true);
  void loadTrace();
}

function openTraces(): void {
  investigation.requestPanel('traces');
}

watch(
  traceId,
  () => {
    traceGeneration += 1;
    traceController?.abort();
    traceController = null;
    trace.value = null;
    traceState.value = 'idle';
    void loadTrace();
  },
  { immediate: true },
);

onBeforeUnmount(() => {
  disposed = true;
  traceGeneration += 1;
  traceController?.abort();
  traceController = null;
});
</script>

<template>
  <section class="execution-chain" aria-label="Execution chain for the selected trace">
    <div class="panel-toolbar">
      <span class="panel-title">execution chain</span>
      <span v-if="traceId.length > 0" class="panel-meta" :title="traceId">
        trace <strong class="trace-id">{{ traceId }}</strong>
      </span>
      <span v-if="trace?.state" class="panel-meta"
        >{{ trace.state }}<template v-if="trace.ok === false"> · failed</template></span
      >
      <span
        class="panel-meta nai-numbers trace-counts"
        title="EVENTS = point records that are not span or call evidence · SPANS = span nodes · CALLS = explicit call edges"
        >EVENTS {{ formatCount(counts.events) }} · SPANS {{ formatCount(counts.spans) }} · CALLS
        {{ formatCount(counts.calls) }}</span
      >
      <span v-if="loader.state.value === 'loading'" class="panel-meta">resolving graph…</span>
      <span
        v-else-if="loader.state.value === 'error'"
        class="tone--bad"
        :title="loader.error.value ?? undefined"
        >graph error</span
      >
      <span class="toolbar-spacer" />
      <button type="button" :disabled="loader.state.value === 'loading'" @click="refresh">
        REFRESH
      </button>
    </div>

    <div v-if="traceId.length === 0" class="panel-state">
      <span>Select a trace in Request → Response to inspect its execution chain.</span>
      <button type="button" @click="openTraces">OPEN REQUEST → RESPONSE</button>
    </div>

    <div v-else class="chain-scroll">
      <div v-if="traceState === 'error'" class="trace-metadata-note tone--warn" role="status">
        The legacy trace summary could not be read; graph evidence is shown as returned by the host.
      </div>
      <div v-else-if="traceState === 'not-found'" class="trace-metadata-note tone--warn" role="status">
        The trace summary was not found in the bounded ledger scan; the graph result is shown as
        returned.
      </div>
      <TraceChainGraph
        :trace="trace"
        :graph="loader.graph.value"
        :state="loader.state.value"
        :legacy-reason="loader.legacyReason.value"
        :graph-error="loader.error.value"
        @retry="refresh"
      />
    </div>
  </section>
</template>

<style scoped>
.execution-chain {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  background: var(--nai-panel-2);
}

.panel-toolbar {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex-wrap: wrap;
  flex: 0 0 auto;
  min-height: var(--nai-toolbar-h);
  padding: 0 var(--nai-space-4);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
}

.panel-title {
  color: var(--nai-accent);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.8px;
}

.panel-meta {
  color: var(--nai-dim);
  font-size: 9px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.trace-id {
  color: var(--nai-fg-soft);
  font-weight: 400;
}

.trace-counts {
  color: var(--nai-muted);
}

.toolbar-spacer {
  flex: 1 1 auto;
}

button {
  height: 20px;
  padding: 0 var(--nai-space-4);
  border: var(--nai-hairline) solid var(--nai-line);
  border-radius: 0;
  background: transparent;
  color: var(--nai-muted);
  font-size: 9px;
  letter-spacing: 0.6px;
  text-transform: uppercase;
}

button:hover:not(:disabled) {
  color: var(--nai-accent);
  border-color: var(--nai-accent);
  background: var(--nai-hover);
}

button:disabled {
  opacity: 0.45;
}

.panel-state {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: var(--nai-space-4);
  flex: 1 1 auto;
  padding: var(--nai-space-5);
  color: var(--nai-muted);
  font-size: var(--nai-font-size-sm);
  text-align: center;
}

.chain-scroll {
  flex: 1 1 auto;
  min-height: 0;
  overflow: auto;
  scrollbar-gutter: stable;
}

.trace-metadata-note {
  padding: var(--nai-space-2) var(--nai-space-4);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  font-size: 9px;
}

.tone--warn {
  color: var(--nai-warn);
}

.tone--bad {
  color: var(--nai-bad);
}
</style>
