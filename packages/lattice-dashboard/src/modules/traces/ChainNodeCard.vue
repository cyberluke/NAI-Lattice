<script setup lang="ts">
import { computed } from 'vue';
import { formatDuration } from '../../ui/format';
import {
  nodeKindLabel,
  nodeStatusLabel,
  nodeTone,
  type ChainNodeView,
} from './chain';

const props = defineProps<{
  view: ChainNodeView;
  selected: boolean;
}>();

const emit = defineEmits<{ (event: 'select'): void }>();

const tone = computed(() => nodeTone(props.view.node));
const duration = computed(() =>
  props.view.node.durationMs === null ? 'N/A' : formatDuration(props.view.node.durationMs),
);
const title = computed(() => {
  const node = props.view.node;
  return [
    node.component ?? 'component not recorded',
    node.operation || 'operation not recorded',
    nodeStatusLabel(node),
    `duration ${duration.value}`,
    node.spanId === null ? 'span id not recorded' : `span ${node.spanId}`,
  ].join(' · ');
});
</script>

<template>
  <button
    type="button"
    class="chain-node"
    :class="[
      `tone--${tone}`,
      {
        'chain-node--selected': selected,
        'chain-node--unresolved': view.node.kind === 'unresolved',
        'chain-node--trace': view.node.kind === 'trace',
      },
    ]"
    :title="title"
    @click="emit('select')"
  >
    <span class="chain-node__kind">{{ nodeKindLabel(view.node) }}</span>
    <strong class="chain-node__component">{{ view.node.component ?? '—' }}</strong>
    <span class="chain-node__operation">{{
      view.node.operation || '(operation not recorded)'
    }}</span>
    <span class="chain-node__facts">
      <span class="chain-node__status">{{ nodeStatusLabel(view.node) }}</span>
      <span class="chain-node__duration nai-numbers">{{ duration }}</span>
    </span>
    <span v-if="view.node.instanceId !== null" class="chain-node__instance">{{
      view.node.instanceId
    }}</span>
    <span v-if="view.node.kind === 'unresolved'" class="chain-node__reason">{{
      view.node.reason ?? 'endpoint was not observed in the ledger'
    }}</span>
    <span v-if="view.cycle" class="chain-node__cycle">cycle</span>
  </button>
</template>

<style scoped>
.chain-node {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 170px;
  max-width: 240px;
  padding: var(--nai-space-3) var(--nai-space-4);
  border: var(--nai-hairline) solid var(--nai-line-strong);
  border-left-width: 2px;
  border-radius: 0;
  background: var(--nai-panel-3);
  color: var(--nai-fg-soft);
  text-align: left;
  text-transform: none;
  letter-spacing: normal;
}

.chain-node:hover {
  background: var(--nai-hover);
  border-color: var(--nai-accent);
}

.chain-node--selected {
  outline: var(--nai-hairline) solid var(--nai-accent);
  outline-offset: -1px;
  background: var(--nai-selected);
}

.chain-node__kind {
  color: var(--nai-dim);
  font-size: 8px;
  letter-spacing: 0.6px;
  text-transform: uppercase;
}

.chain-node__component {
  color: var(--nai-fg);
  font-weight: 400;
  font-size: var(--nai-font-size-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.chain-node__operation {
  color: var(--nai-muted);
  font-size: 10px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.chain-node__facts {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: var(--nai-space-3);
  font-size: 9px;
}

.chain-node__status {
  text-transform: uppercase;
  letter-spacing: 0.4px;
}

.chain-node__duration {
  color: var(--nai-muted);
}

.chain-node__instance,
.chain-node__cycle {
  color: var(--nai-dim);
  font-size: 8px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.chain-node--unresolved {
  border-style: dashed;
  background: var(--nai-panel-2);
}

.chain-node--unresolved .chain-node__component,
.chain-node--unresolved .chain-node__operation {
  color: var(--nai-dim);
}

.chain-node__reason {
  color: var(--nai-warn);
  font-size: 9px;
  white-space: normal;
}

.tone--observed {
  border-left-color: var(--nai-accent);
}

.tone--observed .chain-node__status {
  color: var(--nai-accent);
}

.tone--warn {
  border-left-color: var(--nai-warn);
}

.tone--warn .chain-node__status {
  color: var(--nai-warn);
}

.tone--bad {
  border-left-color: var(--nai-bad);
}

.tone--bad .chain-node__status {
  color: var(--nai-bad);
}

.tone--muted {
  border-left-color: var(--nai-dim);
}

.tone--muted .chain-node__status {
  color: var(--nai-dim);
}
</style>
