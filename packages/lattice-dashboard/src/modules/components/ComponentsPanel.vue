<script setup lang="ts">
import { computed, inject } from 'vue';
import { APP_CONTEXT_KEY } from '../../app-context';
import { formatAge, formatCount } from '../../ui/format';
import { useNow } from '../../shell/useNow';

defineProps<{ instanceId?: string }>();

const context = inject(APP_CONTEXT_KEY);
if (context === undefined) throw new Error('components context is missing');

const summaryRef = context.summary.summary;
const error = context.summary.error;
const stale = context.summary.stale;
const lastFetchedAt = context.summary.lastFetchedAt;
const loading = context.summary.loading;
const investigation = context.investigation;
const now = useNow();

const activeComponent = investigation.component;

const entries = computed(() => {
  const summary = summaryRef.value;
  if (summary === null) return [];
  return Object.entries(summary.components)
    .map(([name, count]) => ({ name, count }))
    .sort((a, b) => b.count - a.count || a.name.localeCompare(b.name));
});

const maxCount = computed(() => {
  let max = 0;
  for (const entry of entries.value) max = Math.max(max, entry.count);
  return max;
});

const total = computed(() => entries.value.reduce((sum, entry) => sum + entry.count, 0));
const age = computed(() => formatAge(lastFetchedAt.value, now.value));

function barWidth(count: number): string {
  if (maxCount.value <= 0) return '0%';
  return `${Math.max(1, (count / maxCount.value) * 100)}%`;
}

function focusComponent(name: string): void {
  investigation.setComponent(name);
  investigation.requestPanel('log-tail');
}

function refresh(): void {
  void context?.summary.refresh(true);
}
</script>

<template>
  <section class="components-panel" aria-label="Components">
    <div class="panel-toolbar">
      <span class="panel-title">component activity</span>
      <span class="panel-meta">
        window {{ formatCount(summaryRef?.window ?? null) }} · {{ entries.length }} components
      </span>
      <span class="toolbar-spacer" />
      <span v-if="stale" class="tone--warn">stale · {{ age }}</span>
      <span v-else-if="error !== null" class="tone--bad" :title="error">summary error</span>
      <span v-else class="panel-meta">measured {{ age }}</span>
      <button type="button" :disabled="loading" @click="refresh">REFRESH</button>
    </div>

    <div v-if="summaryRef === null && error !== null" class="panel-state panel-state--error" role="alert">
      <span>{{ error }}</span>
      <button type="button" @click="refresh">RETRY</button>
    </div>
    <div v-else-if="entries.length === 0" class="panel-state">
      <span v-if="loading">Loading component activity…</span>
      <span v-else>No component activity in the bounded summary window.</span>
    </div>
    <ul v-else class="component-list nai-scroll">
      <li v-for="entry in entries" :key="entry.name">
        <button
          type="button"
          class="component-row"
          :class="{ 'component-row--active': activeComponent === entry.name }"
          :title="`${entry.name} · ${formatCount(entry.count)} records · filter Log Tail`"
          @click="focusComponent(entry.name)"
        >
          <span class="component-name">{{ entry.name }}</span>
          <span class="component-bar" aria-hidden="true"><i :style="{ width: barWidth(entry.count) }" /></span>
          <span class="component-count nai-numbers">{{ formatCount(entry.count) }}</span>
          <span class="component-share nai-numbers">{{ total > 0 ? `${((entry.count / total) * 100).toFixed(1)}%` : '—' }}</span>
        </button>
      </li>
    </ul>
    <div class="panel-footer">
      <span>Click a component to filter the Log Tail (server-side query).</span>
    </div>
  </section>
</template>

<style scoped>
.components-panel {
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
  align-items: center;
  justify-content: center;
  gap: var(--nai-space-5);
  flex: 1 1 auto;
  padding: var(--nai-space-5);
  color: var(--nai-muted);
  font-size: var(--nai-font-size-sm);
  text-align: center;
}

.panel-state--error {
  color: var(--nai-bad);
}

.component-list {
  list-style: none;
  margin: 0;
  padding: var(--nai-space-2) 0;
  overflow: auto;
  scrollbar-gutter: stable;
  flex: 1 1 auto;
  min-height: 0;
}

.component-row {
  display: grid;
  grid-template-columns: minmax(120px, 1.6fr) minmax(40px, 1fr) 64px 52px;
  align-items: center;
  gap: var(--nai-space-4);
  width: 100%;
  height: 22px;
  padding: 0 var(--nai-space-4);
  border: none;
  border-left: 2px solid transparent;
  background: transparent;
  color: var(--nai-muted);
  text-align: left;
  font-size: var(--nai-font-size-sm);
  letter-spacing: normal;
  text-transform: none;
}

.component-row:hover {
  background: var(--nai-hover);
  border-color: transparent;
  border-left-color: var(--nai-accent);
}

.component-row--active {
  background: var(--nai-selected);
  border-left-color: var(--nai-accent);
  color: var(--nai-fg);
}

.component-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.component-bar {
  display: block;
  height: 7px;
  background: var(--nai-bar-track);
  overflow: hidden;
}

.component-bar i {
  display: block;
  height: 100%;
  background: linear-gradient(90deg, var(--nai-bar-start), var(--nai-accent));
}

.component-count {
  color: var(--nai-fg);
  text-align: right;
}

.component-share {
  color: var(--nai-dim);
  text-align: right;
  font-size: 10px;
}

.panel-footer {
  flex: 0 0 auto;
  padding: var(--nai-space-2) var(--nai-space-4);
  border-top: var(--nai-hairline) solid var(--nai-line);
  color: var(--nai-dim);
  font-size: 9px;
}

.tone--warn {
  color: var(--nai-warn);
}

.tone--bad {
  color: var(--nai-bad);
}
</style>
