<script setup lang="ts">
import { computed } from 'vue';
import type { TraceGraph, TraceSummary } from '../../query/traces';
import { formatTimestamp, shortId } from '../../ui/format';
import { graphPointEvents, legacyPointRows } from './chain';

const props = withDefaults(
  defineProps<{
    trace: TraceSummary;
    graph?: TraceGraph | null;
  }>(),
  { graph: null },
);

const emit = defineEmits<{ (event: 'select-span', spanId: string): void }>();

interface EventRow {
  key: string;
  seq: number | null;
  ts: number | null;
  level: string;
  component: string | null;
  message: string;
  spanId: string | null;
  legacy: boolean;
}

const usable = computed(
  () => props.graph !== null && props.graph.coverage !== 'legacy',
);

const rows = computed<EventRow[]>(() => {
  if (usable.value && props.graph !== null) {
    return graphPointEvents(props.graph).map((event) => ({
      key: `graph:${event.id}`,
      seq: event.seq,
      ts: event.ts,
      level: event.level,
      component: event.component,
      message: event.message || '(event message not recorded)',
      spanId: event.spanId,
      legacy: false,
    }));
  }
  return legacyPointRows(props.trace, props.graph).map((record) => ({
    key: record.key,
    seq: record.seq,
    ts: record.ts,
    level: record.level,
    component: record.component,
    message: record.message || '(label not recorded)',
    spanId: null,
    legacy: true,
  }));
});

function selectSpan(row: EventRow): void {
  if (row.spanId !== null) emit('select-span', row.spanId);
}
</script>

<template>
  <div class="trace-events">
    <div class="trace-events-summary nai-numbers">
      {{ rows.length }} point event{{ rows.length === 1 ? '' : 's' }}
      <span v-if="!usable"> · legacy correlation records (no span ids)</span>
    </div>
    <div v-if="rows.length === 0" class="trace-events-state">
      {{ usable ? 'The graph response contains no point events.' : 'No legacy correlation records are available.' }}
    </div>
    <ol v-else class="trace-events-rows">
      <li
        v-for="row in rows"
        :key="row.key"
        class="trace-event-row"
        :class="{ 'trace-event-row--legacy': row.legacy }"
      >
        <span class="trace-event-seq nai-numbers">seq {{ row.seq ?? '—' }}</span>
        <span class="trace-event-ts nai-numbers">{{ formatTimestamp(row.ts) }}</span>
        <span class="trace-event-level" :class="`level--${row.level.toLowerCase()}`">{{
          row.level
        }}</span>
        <span class="trace-event-component" :title="row.component ?? 'component not recorded'">{{
          row.component ?? '—'
        }}</span>
        <span class="trace-event-message">
          <button
            v-if="row.spanId !== null"
            type="button"
            class="trace-event-message__link"
            :title="`Select span ${row.spanId}`"
            @click="selectSpan(row)"
          >
            {{ row.message }}
          </button>
          <span v-else>{{ row.message }}</span>
        </span>
        <span class="trace-event-span">
          <button
            v-if="row.spanId !== null"
            type="button"
            class="trace-event-span__link"
            :title="`span ${row.spanId}`"
            @click="selectSpan(row)"
          >
            span {{ shortId(row.spanId, 10) }}
          </button>
          <span v-else class="trace-event-span__none">point</span>
        </span>
      </li>
    </ol>
  </div>
</template>

<style scoped>
.trace-events {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-2);
  padding: var(--nai-space-3) var(--nai-space-4);
  background: var(--nai-panel-2);
  font-size: 10px;
  min-height: 0;
}

.trace-events-summary {
  color: var(--nai-dim);
  font-size: 9px;
}

.trace-events-state {
  padding: var(--nai-space-4);
  color: var(--nai-muted);
}

.trace-events-rows {
  list-style: none;
  margin: 0;
  padding: 0;
  overflow-x: auto;
}

.trace-event-row {
  display: grid;
  grid-template-columns: 62px 84px 54px 118px minmax(160px, 1fr) 104px;
  gap: var(--nai-space-3);
  align-items: baseline;
  min-width: 680px;
  padding: 1px 0;
  border-bottom: 1px dotted var(--nai-bar-track);
  color: var(--nai-muted);
}

.trace-event-seq,
.trace-event-ts {
  color: var(--nai-dim);
}

.trace-event-level {
  color: var(--nai-muted);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.3px;
}

.trace-event-level.level--warn,
.trace-event-level.level--warning {
  color: var(--nai-warn);
}

.trace-event-level.level--error,
.trace-event-level.level--fatal {
  color: var(--nai-bad);
}

.trace-event-level.level--request {
  color: var(--nai-accent);
}

.trace-event-level.level--tool {
  color: var(--nai-search);
}

.trace-event-component {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.trace-event-message {
  color: var(--nai-fg-soft);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.trace-event-message__link {
  height: auto;
  padding: 0;
  border: none;
  background: transparent;
  color: var(--nai-fg-soft);
  font: inherit;
  text-align: left;
  text-transform: none;
  letter-spacing: normal;
  max-width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.trace-event-message__link:hover {
  color: var(--nai-accent);
  border: none;
  background: transparent;
  text-decoration: underline;
}

.trace-event-span {
  text-align: right;
}

.trace-event-span__link {
  height: auto;
  padding: 0;
  border: none;
  background: transparent;
  color: var(--nai-accent);
  font: inherit;
  text-transform: none;
  letter-spacing: normal;
}

.trace-event-span__link:hover {
  border: none;
  background: transparent;
  text-decoration: underline;
}

.trace-event-span__none {
  color: var(--nai-dim);
  font-size: 8px;
  text-transform: uppercase;
  letter-spacing: 0.4px;
}

.trace-event-row--legacy .trace-event-component {
  color: var(--nai-dim);
}
</style>
