<script setup lang="ts">
import { computed } from 'vue';
import { formatDuration } from '../../ui/format';
import type { ChainLinkView } from './chain';

const props = defineProps<{
  link: ChainLinkView;
  selected: boolean;
}>();

const emit = defineEmits<{ (event: 'select'): void }>();

const latency = computed(() =>
  props.link.edge.latencyMs === null ? 'latency N/A' : formatDuration(props.link.edge.latencyMs),
);
const evidence = computed(() => {
  const count = props.link.edge.evidenceSeqs.length;
  return count === 0 ? 'no evidence' : `ev ${count}`;
});
const title = computed(() => {
  const edge = props.link.edge;
  return [
    `${props.link.from.node.component ?? '—'} → ${props.link.to.node.component ?? '—'}`,
    edge.kind,
    edge.operation ?? 'operation not recorded',
    edge.transport ?? 'transport not recorded',
    latency.value,
    edge.inferred ? 'inferred correlation' : 'explicit observation',
    evidence.value,
  ].join(' · ');
});
</script>

<template>
  <button
    type="button"
    class="chain-edge"
    :class="{
      'chain-edge--inferred': link.edge.inferred,
      'chain-edge--selected': selected,
    }"
    :title="title"
    @click="emit('select')"
  >
    <span class="chain-edge__kind">
      {{ link.edge.kind.toUpperCase() }}<template v-if="link.edge.inferred"> · INFERRED</template>
    </span>
    <span class="chain-edge__operation">{{
      link.edge.operation ?? 'operation not recorded'
    }}</span>
    <span class="chain-edge__transport">{{
      link.edge.transport ?? 'transport not recorded'
    }}</span>
    <span class="chain-edge__facts">
      <span class="chain-edge__latency nai-numbers">{{ latency }}</span>
      <span class="chain-edge__evidence nai-numbers">{{ evidence }}</span>
    </span>
  </button>
</template>

<style scoped>
.chain-edge {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 160px;
  max-width: 230px;
  padding: var(--nai-space-2) var(--nai-space-3);
  border: var(--nai-hairline) solid var(--nai-line-strong);
  border-radius: 0;
  background: var(--nai-panel-2);
  color: var(--nai-muted);
  text-align: left;
  text-transform: none;
  letter-spacing: normal;
}

.chain-edge:hover,
.chain-edge--selected {
  border-color: var(--nai-accent);
  background: var(--nai-hover);
  color: var(--nai-fg);
}

.chain-edge__kind {
  color: var(--nai-accent);
  font-size: 8px;
  letter-spacing: 0.5px;
}

.chain-edge__operation {
  color: var(--nai-fg-soft);
  font-size: 10px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.chain-edge__transport {
  color: var(--nai-dim);
  font-size: 9px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.chain-edge__facts {
  display: flex;
  justify-content: space-between;
  gap: var(--nai-space-3);
  font-size: 9px;
}

.chain-edge__latency,
.chain-edge__evidence {
  color: var(--nai-dim);
}

.chain-edge--inferred {
  border-color: var(--nai-warn);
  border-style: dashed;
}

.chain-edge--inferred .chain-edge__kind {
  color: var(--nai-warn);
}
</style>
