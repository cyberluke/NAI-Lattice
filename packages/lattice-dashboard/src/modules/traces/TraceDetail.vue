<script setup lang="ts">
import { computed, inject, ref, shallowRef } from 'vue';
import { APP_CONTEXT_KEY } from '../../app-context';
import type { TraceSummary } from '../../query/traces';
import { formatCount } from '../../ui/format';
import {
  legacyEventCount,
  traceGraphCounts,
  type TraceGraphCounts,
} from './chain';
import TraceCallTree from './TraceCallTree.vue';
import TraceChainGraph from './TraceChainGraph.vue';
import TraceEvents from './TraceEvents.vue';
import TraceWaterfall from './TraceWaterfall.vue';
import { useTraceGraph } from './useTraceGraph';

const props = defineProps<{ trace: TraceSummary }>();

const context = inject(APP_CONTEXT_KEY);
if (context === undefined) throw new Error('trace detail context is missing');

const loader = useTraceGraph(context.traces, () => props.trace.traceId);

const TABS = [
  { key: 'chain', label: 'CHAIN' },
  { key: 'waterfall', label: 'WATERFALL' },
  { key: 'tree', label: 'CALL TREE' },
  { key: 'events', label: 'EVENTS' },
] as const;
type TabKey = (typeof TABS)[number]['key'];

const tab = ref<TabKey>('chain');
const focusSpanId = shallowRef<string | null>(null);

const usableGraph = computed(() =>
  loader.graph.value !== null && loader.graph.value.coverage !== 'legacy'
    ? loader.graph.value
    : null,
);
const counts = computed<TraceGraphCounts>(() =>
  traceGraphCounts(
    usableGraph.value,
    legacyEventCount(props.trace, loader.graph.value),
  ),
);
const isLegacy = computed(() => loader.state.value === 'legacy');
const isError = computed(() => loader.state.value === 'error');
const isChainTab = computed(() => tab.value === 'chain');

function selectTab(next: TabKey): void {
  tab.value = next;
}

function refresh(): void {
  void loader.load(true);
}

function focusTrace(): void {
  // Node/edge selection in the expanded row also makes the trace the shared
  // investigation focus, so linked modules agree on the selected trace.
  context?.investigation?.setTrace(props.trace.traceId);
}

function focusSpan(spanId: string): void {
  focusTrace();
  focusSpanId.value = spanId;
  if (usableGraph.value !== null) tab.value = 'chain';
}
</script>

<template>
  <div class="trace-detail">
    <div class="trace-detail-toolbar">
      <div class="trace-tabs" role="tablist" aria-label="Trace detail">
        <button
          v-for="entry in TABS"
          :key="entry.key"
          type="button"
          role="tab"
          class="trace-tab"
          :class="{ 'trace-tab--active': tab === entry.key }"
          :aria-selected="tab === entry.key"
          @click="selectTab(entry.key)"
        >
          {{ entry.label }}
        </button>
      </div>
      <span
        class="trace-counts nai-numbers"
        title="EVENTS = point records that are not span or call evidence · SPANS = span nodes · CALLS = explicit call edges"
        >EVENTS {{ formatCount(counts.events) }} · SPANS {{ formatCount(counts.spans) }} · CALLS
        {{ formatCount(counts.calls) }}</span
      >
      <span v-if="loader.state.value === 'loading'" class="panel-meta">resolving graph…</span>
      <span v-else-if="usableGraph !== null" class="panel-meta"
        >coverage {{ usableGraph.coverage }}</span
      >
      <span v-if="counts.inferredCalls > 0" class="panel-meta"
        >+{{ formatCount(counts.inferredCalls) }} inferred calls hidden</span
      >
      <span class="toolbar-spacer" />
      <button
        type="button"
        class="trace-refresh"
        :disabled="loader.state.value === 'loading'"
        @click="refresh"
      >
        REFRESH GRAPH
      </button>
    </div>

    <div
      v-if="!isChainTab && isLegacy"
      class="trace-fallback trace-fallback--legacy"
      role="status"
    >
      <strong>CAUSAL CHAIN UNAVAILABLE — legacy correlation only</strong>
      <span>{{ loader.legacyReason.value }}</span>
    </div>
    <div
      v-else-if="!isChainTab && isError"
      class="trace-fallback trace-fallback--error"
      role="alert"
    >
      <strong>CAUSAL CHAIN UNAVAILABLE — graph request failed</strong>
      <span>{{ loader.error.value }}</span>
      <button type="button" @click="refresh">RETRY</button>
    </div>

    <div
      v-if="!isChainTab && usableGraph !== null && usableGraph.diagnostics.length > 0"
      class="trace-diagnostics"
      role="status"
    >
      <span
        v-for="diagnostic in usableGraph.diagnostics"
        :key="`${diagnostic.code}-${diagnostic.message}`"
        class="trace-diagnostic"
        :class="`trace-diagnostic--${diagnostic.severity.toLowerCase()}`"
      >
        {{ diagnostic.severity }} · {{ diagnostic.code }}: {{ diagnostic.message }}
      </span>
    </div>

    <div class="trace-detail-body">
      <TraceChainGraph
        v-if="tab === 'chain'"
        role="tabpanel"
        aria-label="Execution chain"
        :trace="trace"
        :graph="loader.graph.value"
        :state="loader.state.value"
        :legacy-reason="loader.legacyReason.value"
        :graph-error="loader.error.value"
        :focus-span-id="focusSpanId"
        @retry="refresh"
        @select-node="focusTrace"
        @select-edge="focusTrace"
        @select-span="focusSpan"
      />
      <TraceWaterfall
        v-else-if="tab === 'waterfall'"
        role="tabpanel"
        aria-label="Trace waterfall"
        :trace="trace"
        :graph="loader.graph.value"
      />
      <TraceCallTree
        v-else-if="tab === 'tree'"
        role="tabpanel"
        aria-label="Call tree"
        :graph="usableGraph"
      />
      <TraceEvents
        v-else
        role="tabpanel"
        aria-label="Point events"
        :trace="trace"
        :graph="loader.graph.value"
        @select-span="focusSpan"
      />
    </div>
  </div>
</template>

<style scoped>
.trace-detail {
  display: flex;
  flex-direction: column;
  min-height: 0;
  border-top: var(--nai-hairline) solid var(--nai-line);
}

.trace-detail-toolbar {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex-wrap: wrap;
  flex: 0 0 auto;
  min-height: 26px;
  padding: 0 var(--nai-space-4);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
}

.trace-tabs {
  display: inline-flex;
  align-items: stretch;
  gap: 0;
}

.trace-tab {
  height: 25px;
  padding: 0 var(--nai-space-3);
  border: none;
  border-bottom: 2px solid transparent;
  border-radius: 0;
  background: transparent;
  color: var(--nai-dim);
  font-size: 9px;
  letter-spacing: 0.6px;
  text-transform: uppercase;
}

.trace-tab:hover {
  color: var(--nai-fg-soft);
  background: var(--nai-hover);
  border-bottom-color: var(--nai-line-strong);
}

.trace-tab--active {
  color: var(--nai-accent);
  border-bottom-color: var(--nai-accent);
  background: var(--nai-accent-bg);
}

.trace-counts {
  color: var(--nai-muted);
  font-size: 9px;
  letter-spacing: 0.3px;
  white-space: nowrap;
}

.panel-meta {
  color: var(--nai-dim);
  font-size: 9px;
  white-space: nowrap;
}

.toolbar-spacer {
  flex: 1 1 auto;
}

.trace-refresh {
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

.trace-refresh:hover:not(:disabled) {
  color: var(--nai-accent);
  border-color: var(--nai-accent);
  background: var(--nai-hover);
}

.trace-refresh:disabled {
  opacity: 0.45;
}

.trace-fallback {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-2);
  flex: 0 0 auto;
  padding: var(--nai-space-3) var(--nai-space-4);
  font-size: 10px;
  line-height: 1.5;
}

.trace-fallback strong {
  font-weight: 400;
  font-size: 10px;
  letter-spacing: 0.4px;
  text-transform: uppercase;
}

.trace-fallback--legacy {
  border-left: 2px solid var(--nai-warn);
  background: var(--nai-warn-bg);
  color: var(--nai-warn);
}

.trace-fallback--error {
  border-left: 2px solid var(--nai-bad);
  background: var(--nai-panel);
  color: var(--nai-bad);
}

.trace-fallback button {
  align-self: flex-start;
  height: 20px;
  padding: 0 var(--nai-space-4);
  border: var(--nai-hairline) solid var(--nai-bad);
  background: transparent;
  color: var(--nai-bad);
  font-size: 9px;
  letter-spacing: 0.6px;
  text-transform: uppercase;
}

.trace-diagnostics {
  display: flex;
  flex-direction: column;
  gap: 1px;
  padding: var(--nai-space-2) var(--nai-space-4);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  border-left: 2px solid var(--nai-warn);
  background: var(--nai-warn-bg);
  color: var(--nai-warn);
  font-size: 9px;
}

.trace-diagnostic--error {
  color: var(--nai-bad);
}

.trace-detail-body {
  min-height: 0;
}
</style>
