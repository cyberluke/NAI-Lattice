<script setup lang="ts">
import { computed, inject, onBeforeUnmount, onMounted, ref, shallowRef, watch } from 'vue';
import { APP_CONTEXT_KEY } from '../../app-context';
import type { TraceSummary, TracesResponse } from '../../query/traces';
import {
  formatAge,
  formatCount,
  formatDuration,
  formatTimestamp,
  shortId,
} from '../../ui/format';
import { useNow } from '../../shell/useNow';
import TraceDetail from './TraceDetail.vue';

defineProps<{ instanceId?: string }>();

const context = inject(APP_CONTEXT_KEY);
if (context === undefined) throw new Error('traces context is missing');

const investigation = context.investigation;
const now = useNow();
const response = shallowRef<TracesResponse | null>(null);
const loading = ref(false);
const error = shallowRef<string | null>(null);
const expanded = ref<ReadonlySet<string>>(new Set());
const lastFetchedAt = ref<number | null>(null);
const activeTrace = investigation.trace;

type TraceLookupState = 'idle' | 'loading' | 'found' | 'not-found' | 'error';
const remoteTrace = shallowRef<TraceSummary | null>(null);
const lookupState = ref<TraceLookupState>('idle');
const lookupTraceId = shallowRef<string | null>(null);
const lookupTruncated = ref(false);
const lookupError = shallowRef<string | null>(null);

const MAX_DISPLAYED = 200;
let timer: ReturnType<typeof setInterval> | null = null;
let controller: AbortController | null = null;
let lookupController: AbortController | null = null;
let lookupGeneration = 0;
let disposed = false;
let inFlight = false;

const allTraces = computed(() => response.value?.traces ?? []);

const matchingTraces = computed(() => {
  const filter = activeTrace.value.trim();
  if (filter.length === 0) return allTraces.value;
  const inWindow = allTraces.value.filter((trace) => trace.traceId === filter);
  const resolved = remoteTrace.value;
  if (resolved !== null && resolved.traceId === filter) return [resolved, ...inWindow];
  return inWindow;
});

const visibleTraces = computed(() => matchingTraces.value.slice(0, MAX_DISPLAYED));

const hiddenCount = computed(() =>
  Math.max(0, matchingTraces.value.length - visibleTraces.value.length),
);

const freshAge = computed(() => formatAge(lastFetchedAt.value, now.value));

function isExpanded(traceId: string): boolean {
  return expanded.value.has(traceId);
}

function toggle(traceId: string): void {
  const next = new Set(expanded.value);
  if (next.has(traceId)) next.delete(traceId);
  else next.add(traceId);
  expanded.value = next;
}

function expand(traceId: string): void {
  const next = new Set(expanded.value);
  next.add(traceId);
  expanded.value = next;
}

function cancelLookup(): void {
  lookupGeneration += 1;
  lookupController?.abort();
  lookupController = null;
}

function resetLookup(): void {
  cancelLookup();
  remoteTrace.value = null;
  lookupState.value = 'idle';
  lookupTraceId.value = null;
  lookupTruncated.value = false;
  lookupError.value = null;
}

/**
 * Resolves a focused trace that the bounded window did not return. The host
 * echoes the filter and reports `truncated`; an empty `traces` array means
 * "not found", never an invented span.
 */
async function resolveFocusedTrace(traceId: string, force = false): Promise<void> {
  if (disposed || traceId.length === 0) return;
  if (allTraces.value.some((trace) => trace.traceId === traceId)) {
    cancelLookup();
    remoteTrace.value = null;
    lookupState.value = 'found';
    lookupTraceId.value = traceId;
    lookupTruncated.value = false;
    lookupError.value = null;
    expand(traceId);
    return;
  }
  if (
    !force &&
    lookupTraceId.value === traceId &&
    (lookupState.value === 'loading' ||
      lookupState.value === 'found' ||
      lookupState.value === 'not-found')
  ) {
    return;
  }
  cancelLookup();
  const generation = lookupGeneration;
  const current = new AbortController();
  lookupController = current;
  lookupTraceId.value = traceId;
  lookupState.value = 'loading';
  lookupTruncated.value = false;
  lookupError.value = null;
  remoteTrace.value = null;
  try {
    const result = await context?.traces.fetchTrace(traceId, current.signal);
    if (
      disposed ||
      current.signal.aborted ||
      generation !== lookupGeneration ||
      activeTrace.value.trim() !== traceId ||
      result === undefined
    ) {
      return;
    }
    lookupTruncated.value = result.truncated === true;
    const found = result.traces.find((trace) => trace.traceId === traceId) ?? null;
    if (found === null) {
      remoteTrace.value = null;
      lookupState.value = 'not-found';
      return;
    }
    remoteTrace.value = found;
    lookupState.value = 'found';
    expand(traceId);
  } catch (caught) {
    if (disposed || current.signal.aborted || generation !== lookupGeneration) return;
    remoteTrace.value = null;
    lookupState.value = 'error';
    lookupError.value = caught instanceof Error ? caught.message : 'trace lookup failed';
  } finally {
    if (lookupController === current) lookupController = null;
  }
}

async function load(): Promise<void> {
  if (disposed || inFlight) return;
  if (typeof document !== 'undefined' && document.visibilityState === 'hidden') return;
  inFlight = true;
  loading.value = true;
  controller?.abort();
  const current = new AbortController();
  controller = current;
  try {
    const result = await context?.traces.fetch(current.signal);
    if (disposed || current.signal.aborted || result === undefined) return;
    response.value = result;
    lastFetchedAt.value = Date.now();
    error.value = null;
    const filter = activeTrace.value.trim();
    if (filter.length === 0) return;
    if (result.traces.some((trace) => trace.traceId === filter)) {
      cancelLookup();
      remoteTrace.value = null;
      lookupTraceId.value = filter;
      lookupState.value = 'found';
      lookupTruncated.value = false;
      lookupError.value = null;
      expand(filter);
      return;
    }
    if (lookupTraceId.value !== filter) void resolveFocusedTrace(filter);
  } catch (caught) {
    if (disposed || current.signal.aborted) return;
    error.value = caught instanceof Error ? caught.message : 'traces request failed';
  } finally {
    if (controller === current) controller = null;
    inFlight = false;
    loading.value = false;
  }
}

function focusTrace(traceId: string): void {
  investigation.setTrace(traceId);
  const next = new Set(expanded.value);
  next.add(traceId);
  expanded.value = next;
}

function clearTrace(): void {
  resetLookup();
  investigation.setTrace('');
}

function retryLookup(): void {
  const traceId = activeTrace.value.trim();
  if (traceId.length > 0) void resolveFocusedTrace(traceId, true);
}

function traceTone(trace: TraceSummary): 'ok' | 'warn' | 'bad' | 'muted' {
  // An open trace has no verdict yet, whatever `ok` says (it is null on the
  // wire but a stale payload must never paint an unfinished trace green).
  if (trace.state === 'open') return 'warn';
  if (trace.ok === true) return 'ok';
  if (trace.ok === false) return 'bad';
  return 'muted';
}

watch(activeTrace, (value) => {
  const traceId = value.trim();
  if (traceId.length === 0) {
    resetLookup();
    return;
  }
  expand(traceId);
  resetLookup();
  void resolveFocusedTrace(traceId);
});

onMounted(() => {
  void load();
  timer = setInterval(() => {
    void load();
  }, 5000);
});

onBeforeUnmount(() => {
  disposed = true;
  if (timer !== null) clearInterval(timer);
  timer = null;
  controller?.abort();
  controller = null;
  cancelLookup();
});
</script>

<template>
  <section class="traces-panel" aria-label="Request to response traces">
    <div class="panel-toolbar">
      <span class="panel-title">request → response</span>
      <span class="panel-meta">
        window {{ formatCount(response?.window ?? null) }} · {{ visibleTraces.length }} traces
      </span>
      <span class="toolbar-spacer" />
      <span v-if="error !== null" class="tone--bad" :title="error">traces error</span>
      <span v-else class="panel-meta">measured {{ freshAge }}</span>
      <button type="button" :disabled="loading" @click="load">REFRESH</button>
    </div>

    <div v-if="activeTrace.trim().length > 0" class="trace-filter">
      <span>focused trace <strong>{{ activeTrace }}</strong></span>
      <span v-if="lookupState === 'loading'" class="panel-meta">resolving in ledger…</span>
      <span v-else-if="lookupState === 'found' && remoteTrace !== null" class="panel-meta">
        resolved outside the current window
      </span>
      <span v-if="lookupTruncated" class="tone--warn">search was bounded</span>
      <span class="toolbar-spacer" />
      <button type="button" @click="clearTrace">CLEAR</button>
    </div>

    <div v-if="lookupState === 'error'" class="lookup-error" role="alert">
      <span>trace lookup failed: {{ lookupError }}</span>
      <button type="button" @click="retryLookup">RETRY</button>
    </div>

    <div v-if="response === null && error !== null" class="panel-state panel-state--error" role="alert">
      <span>{{ error }}</span>
      <button type="button" @click="load">RETRY</button>
    </div>
    <div v-else-if="visibleTraces.length === 0" class="panel-state">
      <span v-if="loading && response === null">Loading traces…</span>
      <span v-else-if="activeTrace.trim().length > 0 && lookupState === 'not-found'">
        trace not found in the ledger<span v-if="lookupTruncated">
          — the search was bounded (truncated scan), so the trace may be older than the examined records.</span>
      </span>
      <span v-else-if="activeTrace.trim().length > 0 && lookupState === 'error'">
        The ledger lookup failed; the focused trace was not resolved.
      </span>
      <span v-else-if="activeTrace.trim().length > 0">
        Resolving {{ activeTrace }} in the ledger…
      </span>
      <span v-else>No request→response traces in the bounded window.</span>
    </div>
    <div v-else class="traces-scroll">
      <table class="trace-table">
        <thead>
          <tr>
            <th scope="col">trace</th>
            <th scope="col">state</th>
            <th scope="col">terminal</th>
            <th scope="col">duration</th>
            <th scope="col" title="Legacy lifecycle records; schema-2 spans are counted in the trace detail.">
              records
            </th>
            <th scope="col">first</th>
            <th scope="col" aria-label="actions"></th>
          </tr>
        </thead>
        <tbody>
          <template v-for="trace in visibleTraces" :key="trace.traceId">
            <tr class="trace-row" :class="{ 'trace-row--open': isExpanded(trace.traceId) }">
              <td>
                <button type="button" class="trace-id" :title="trace.traceId" @click="focusTrace(trace.traceId)">
                  {{ shortId(trace.traceId, 12) }}
                </button>
              </td>
              <td>
                <span class="trace-state-cell">
                  <span class="trace-state" :class="`tone--${traceTone(trace)}`">{{ trace.state }}</span>
                  <span v-if="trace.state === 'open'" class="ok-unknown">ok: null</span>
                </span>
              </td>
              <td>{{ trace.terminal ?? '—' }}</td>
              <td class="nai-numbers">{{ formatDuration(trace.durationMs) }}</td>
              <td class="nai-numbers">{{ formatCount(trace.spans.length) }}</td>
              <td class="nai-numbers">{{ formatTimestamp(trace.firstTs) }}</td>
              <td class="trace-actions">
                <button
                  type="button"
                  :aria-expanded="isExpanded(trace.traceId)"
                  :title="isExpanded(trace.traceId) ? 'Collapse spans' : 'Expand spans'"
                  @click="toggle(trace.traceId)"
                >
                  {{ isExpanded(trace.traceId) ? 'COLLAPSE' : 'EXPAND' }}
                </button>
              </td>
            </tr>
            <tr v-if="isExpanded(trace.traceId)" class="span-row">
              <td colspan="7">
                <TraceDetail :trace="trace" />
              </td>
            </tr>
          </template>
        </tbody>
      </table>
      <div v-if="hiddenCount > 0" class="hidden-note">
        Showing the newest {{ MAX_DISPLAYED }} traces of {{ formatCount(matchingTraces.length) }}; narrow with a
        focused trace.
      </div>
    </div>

    <div class="panel-footer">
      <span>
        Expand a trace for CHAIN · WATERFALL · CALL TREE · EVENTS from the host execution graph.
        Legacy correlation shows point records only; no chain is invented.
      </span>
    </div>
  </section>
</template>

<style scoped>
.traces-panel {
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

.trace-filter {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex: 0 0 auto;
  padding: var(--nai-space-2) var(--nai-space-4);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-accent-bg);
  color: var(--nai-search);
  font-size: 10px;
}

.trace-filter strong {
  color: var(--nai-fg);
  font-weight: 400;
}

.lookup-error {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex: 0 0 auto;
  padding: var(--nai-space-2) var(--nai-space-4);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  border-left: 2px solid var(--nai-bad);
  background: var(--nai-panel);
  color: var(--nai-bad);
  font-size: 10px;
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

.traces-scroll {
  flex: 1 1 auto;
  min-height: 0;
  overflow: auto;
  scrollbar-gutter: stable;
}

.trace-table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--nai-font-size-sm);
  table-layout: fixed;
}

.trace-table th {
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

.trace-table th:nth-child(1) {
  width: 152px;
}

.trace-table th:nth-child(2) {
  width: 74px;
}

.trace-table th:nth-child(3) {
  width: 90px;
}

.trace-table th:nth-child(4) {
  width: 76px;
}

.trace-table th:nth-child(5) {
  width: 58px;
}

.trace-table th:nth-child(6) {
  width: 92px;
}

.trace-table th:nth-child(7) {
  width: 92px;
}

.trace-table td {
  padding: 2px var(--nai-space-3);
  border-bottom: var(--nai-hairline) solid var(--nai-row-line);
  color: var(--nai-muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.trace-row:hover {
  background: rgba(25, 194, 216, 0.05);
}

.trace-row--open {
  background: var(--nai-selected);
}

.trace-id {
  height: 18px;
  padding: 0;
  border: none;
  color: var(--nai-accent);
  letter-spacing: normal;
  text-transform: none;
  font-size: var(--nai-font-size-sm);
}

.trace-id:hover {
  border: none;
  background: transparent;
  text-decoration: underline;
}

.trace-state-cell {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.trace-state {
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.4px;
}

.ok-unknown {
  margin-left: var(--nai-space-2);
  color: var(--nai-dim);
  font-size: 8px;
  letter-spacing: 0.3px;
}

.trace-actions {
  text-align: right;
}

.trace-actions button {
  height: 18px;
}

.span-row td {
  padding: 0;
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
  /* The table cell must not force the detail panels onto one clipped line. */
  white-space: normal;
  overflow: visible;
  text-overflow: clip;
}

.span-list {
  list-style: none;
  margin: 0;
  padding: 2px var(--nai-space-4) var(--nai-space-3) calc(152px + var(--nai-space-4));
}

.span-list li {
  display: grid;
  grid-template-columns: 92px 62px minmax(0, 1fr) 64px 90px;
  gap: var(--nai-space-4);
  padding: 1px 0;
  color: var(--nai-muted);
  font-size: 11px;
  border-bottom: 1px dotted var(--nai-bar-track);
}

.span-kind {
  color: var(--nai-accent);
  text-transform: uppercase;
  font-size: 9px;
  letter-spacing: 0.4px;
}

.span-label {
  overflow: hidden;
  text-overflow: ellipsis;
}

.span-seq {
  color: var(--nai-dim);
  text-align: right;
}

.span-empty {
  padding: var(--nai-space-3) calc(152px + var(--nai-space-4));
  color: var(--nai-dim);
  font-size: 10px;
}

.hidden-note {
  padding: var(--nai-space-2) var(--nai-space-4);
  color: var(--nai-dim);
  font-size: 9px;
}

.panel-footer {
  flex: 0 0 auto;
  padding: var(--nai-space-2) var(--nai-space-4);
  border-top: var(--nai-hairline) solid var(--nai-line);
  color: var(--nai-dim);
  font-size: 9px;
}

.tone--ok {
  color: var(--nai-ok);
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
