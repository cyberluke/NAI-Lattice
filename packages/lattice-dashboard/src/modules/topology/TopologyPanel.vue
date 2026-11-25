<script setup lang="ts">
import { computed, inject, onBeforeUnmount, onMounted, ref, shallowRef } from 'vue';
import { APP_CONTEXT_KEY } from '../../app-context';
import type { LatticeChartOption, LatticeChartPick } from '../../charts/echarts';
import { useNow } from '../../shell/useNow';
import {
  buildTopologyOption,
  interpretTopologyPick,
  TopologyStore,
  type TopologyEdge,
  type TopologySnapshot,
} from '../../topology/topology';
import LazyChart from '../../ui/LazyChart.vue';
import { formatAge, formatCount } from '../../ui/format';

defineProps<{ instanceId?: string }>();

const context = inject(APP_CONTEXT_KEY);
if (context === undefined) throw new Error('topology context is missing');

const store = new TopologyStore();
const investigation = context.investigation;
const now = useNow();

const snapshot = store.snapshot;
const error = store.error;
const stale = store.stale;
const loading = store.loading;
const unsupported = store.unsupported;
const lastFetchedAt = store.lastFetchedAt;

const selection = shallowRef<TopologyEdge | null>(null);
const filterNotice = ref<string | null>(null);

const age = computed(() => formatAge(lastFetchedAt.value, now.value));
const nodeCount = computed(() => snapshot.value?.nodes.length ?? 0);
const edgeCount = computed(() => snapshot.value?.edges.length ?? 0);
const isEmpty = computed(
  () => snapshot.value !== null && snapshot.value.nodes.length === 0,
);

/**
 * The host polls every 5 s. Rebuilding the chart for an unchanged graph makes
 * ECharts replace its option (and reset any user roam) for no new evidence.
 * Fingerprint the real graph before rebuilding; ordering is id-stable.
 */
function topologySignature(value: TopologySnapshot): string {
  const nodes = [...value.nodes]
    .sort((a, b) => a.id.localeCompare(b.id))
    .map((node) => [node.id, node.label, node.count, node.errors]);
  const edges = [...value.edges]
    .sort((a, b) => {
      const bySource = a.source.localeCompare(b.source);
      return bySource !== 0 ? bySource : a.target.localeCompare(b.target);
    })
    .map((edge) => [edge.source, edge.target, edge.count]);
  return JSON.stringify([nodes, edges, value.truncated]);
}

let lastSignature = '';
let lastOption: LatticeChartOption | null = null;

const chartOption = computed<LatticeChartOption | null>(() => {
  const current = snapshot.value;
  if (current === null) {
    lastSignature = '';
    lastOption = null;
    return null;
  }
  const signature = topologySignature(current);
  if (signature === lastSignature && lastOption !== null) return lastOption;
  lastSignature = signature;
  lastOption = buildTopologyOption(current);
  return lastOption;
});

function refresh(): void {
  void store.refresh();
}

function onPick(pick: LatticeChartPick): void {
  const interpreted = interpretTopologyPick(pick);
  if (interpreted.kind === 'node') {
    selection.value = null;
    filterNotice.value = `Log Tail filtered by component ${interpreted.component}`;
    investigation.setComponent(interpreted.component);
    investigation.requestPanel('log-tail');
    return;
  }
  if (interpreted.kind === 'edge') {
    selection.value = {
      source: interpreted.source,
      target: interpreted.target,
      count: interpreted.count,
    };
    filterNotice.value = null;
  }
}

onMounted(() => store.start());
onBeforeUnmount(() => store.dispose());
</script>

<template>
  <section class="topology-panel" aria-label="Component topology derived from trace spans">
    <div class="panel-toolbar">
      <span class="panel-title">component topology</span>
      <span class="panel-meta nai-numbers">
        window {{ formatCount(snapshot?.window ?? null) }} ·
        {{ formatCount(nodeCount) }} nodes · {{ formatCount(edgeCount) }} edges
      </span>
      <span v-if="snapshot?.truncated" class="tone--warn">truncated at the host cap</span>
      <span class="toolbar-spacer" />
      <span v-if="stale" class="tone--warn">stale · {{ age }}</span>
      <span v-else-if="error !== null" class="tone--bad" :title="error">topology error</span>
      <span v-else class="panel-meta">measured {{ age }}</span>
      <button type="button" :disabled="loading" @click="refresh">REFRESH</button>
    </div>

    <div v-if="unsupported" class="panel-state panel-state--unsupported" role="status">
      <strong>topology not wired</strong>
      <span>
        This host does not serve <code>/lattice/api/topology</code> (HTTP 404). No component or
        edge is invented here.
      </span>
    </div>
    <div
      v-else-if="snapshot === null && error !== null"
      class="panel-state panel-state--error"
      role="alert"
    >
      <span>{{ error }}</span>
      <button type="button" @click="refresh">RETRY</button>
    </div>
    <div v-else-if="snapshot === null" class="panel-state">
      <span v-if="loading">Loading the bounded topology window…</span>
      <span v-else>Topology is unavailable.</span>
    </div>
    <div v-else-if="isEmpty" class="panel-state" role="status">
      <span>no component-bearing traces in the window</span>
      <span class="muted">
        The host examined {{ formatCount(snapshot.window) }} recent records and found no consecutive
        component transitions. No graph is fabricated.
      </span>
    </div>

    <div v-else class="topology-body">
      <div class="graph-surface">
        <LazyChart
          v-if="chartOption !== null"
          :option="chartOption"
          aria-label="Component topology graph"
          :on-pick="onPick"
        />
      </div>
      <div v-if="filterNotice !== null" class="selection-bar selection-bar--info" role="status">
        {{ filterNotice }}
      </div>
      <div v-else-if="selection !== null" class="selection-bar" role="status">
        <span class="selection-title">edge</span>
        <strong>{{ selection.source }}</strong>
        <span aria-hidden="true">→</span>
        <strong>{{ selection.target }}</strong>
        <span class="nai-numbers">
          {{ selection.count === null ? 'count not reported' : `${formatCount(selection.count)} transitions` }}
        </span>
        <span class="muted">host-derived count only; no edge detail exists</span>
        <span class="toolbar-spacer" />
        <button type="button" @click="selection = null">CLOSE</button>
      </div>
      <div class="panel-footer">
        <span>Node size ≈ host span count, red = errors. Click a node to filter the Log Tail.</span>
        <span v-if="!snapshot.truncated">complete bounded window</span>
        <span v-else class="tone--warn">the window was truncated by the host</span>
      </div>
    </div>
  </section>
</template>

<style scoped>
.topology-panel {
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
  gap: var(--nai-space-3);
  flex: 1 1 auto;
  padding: var(--nai-space-5);
  color: var(--nai-muted);
  font-size: var(--nai-font-size-sm);
  text-align: center;
}

.panel-state .muted {
  max-width: 620px;
  color: var(--nai-dim);
  font-size: 10px;
  line-height: 1.5;
}

.panel-state--error {
  color: var(--nai-bad);
}

.panel-state--unsupported strong {
  color: var(--nai-fg);
  font-weight: 400;
  text-transform: uppercase;
  letter-spacing: 0.6px;
  font-size: 10px;
}

.topology-body {
  display: flex;
  flex-direction: column;
  flex: 1 1 auto;
  min-height: 0;
}

.graph-surface {
  position: relative;
  flex: 1 1 auto;
  min-height: 160px;
  overflow: hidden;
}

.selection-bar {
  display: flex;
  align-items: center;
  gap: var(--nai-space-3);
  flex: 0 0 auto;
  padding: var(--nai-space-2) var(--nai-space-4);
  border-top: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
  color: var(--nai-fg-soft);
  font-size: 10px;
}

.selection-bar strong {
  color: var(--nai-fg);
  font-weight: 400;
}

.selection-bar--info {
  color: var(--nai-accent);
  border-left: 2px solid var(--nai-accent);
  background: var(--nai-accent-bg);
}

.selection-title {
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.selection-bar .muted {
  color: var(--nai-dim);
  font-size: 9px;
}

.panel-footer {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex: 0 0 auto;
  min-height: 24px;
  padding: 0 var(--nai-space-4);
  border-top: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
  color: var(--nai-dim);
  font-size: 9px;
}

.panel-footer span:first-child {
  flex: 1 1 auto;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

code {
  color: var(--nai-search);
  font-family: inherit;
}

.tone--warn {
  color: var(--nai-warn);
}

.tone--bad {
  color: var(--nai-bad);
}
</style>
