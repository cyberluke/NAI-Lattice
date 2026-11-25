<script setup lang="ts">
import { computed } from 'vue';
import type { TraceGraph } from '../../query/traces';
import { formatDuration } from '../../ui/format';
import {
  buildCallTreeRows,
  nodeKindLabel,
  nodeStatusLabel,
  nodeTone,
} from './chain';

const props = defineProps<{ graph: TraceGraph | null }>();

const usable = computed(
  () => props.graph !== null && props.graph.coverage !== 'legacy',
);
const result = computed(() =>
  usable.value && props.graph !== null ? buildCallTreeRows(props.graph) : null,
);
const rows = computed(() => result.value?.rows ?? []);
const noParentEdges = computed(
  () => result.value !== null && result.value.parentEdges === 0,
);
</script>

<template>
  <div class="call-tree">
    <div v-if="!usable" class="call-tree-state">
      <span>No schema-2 parent edges are available.</span>
      <span class="muted">
        Legacy correlation has no parent hierarchy; the Waterfall tab lists its point records.
      </span>
    </div>
    <template v-else-if="result !== null">
      <div class="call-tree-summary nai-numbers">
        {{ rows.length }} nodes · {{ result.parentEdges }} parent edges · {{ result.roots }} roots
        <span v-if="result.cyclic > 0" class="tone--warn">· {{ result.cyclic }} cycle rows</span>
        <span v-if="result.missingParents > 0" class="tone--warn">
          · {{ result.missingParents }} missing parents
        </span>
      </div>
      <div v-if="noParentEdges" class="call-tree-note tone--warn">
        No parent edges were recorded; spans are listed without an invented hierarchy.
      </div>
      <div v-else-if="result.missingParents > 0" class="call-tree-note tone--warn">
        {{ result.missingParents }} parent edge{{ result.missingParents === 1 ? '' : 's' }}
        reference a node the graph did not include; those nodes are roots here.
      </div>
      <ol v-if="rows.length > 0" class="call-tree-rows">
        <li
          v-for="row in rows"
          :key="row.node.id"
          class="call-tree-row"
          :class="{
            'call-tree-row--unresolved': row.node.kind === 'unresolved',
            'call-tree-row--cycle': row.relation === 'cycle',
          }"
          :data-depth="row.depth"
        >
          <div class="call-tree-line" :style="{ paddingLeft: `${row.depth * 14}px` }">
            <span class="call-tree-branch" aria-hidden="true">{{
              row.depth === 0 ? '●' : '└'
            }}</span>
            <span class="call-tree-kind">{{ nodeKindLabel(row.node) }}</span>
            <span class="call-tree-component" :title="row.node.component ?? 'component not recorded'">{{
              row.node.component ?? '—'
            }}</span>
            <span class="call-tree-operation" :title="row.node.operation">{{
              row.node.operation || '(operation not recorded)'
            }}</span>
            <span class="call-tree-status" :class="`tone--${nodeTone(row.node)}`">{{
              nodeStatusLabel(row.node)
            }}</span>
            <span class="call-tree-duration nai-numbers">{{
              row.node.durationMs === null ? 'N/A' : formatDuration(row.node.durationMs)
            }}</span>
          </div>
          <div v-if="row.note !== null" class="call-tree-row-note">{{ row.note }}</div>
          <div v-if="row.node.kind === 'unresolved'" class="call-tree-row-note">
            unresolved endpoint: {{ row.node.reason ?? 'no reason was reported' }}
          </div>
        </li>
      </ol>
      <div v-else class="call-tree-state">The graph contains no nodes to list.</div>
    </template>
  </div>
</template>

<style scoped>
.call-tree {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-2);
  padding: var(--nai-space-3) var(--nai-space-4);
  background: var(--nai-panel-2);
  font-size: 10px;
  min-height: 0;
}

.call-tree-state {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-2);
  padding: var(--nai-space-4);
  color: var(--nai-muted);
  font-size: 10px;
}

.call-tree-state .muted {
  color: var(--nai-dim);
}

.call-tree-summary {
  color: var(--nai-dim);
  font-size: 9px;
}

.call-tree-note {
  padding-left: var(--nai-space-3);
  border-left: 2px solid var(--nai-warn);
  font-size: 9px;
}

.call-tree-rows {
  list-style: none;
  margin: 0;
  padding: 0;
  overflow-x: auto;
}

.call-tree-row {
  border-bottom: 1px dotted var(--nai-bar-track);
  padding: 1px 0;
}

.call-tree-line {
  display: grid;
  grid-template-columns: 14px 54px minmax(110px, 0.9fr) minmax(140px, 1.4fr) 78px 60px;
  gap: var(--nai-space-3);
  align-items: baseline;
  min-width: 620px;
}

.call-tree-branch {
  color: var(--nai-dim);
  font-size: 9px;
}

.call-tree-kind {
  color: var(--nai-accent);
  font-size: 8px;
  letter-spacing: 0.4px;
}

.call-tree-component,
.call-tree-operation {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.call-tree-component {
  color: var(--nai-muted);
}

.call-tree-operation {
  color: var(--nai-fg-soft);
}

.call-tree-status {
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.3px;
}

.call-tree-duration {
  color: var(--nai-muted);
  text-align: right;
}

.call-tree-row-note {
  color: var(--nai-warn);
  font-size: 9px;
  padding-left: calc(14px + var(--nai-space-3));
}

.call-tree-row--unresolved .call-tree-component,
.call-tree-row--unresolved .call-tree-operation {
  color: var(--nai-dim);
}

.call-tree-row--cycle {
  border-left: 2px solid var(--nai-warn);
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
</style>
