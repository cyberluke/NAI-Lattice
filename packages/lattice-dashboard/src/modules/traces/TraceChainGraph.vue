<script setup lang="ts">
import { computed, ref, shallowRef, watch } from 'vue';
import type { TraceGraph, TraceGraphNode, TraceSummary } from '../../query/traces';
import { formatDuration, formatTimestamp } from '../../ui/format';
import ChainEdgeChip from './ChainEdgeChip.vue';
import ChainNodeCard from './ChainNodeCard.vue';
import {
  buildChainLayout,
  countEdgeKind,
  legacyPointRows,
  linksIntoLayer,
  nodeKindLabel,
  nodeStatusLabel,
  nodeTone,
  type ChainLinkView,
  type ChainNodeView,
} from './chain';
import type { TraceGraphState } from './useTraceGraph';

const props = withDefaults(
  defineProps<{
    trace?: TraceSummary | null;
    graph?: TraceGraph | null;
    state?: TraceGraphState;
    legacyReason?: string | null;
    graphError?: string | null;
    focusSpanId?: string | null;
  }>(),
  {
    trace: null,
    graph: null,
    state: 'idle',
    legacyReason: null,
    graphError: null,
    focusSpanId: null,
  },
);

const emit = defineEmits<{
  (event: 'select-node', node: TraceGraphNode): void;
  (event: 'select-edge', edge: ChainLinkView['edge']): void;
  (event: 'select-span', spanId: string): void;
  (event: 'retry'): void;
}>();

const includeInferred = ref(false);
const selectedNode = shallowRef<TraceGraphNode | null>(null);
const selectedLink = shallowRef<ChainLinkView | null>(null);

const legacy = computed(
  () =>
    props.state === 'legacy' ||
    (props.graph !== null && props.graph.coverage === 'legacy'),
);

const ready = computed(
  () => !legacy.value && props.graph !== null && props.graph.coverage !== 'legacy',
);

const layout = computed(() =>
  ready.value && props.graph !== null
    ? buildChainLayout(props.graph, { includeInferred: includeInferred.value })
    : null,
);

const linearSteps = computed<Array<{ view: ChainNodeView; link: ChainLinkView | null }>>(() => {
  const value = layout.value;
  if (value === null) return [];
  const views = value.layers
    .map((layer) => layer[0])
    .filter((view) => view !== undefined);
  return views.map((view, index) => ({
    view,
    link:
      index < views.length - 1
        ? value.links.find((link) => link.from.node.id === view.node.id) ?? null
        : null,
  }));
});

const gaps = computed<ChainLinkView[][]>(() => {
  const value = layout.value;
  if (value === null) return [];
  return value.layers.slice(0, -1).map((_, index) => linksIntoLayer(value, index + 1));
});

const legacyRecords = computed(() => legacyPointRows(props.trace ?? null, props.graph));
const diagnostics = computed(() => props.graph?.diagnostics ?? []);
const responseEdges = computed(() => countEdgeKind(props.graph, 'response'));

const coverageText = computed(() => {
  const graph = props.graph;
  if (graph === null) return 'unknown';
  return graph.coverageDetail !== null
    ? `${graph.coverage} · ${graph.coverageDetail}`
    : graph.coverage;
});

const legacyReasonText = computed(
  () =>
    props.legacyReason ??
    'The host returned no schema-2 execution graph for this trace; only legacy request correlation exists.',
);

function clearSelection(): void {
  selectedNode.value = null;
  selectedLink.value = null;
}

function selectNode(view: ChainNodeView): void {
  if (selectedNode.value?.id === view.node.id) {
    clearSelection();
    return;
  }
  selectedNode.value = view.node;
  selectedLink.value = null;
  emit('select-node', view.node);
  if (view.node.spanId !== null) emit('select-span', view.node.spanId);
}

function selectLink(link: ChainLinkView): void {
  if (selectedLink.value?.edge.id === link.edge.id) {
    clearSelection();
    return;
  }
  selectedLink.value = link;
  selectedNode.value = null;
  emit('select-edge', link.edge);
}

function isNodeSelected(view: ChainNodeView): boolean {
  return selectedNode.value?.id === view.node.id;
}

function isLinkSelected(link: ChainLinkView): boolean {
  return selectedLink.value?.edge.id === link.edge.id;
}

function durationLabel(node: TraceGraphNode): string {
  return node.durationMs === null ? 'N/A' : formatDuration(node.durationMs);
}

watch(
  () => props.focusSpanId,
  (spanId) => {
    if (spanId === null || props.graph === null) return;
    const node = props.graph.nodes.find(
      (candidate) => candidate.spanId === spanId || candidate.id === spanId,
    );
    if (node !== undefined) {
      selectedNode.value = node;
      selectedLink.value = null;
    }
  },
  { immediate: true },
);

watch(
  () => props.graph,
  (graph) => {
    if (graph === null) {
      clearSelection();
      return;
    }
    if (
      selectedNode.value !== null &&
      !graph.nodes.some((node) => node.id === selectedNode.value?.id)
    ) {
      clearSelection();
    }
  },
);
</script>

<template>
  <div class="chain-surface">
    <div v-if="graphError !== null" class="chain-state chain-state--error" role="alert">
      <strong>CAUSAL CHAIN UNAVAILABLE — graph request failed</strong>
      <span>{{ graphError }}</span>
      <button type="button" @click="emit('retry')">RETRY</button>
    </div>

    <div
      v-else-if="graph === null && (state === 'loading' || state === 'idle')"
      class="chain-state"
      role="status"
    >
      <span>Resolving the schema-2 execution graph…</span>
      <span class="chain-state__muted">No chain is drawn until the host answers.</span>
    </div>

    <div v-else-if="legacy" class="chain-state chain-state--legacy" role="status">
      <strong>CAUSAL CHAIN UNAVAILABLE — legacy correlation only</strong>
      <span>{{ legacyReasonText }}</span>
      <span class="chain-state__muted">
        The records below are legacy request-correlation lifecycle events with no span id, parent
        edge or call observation. They are not spans and not calls.
      </span>
      <ul v-if="legacyRecords.length > 0" class="chain-legacy-events">
        <li
          v-for="record in legacyRecords"
          :key="record.key"
          class="chain-legacy-row"
        >
          <span class="chain-legacy-seq nai-numbers">seq {{ record.seq ?? '—' }}</span>
          <span class="chain-legacy-kind">{{ record.level }}</span>
          <span class="chain-legacy-component">{{ record.component ?? '—' }}</span>
          <span class="chain-legacy-label" :title="record.message">{{
            record.message || '—'
          }}</span>
          <span class="chain-legacy-ts nai-numbers">{{ formatTimestamp(record.ts) }}</span>
        </li>
      </ul>
      <span v-else class="chain-state__muted">
        No legacy correlation records are available for this trace either.
      </span>
    </div>

    <div v-else-if="!ready" class="chain-state">
      <span>Execution graph is unavailable.</span>
    </div>

    <template v-else-if="graph !== null && layout !== null">
      <div class="chain-toolbar">
        <span class="chain-coverage">coverage {{ coverageText }}</span>
        <div class="chain-legend" aria-label="Chain legend">
          <span class="legend-item"><i class="legend-dot legend-dot--observed" />observed</span>
          <span class="legend-item"><i class="legend-dot legend-dot--warn" />incomplete</span>
          <span class="legend-item"><i class="legend-dot legend-dot--bad" />failed</span>
          <span class="legend-item"><i class="legend-dot legend-dot--muted" />unknown / unresolved</span>
          <span class="legend-item legend-item--dash"
            ><i class="legend-dash" />inferred (off by default)</span
          >
        </div>
        <span class="toolbar-spacer" />
        <label class="inferred-toggle">
          <input v-model="includeInferred" type="checkbox" />
          show inferred
        </label>
      </div>

      <div v-if="diagnostics.length > 0" class="chain-diagnostics" role="status">
        <span
          v-for="diagnostic in diagnostics"
          :key="`${diagnostic.code}-${diagnostic.message}`"
          class="chain-diagnostic"
          :class="`chain-diagnostic--${diagnostic.severity.toLowerCase()}`"
        >
          {{ diagnostic.severity }} · {{ diagnostic.code }}: {{ diagnostic.message }}
        </span>
      </div>

      <div v-if="graph.nodes.length === 0" class="chain-state">
        <span>The graph response contains no nodes.</span>
        <span class="chain-state__muted">No chain is invented from an empty graph.</span>
      </div>

      <div v-else class="chain-body">
        <div v-if="layout.mode === 'linear'" class="chain-breadcrumb" role="list">
          <template v-for="(step, index) in linearSteps" :key="step.view.node.id">
            <ChainNodeCard
              :view="step.view"
              :selected="isNodeSelected(step.view)"
              @select="selectNode(step.view)"
            />
            <div
              v-if="index < linearSteps.length - 1"
              class="chain-arrow"
              role="listitem"
              aria-hidden="false"
            >
              <span class="chain-arrow__mark" aria-hidden="true">→</span>
              <ChainEdgeChip
                v-if="step.link !== null"
                :link="step.link"
                :selected="isLinkSelected(step.link)"
                @select="selectLink(step.link)"
              />
              <span v-else class="chain-arrow__missing">no recorded edge</span>
            </div>
          </template>
        </div>

        <div v-else class="chain-layers" role="list">
          <template v-for="(layer, layerIndex) in layout.layers" :key="`layer-${layerIndex}`">
            <div class="chain-layer" role="listitem">
              <span class="chain-layer__index">L{{ layerIndex }}</span>
              <ChainNodeCard
                v-for="view in layer"
                :key="view.node.id"
                :view="view"
                :selected="isNodeSelected(view)"
                @select="selectNode(view)"
              />
            </div>
            <div v-if="layerIndex < layout.layers.length - 1" class="chain-gap">
              <ChainEdgeChip
                v-for="link in gaps[layerIndex] ?? []"
                :key="link.edge.id"
                :link="link"
                :selected="isLinkSelected(link)"
                @select="selectLink(link)"
              />
              <span v-if="(gaps[layerIndex] ?? []).length === 0" class="chain-gap__empty"
                >no recorded edge</span
              >
            </div>
          </template>
        </div>

        <div class="chain-notes">
          <span v-if="layout.note === 'parent'" class="tone--warn">
            no call edges were recorded; this chain uses the explicit parent hierarchy.
          </span>
          <span v-if="layout.note === 'none'" class="tone--warn">
            no call or parent edges were recorded; observed nodes are listed without an invented
            chain.
          </span>
          <span v-if="layout.augmentedParents > 0">
            {{ layout.augmentedParents }} explicit parent edge{{
              layout.augmentedParents === 1 ? '' : 's'
            }}
            fill{{ layout.augmentedParents === 1 ? 's' : '' }} a gap where no call observation
            exists.
          </span>
          <span v-if="layout.inferredHidden > 0">
            {{ layout.inferredHidden }} inferred edge{{ layout.inferredHidden === 1 ? '' : 's' }}
            hidden.
          </span>
          <span v-if="layout.droppedEdges > 0" class="tone--warn">
            {{ layout.droppedEdges }} edge{{ layout.droppedEdges === 1 ? '' : 's' }} reference a
            node the graph did not include and are not drawn.
          </span>
          <span v-if="layout.cycleCount > 0" class="tone--warn">
            {{ layout.cycleCount }} node{{ layout.cycleCount === 1 ? '' : 's' }} participate in a
            cycle; layering stops at the repeated node.
          </span>
          <span v-if="responseEdges > 0">
            {{ responseEdges }} response observation{{ responseEdges === 1 ? '' : 's' }} retained;
            not drawn as a second hop for the same call.
          </span>
        </div>

        <div v-if="selectedNode !== null" class="chain-inspector" role="status">
          <div class="chain-inspector__head">
            <span class="chain-inspector__title">span inspector</span>
            <span class="chain-inspector__status" :class="`tone--${nodeTone(selectedNode)}`">{{
              nodeStatusLabel(selectedNode)
            }}</span>
            <span class="chain-inspector__kind">{{ nodeKindLabel(selectedNode) }}</span>
            <span class="toolbar-spacer" />
            <button type="button" @click="clearSelection">CLOSE</button>
          </div>
          <dl class="chain-fields">
            <dt>component</dt>
            <dd>{{ selectedNode.component ?? '—' }}</dd>
            <dt>operation</dt>
            <dd>{{ selectedNode.operation || '—' }}</dd>
            <dt>instance</dt>
            <dd>{{ selectedNode.instanceId ?? '—' }}</dd>
            <dt>status</dt>
            <dd>{{ nodeStatusLabel(selectedNode) }}</dd>
            <dt>duration</dt>
            <dd class="nai-numbers">{{ durationLabel(selectedNode) }}</dd>
            <dt>start</dt>
            <dd class="nai-numbers">{{ formatTimestamp(selectedNode.startTs) }}</dd>
            <dt>parent span</dt>
            <dd>{{ selectedNode.parentSpanId ?? '—' }}</dd>
            <dt>span id</dt>
            <dd>{{ selectedNode.spanId ?? '—' }}</dd>
            <dt>evidence</dt>
            <dd class="evidence-chips">
              <span
                v-for="sequence in selectedNode.evidenceSeqs"
                :key="`node-seq-${sequence}`"
                class="evidence-chip nai-numbers"
                >seq {{ sequence }}</span
              >
              <span v-if="selectedNode.evidenceSeqs.length === 0" class="evidence-none"
                >no evidence sequence recorded</span
              >
            </dd>
          </dl>
          <div v-if="selectedNode.kind === 'unresolved'" class="chain-unresolved">
            unresolved endpoint: {{ selectedNode.reason ?? 'no reason was reported' }}
          </div>
        </div>

        <div v-else-if="selectedLink !== null" class="chain-inspector" role="status">
          <div class="chain-inspector__head">
            <span class="chain-inspector__title">edge evidence</span>
            <span class="chain-inspector__status tone--observed">{{
              selectedLink.edge.kind.toUpperCase()
            }}</span>
            <span class="chain-inspector__kind">{{
              selectedLink.edge.inferred ? 'INFERRED' : 'EXPLICIT'
            }}</span>
            <span class="toolbar-spacer" />
            <button type="button" @click="clearSelection">CLOSE</button>
          </div>
          <dl class="chain-fields">
            <dt>direction</dt>
            <dd>
              {{ selectedLink.from.node.component ?? '—' }}
              <span aria-hidden="true">→</span>
              {{ selectedLink.to.node.component ?? '—' }}
            </dd>
            <dt>operations</dt>
            <dd>
              {{ selectedLink.from.node.operation || '—' }}
              <span aria-hidden="true">→</span>
              {{ selectedLink.to.node.operation || '—' }}
            </dd>
            <dt>operation</dt>
            <dd>{{ selectedLink.edge.operation ?? '—' }}</dd>
            <dt>transport</dt>
            <dd>{{ selectedLink.edge.transport ?? '—' }}</dd>
            <dt>latency</dt>
            <dd class="nai-numbers">
              {{
                selectedLink.edge.latencyMs === null
                  ? 'N/A'
                  : formatDuration(selectedLink.edge.latencyMs)
              }}
            </dd>
            <dt>evidence</dt>
            <dd class="evidence-chips">
              <span
                v-for="sequence in selectedLink.edge.evidenceSeqs"
                :key="`edge-seq-${sequence}`"
                class="evidence-chip nai-numbers"
                >seq {{ sequence }}</span
              >
              <span v-if="selectedLink.edge.evidenceSeqs.length === 0" class="evidence-none"
                >no evidence sequence recorded</span
              >
            </dd>
          </dl>
        </div>
      </div>
    </template>
  </div>
</template>

<style scoped>
.chain-surface {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-3);
  min-height: 0;
  padding: var(--nai-space-3) var(--nai-space-4) var(--nai-space-4);
  background: var(--nai-panel-2);
  font-size: var(--nai-font-size-sm);
}

.chain-state {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-2);
  padding: var(--nai-space-4);
  color: var(--nai-muted);
  font-size: 10px;
  line-height: 1.55;
}

.chain-state strong {
  color: var(--nai-fg);
  font-weight: 400;
  font-size: 10px;
  letter-spacing: 0.4px;
  text-transform: uppercase;
}

.chain-state--legacy strong {
  color: var(--nai-warn);
}

.chain-state--error {
  border-left: 2px solid var(--nai-bad);
  color: var(--nai-bad);
}

.chain-state--error strong {
  color: var(--nai-bad);
}

.chain-state__muted {
  color: var(--nai-dim);
}

.chain-state button,
.chain-inspector button {
  align-self: flex-start;
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

.chain-state button:hover,
.chain-inspector button:hover {
  color: var(--nai-accent);
  border-color: var(--nai-accent);
  background: var(--nai-hover);
}

.chain-toolbar {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex-wrap: wrap;
  flex: 0 0 auto;
}

.chain-coverage {
  color: var(--nai-dim);
  font-size: 9px;
  letter-spacing: 0.4px;
  text-transform: uppercase;
}

.chain-legend {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex-wrap: wrap;
  font-size: 9px;
  color: var(--nai-dim);
}

.legend-item {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}

.legend-dot {
  width: 6px;
  height: 6px;
  display: inline-block;
}

.legend-dot--observed {
  background: var(--nai-accent);
}

.legend-dot--warn {
  background: var(--nai-warn);
}

.legend-dot--bad {
  background: var(--nai-bad);
}

.legend-dot--muted {
  background: var(--nai-dim);
}

.legend-dash {
  width: 14px;
  height: 0;
  display: inline-block;
  border-top: var(--nai-hairline) dashed var(--nai-warn);
}

.inferred-toggle {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  color: var(--nai-muted);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.4px;
}

.inferred-toggle input {
  accent-color: var(--nai-accent);
}

.chain-diagnostics {
  display: flex;
  flex-direction: column;
  gap: 1px;
  padding: var(--nai-space-2) var(--nai-space-3);
  border-left: 2px solid var(--nai-warn);
  background: var(--nai-warn-bg);
  font-size: 9px;
  color: var(--nai-warn);
}

.chain-diagnostic--error {
  color: var(--nai-bad);
}

.chain-body {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-3);
  min-height: 0;
}

.chain-breadcrumb {
  display: flex;
  align-items: center;
  gap: var(--nai-space-3);
  overflow-x: auto;
  padding-bottom: var(--nai-space-2);
}

.chain-arrow {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 2px;
  flex: 0 0 auto;
}

.chain-arrow__mark {
  color: var(--nai-accent);
  font-size: 13px;
  line-height: 1;
}

.chain-arrow__missing {
  color: var(--nai-dim);
  font-size: 8px;
  white-space: nowrap;
}

.chain-layers {
  display: flex;
  align-items: stretch;
  gap: var(--nai-space-2);
  overflow-x: auto;
  padding-bottom: var(--nai-space-2);
}

.chain-layer {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-3);
  flex: 0 0 auto;
  min-width: 178px;
}

.chain-layer__index {
  color: var(--nai-dim);
  font-size: 8px;
  letter-spacing: 0.5px;
}

.chain-gap {
  display: flex;
  flex-direction: column;
  justify-content: center;
  gap: var(--nai-space-2);
  flex: 0 0 auto;
  min-width: 168px;
  padding: 0 var(--nai-space-2);
  border-left: var(--nai-hairline) solid var(--nai-row-line);
  border-right: var(--nai-hairline) solid var(--nai-row-line);
}

.chain-gap__empty {
  color: var(--nai-dim);
  font-size: 8px;
  text-align: center;
}

.chain-notes {
  display: flex;
  flex-wrap: wrap;
  gap: var(--nai-space-4);
  padding-top: var(--nai-space-2);
  border-top: var(--nai-hairline) solid var(--nai-row-line);
  color: var(--nai-dim);
  font-size: 9px;
}

.chain-inspector {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-2);
  padding: var(--nai-space-3) var(--nai-space-4);
  border-top: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
}

.chain-inspector__head {
  display: flex;
  align-items: center;
  gap: var(--nai-space-3);
}

.chain-inspector__title {
  color: var(--nai-accent);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.6px;
}

.chain-inspector__kind {
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
}

.chain-inspector__status {
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.4px;
}

.chain-fields {
  display: grid;
  grid-template-columns: 82px minmax(0, 1fr);
  gap: 1px var(--nai-space-4);
  margin: 0;
  font-size: 10px;
}

.chain-fields dt {
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.3px;
}

.chain-fields dd {
  margin: 0;
  color: var(--nai-fg-soft);
  overflow-wrap: anywhere;
}

.evidence-chips {
  display: flex;
  flex-wrap: wrap;
  gap: var(--nai-space-2);
}

.evidence-chip {
  padding: 0 var(--nai-space-2);
  border: var(--nai-hairline) solid var(--nai-line-strong);
  background: var(--nai-accent-bg);
  color: var(--nai-accent);
  font-size: 9px;
}

.evidence-none {
  color: var(--nai-dim);
  font-size: 9px;
}

.chain-unresolved {
  border-left: 2px solid var(--nai-warn);
  padding-left: var(--nai-space-3);
  color: var(--nai-warn);
  font-size: 9px;
}

.chain-legacy-events {
  list-style: none;
  margin: var(--nai-space-2) 0 0;
  padding: 0;
  border-top: var(--nai-hairline) solid var(--nai-line);
}

.chain-legacy-row {
  display: grid;
  grid-template-columns: 64px 60px 120px minmax(0, 1fr) 84px;
  gap: var(--nai-space-3);
  align-items: baseline;
  padding: 2px 0;
  border-bottom: 1px dotted var(--nai-bar-track);
  color: var(--nai-muted);
  font-size: 10px;
}

.chain-legacy-seq,
.chain-legacy-ts {
  color: var(--nai-dim);
  text-align: right;
}

.chain-legacy-kind {
  color: var(--nai-accent);
  font-size: 9px;
  text-transform: uppercase;
}

.chain-legacy-component,
.chain-legacy-label {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.tone--observed {
  color: var(--nai-accent);
}

.tone--warn {
  color: var(--nai-warn);
}

.tone--bad {
  color: var(--nai-bad);
}

.tone--muted {
  color: var(--nai-dim);
}

.toolbar-spacer {
  flex: 1 1 auto;
}
</style>
