<script setup lang="ts">
import { computed, inject, onBeforeUnmount, onMounted, ref, shallowRef, watch } from 'vue';
import { APP_CONTEXT_KEY } from '../../app-context';
import {
  errorGroupTone,
  groupErrorRecords,
  type ErrorGroup,
} from '../../analytics/errors';
import {
  loadErrorWindow,
  ERROR_MAX_RECORDS,
  type ErrorWindow,
} from '../../query/error-window';
import { isAbortError } from '../../query/client';
import { useNow } from '../../shell/useNow';
import {
  formatAge,
  formatCount,
  formatDateTime,
  formatTimestamp,
  shortId,
} from '../../ui/format';

defineProps<{ instanceId?: string }>();

const context = inject(APP_CONTEXT_KEY);
if (context === undefined) throw new Error('error intelligence context is missing');

const feed = context.feed;
const connection = feed.connection;
const investigation = context.investigation;
const now = useNow();

const includeWarn = ref(false);
const result = shallowRef<ErrorWindow | null>(null);
const loading = ref(false);
const error = shallowRef<string | null>(null);
const fetchedAt = ref<number | null>(null);

let controller: AbortController | null = null;
let generation = 0;
let disposed = false;
let adoptedCursor = false;

const groups = computed<ErrorGroup[]>(() =>
  result.value === null ? [] : groupErrorRecords(result.value.records),
);
const age = computed(() => formatAge(fetchedAt.value, now.value));
const totalMatches = computed(() => result.value?.records.length ?? 0);
const windowLabel = computed(() => {
  const info = result.value;
  if (info === null) return 'bounded window not loaded';
  const end = info.highWater !== null ? formatCount(info.highWater) : 'ledger end';
  return `seq ${formatCount(info.startAfter)}–${end}`;
});

async function load(): Promise<void> {
  if (disposed) return;
  const currentGeneration = (generation += 1);
  controller?.abort();
  const current = new AbortController();
  controller = current;
  loading.value = true;
  error.value = null;
  try {
    const window = await loadErrorWindow(context!.query, {
      includeWarn: includeWarn.value,
      highWater: feed.cursor.value > 0 ? feed.cursor.value : null,
      signal: current.signal,
    });
    if (disposed || currentGeneration !== generation || current.signal.aborted) return;
    result.value = window;
    fetchedAt.value = Date.now();
  } catch (caught) {
    if (disposed || currentGeneration !== generation || isAbortError(caught)) return;
    error.value = caught instanceof Error ? caught.message : 'error query failed';
  } finally {
    if (currentGeneration === generation) loading.value = false;
  }
}

function filterToLogTail(group: ErrorGroup): void {
  investigation.setComponent(group.component);
  investigation.requestPanel('log-tail');
}

function focusTrace(group: ErrorGroup): void {
  const traceId = group.sample.trace_id;
  if (traceId === null || traceId.length === 0) return;
  investigation.setTrace(traceId);
  investigation.requestPanel('traces');
}

function retry(): void {
  void load();
}

watch(includeWarn, () => {
  void load();
});

watch(
  () => feed.cursor.value,
  (value) => {
    if (adoptedCursor || value <= 0) return;
    adoptedCursor = true;
    if (result.value === null || result.value.retainedStart) void load();
  },
);

onMounted(() => {
  void load();
});

onBeforeUnmount(() => {
  disposed = true;
  generation += 1;
  controller?.abort();
  controller = null;
});
</script>

<template>
  <section class="errors-panel" aria-label="Error intelligence">
    <header class="panel-toolbar">
      <span class="panel-title">error intelligence</span>
      <label class="warn-toggle">
        <input v-model="includeWarn" type="checkbox" />
        <span>include warn</span>
      </label>
      <span class="panel-meta">
        {{ formatCount(totalMatches) }} match(es) · max {{ formatCount(ERROR_MAX_RECORDS) }}
      </span>
      <span class="panel-meta">{{ windowLabel }}</span>
      <span class="toolbar-spacer" />
      <span v-if="error !== null" class="tone--bad" :title="error">error query failed</span>
      <span v-else-if="result === null" class="panel-meta">not measured</span>
      <span v-else class="panel-meta">
        {{ result.complete ? 'window complete' : 'window bounded' }} · measured {{ age }}
      </span>
      <button type="button" :disabled="loading" @click="retry">REFRESH</button>
    </header>

    <div v-if="result !== null" class="window-strip" role="status">
      <span class="nai-numbers">
        scanned {{ formatCount(result.scanned) }} record(s) in {{ formatCount(result.pages) }} page(s)
      </span>
      <span v-if="result.retainedStart" class="tone--warn">
        live cursor unavailable · scanned from the retained-history start
      </span>
      <span v-if="connection !== 'live'" class="tone--warn">
        stream {{ connection }} · the window ends at the last observed cursor
      </span>
      <span v-if="result.truncated" class="tone--warn">
        bounded scan stopped early; at most {{ formatCount(ERROR_MAX_RECORDS) }} matching records are shown
      </span>
      <span v-if="!result.complete && !result.truncated" class="tone--warn">
        scan bound reached before the window end
      </span>
    </div>

    <div v-if="result === null && error !== null" class="panel-state panel-state--error" role="alert">
      <span>{{ error }}</span>
      <button type="button" @click="retry">RETRY</button>
    </div>
    <div v-else-if="result === null" class="panel-state">
      <span>{{ loading ? 'Scanning the host ledger for errors…' : 'No error window loaded.' }}</span>
    </div>
    <div v-else-if="groups.length === 0" class="panel-state panel-state--empty">
      <span>no errors in the bounded window</span>
      <span class="muted">
        The scan examined {{ formatCount(result.scanned) }} record(s); no error or warn group was
        measured{{ includeWarn ? ' (warnings were included)' : '' }}.
      </span>
    </div>
    <div v-else class="group-scroll">
      <table class="group-table">
        <thead>
          <tr>
            <th scope="col">severity</th>
            <th scope="col">component</th>
            <th scope="col">normalized message</th>
            <th scope="col">count</th>
            <th scope="col">first</th>
            <th scope="col">last</th>
            <th scope="col">sample trace</th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="group in groups"
            :key="group.key"
            class="group-row"
            tabindex="0"
            :title="group.sample.message"
            @click="filterToLogTail(group)"
            @keydown.enter.prevent="filterToLogTail(group)"
          >
            <td>
              <span class="severity" :class="`tone--${errorGroupTone(group)}`">
                {{ group.errorCount > 0 ? 'ERROR' : 'WARN' }}
              </span>
              <span v-if="group.errorCount > 0 && group.warnCount > 0" class="mix nai-numbers">
                {{ formatCount(group.warnCount) }} warn
              </span>
            </td>
            <td>
              <button
                type="button"
                class="cell-link"
                title="Filter the Log Tail by this component"
                @click.stop="filterToLogTail(group)"
              >
                {{ group.component }}
              </button>
            </td>
            <td class="message" :title="group.normalized">{{ group.normalized }}</td>
            <td class="nai-numbers count">{{ formatCount(group.count) }}</td>
            <td class="nai-numbers time" :title="formatDateTime(group.firstMs)">
              {{ formatTimestamp(group.firstMs) }}
            </td>
            <td class="nai-numbers time" :title="formatDateTime(group.lastMs)">
              {{ formatTimestamp(group.lastMs) }}
            </td>
            <td>
              <button
                v-if="group.sample.trace_id !== null && group.sample.trace_id.length > 0"
                type="button"
                class="cell-link trace-link"
                :title="group.sample.trace_id"
                @click.stop="focusTrace(group)"
              >
                {{ shortId(group.sample.trace_id, 12) }}
              </button>
              <span v-else class="muted">—</span>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <footer class="panel-footer">
      <span>Click a group to filter the Log Tail by component; the trace link focuses Traces.</span>
      <span class="muted">
        Grouping normalizes UUIDs, hex, ISO timestamps and numeric runs. Sample messages stay verbatim.
      </span>
    </footer>
  </section>
</template>

<style scoped>
.errors-panel {
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
  overflow: hidden;
}

.panel-title {
  color: var(--nai-accent);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.8px;
  white-space: nowrap;
}

.panel-meta {
  color: var(--nai-dim);
  font-size: 9px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.warn-toggle {
  display: inline-flex;
  align-items: center;
  gap: var(--nai-space-2);
  color: var(--nai-muted);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
  white-space: nowrap;
}

.warn-toggle input {
  accent-color: var(--nai-warn);
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
  white-space: nowrap;
}

button:hover:not(:disabled) {
  color: var(--nai-accent);
  border-color: var(--nai-accent);
  background: var(--nai-hover);
}

button:disabled {
  opacity: 0.45;
}

.window-strip {
  display: flex;
  align-items: center;
  gap: var(--nai-space-5);
  flex: 0 0 auto;
  padding: var(--nai-space-2) var(--nai-space-4);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel-2);
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.4px;
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

.panel-state--empty > span:first-child {
  color: var(--nai-fg-soft);
}

.panel-state--error {
  color: var(--nai-bad);
}

.muted {
  color: var(--nai-dim);
  font-size: 10px;
}

.group-scroll {
  flex: 1 1 auto;
  min-height: 0;
  overflow: auto;
  scrollbar-gutter: stable;
}

.group-table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--nai-font-size-sm);
  table-layout: fixed;
}

.group-table th {
  position: sticky;
  top: 0;
  z-index: 1;
  padding: var(--nai-space-2) var(--nai-space-3);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
  color: var(--nai-dim);
  font-size: 9px;
  font-weight: 400;
  text-transform: uppercase;
  letter-spacing: 0.6px;
  text-align: left;
}

.group-table th:nth-child(1) {
  width: 96px;
}

.group-table th:nth-child(2) {
  width: 170px;
}

.group-table th:nth-child(4) {
  width: 64px;
}

.group-table th:nth-child(5),
.group-table th:nth-child(6) {
  width: 96px;
}

.group-table th:nth-child(7) {
  width: 112px;
}

.group-table td {
  padding: 2px var(--nai-space-3);
  border-bottom: var(--nai-hairline) solid var(--nai-row-line);
  color: var(--nai-muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  vertical-align: middle;
}

.group-row {
  cursor: pointer;
  outline: none;
}

.group-row:hover {
  background: rgba(25, 194, 216, 0.05);
}

.group-row:focus-visible {
  box-shadow: inset 0 0 0 1px var(--nai-focus);
}

.severity {
  font-weight: 700;
  font-size: 9px;
  letter-spacing: 0.4px;
}

.mix {
  margin-left: var(--nai-space-2);
  color: var(--nai-dim);
  font-size: 9px;
}

.message {
  color: var(--nai-fg);
  font-size: 10.5px;
}

.count {
  color: var(--nai-fg);
  text-align: right;
}

.time {
  color: var(--nai-dim);
  font-size: 10px;
}

.cell-link {
  height: auto;
  padding: 0;
  border: none;
  background: transparent;
  color: var(--nai-accent);
  font-size: inherit;
  letter-spacing: normal;
  text-transform: none;
  text-align: left;
  max-width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
}

.cell-link:hover {
  color: var(--nai-accent-strong);
  border: none;
  background: transparent;
  text-decoration: underline;
}

.trace-link {
  color: var(--nai-search);
}

.panel-footer {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex: 0 0 auto;
  padding: var(--nai-space-2) var(--nai-space-4);
  border-top: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
  color: var(--nai-dim);
  font-size: 9px;
  overflow: hidden;
}

.panel-footer > span {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.tone--warn {
  color: var(--nai-warn);
}

.tone--bad {
  color: var(--nai-bad);
}
</style>
