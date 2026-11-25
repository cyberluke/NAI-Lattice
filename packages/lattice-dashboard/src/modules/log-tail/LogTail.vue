<script setup lang="ts">
import {
  computed,
  inject,
  nextTick,
  onBeforeUnmount,
  onMounted,
  ref,
  shallowRef,
  watch,
} from 'vue';
import { APP_CONTEXT_KEY } from '../../app-context';
import { LogView, type LogViewMeta } from '../../telemetry/view';
import { QuerySession, type QueryFilters, type QuerySessionState } from '../../query/client';
import type { LogLevel, LogRecord } from '../../telemetry/types';
import { LOG_LEVELS } from '../../telemetry/types';
import {
  displayLevel,
  formatAge,
  formatCount,
  formatDateTime,
  formatTimestamp,
  levelCssClass,
  shortId,
} from '../../ui/format';
import { useNow } from '../../shell/useNow';
import RecordInspector from '../inspector/RecordInspector.vue';

defineProps<{ instanceId?: string }>();

const context = inject(APP_CONTEXT_KEY);
if (context === undefined) throw new Error('log tail context is missing');

const feed = context.feed;
const interop = context.interop;
const investigation = context.investigation;
const now = useNow();

const EMPTY_META: LogViewMeta = {
  paused: false,
  suspended: false,
  catchUp: 'idle',
  catchUpError: null,
  lastSeq: 0,
  truncated: 0,
};

const EMPTY_QUERY: QuerySessionState = {
  rows: [],
  loading: false,
  error: null,
  hasMore: false,
  nextAfter: null,
  truncated: false,
  cleared: false,
};

const rows = shallowRef<readonly LogRecord[]>([]);
const viewMeta = shallowRef<LogViewMeta>(EMPTY_META);
const displayMode = ref<'live' | 'query'>('live');
const queryState = shallowRef<QuerySessionState>(EMPTY_QUERY);
const selectedSeq = ref<number | null>(null);
const inspectedSeq = ref<number | null>(null);
const resumeBusy = ref(false);

const filterText = ref('');
const filterComponent = ref(investigation.component.value);
const filterTrace = ref(investigation.trace.value);
const filterLevel = ref<LogLevel | ''>('');
const sinceMs = ref<number | null>(investigation.sinceMs.value);

const ROW_HEIGHT = 22;
const OVERSCAN = 10;

const scroller = ref<HTMLElement | null>(null);
const scrollTop = ref(0);
const viewportHeight = ref(480);
const follow = ref(true);
let resizeObserver: ResizeObserver | null = null;
let scrollFrame: number | null = null;
let debounceTimer: ReturnType<typeof setTimeout> | null = null;
let windowResizeHandler: (() => void) | null = null;

const view = new LogView(feed, {
  capacity: 5000,
  onUpdate: (next, meta) => applyViewRows(next, meta),
});

const querySession = new QuerySession(context.query, (state) => {
  queryState.value = state;
});

const displayRows = computed<readonly LogRecord[]>(() =>
  displayMode.value === 'query' ? queryState.value.rows : rows.value,
);

const inspectedRecord = computed<LogRecord | null>(() => {
  if (inspectedSeq.value === null) return null;
  return (
    displayRows.value.find((row) => row.runtime_sequence === inspectedSeq.value) ?? null
  );
});

const total = computed(() => displayRows.value.length);
const visibleStart = computed(() =>
  Math.max(0, Math.floor(scrollTop.value / ROW_HEIGHT) - OVERSCAN),
);
const visibleEnd = computed(() =>
  Math.min(
    total.value,
    Math.ceil((scrollTop.value + Math.max(viewportHeight.value, ROW_HEIGHT)) / ROW_HEIGHT) + OVERSCAN,
  ),
);
const visibleRows = computed(() => displayRows.value.slice(visibleStart.value, visibleEnd.value));
const topPad = computed(() => visibleStart.value * ROW_HEIGHT);
const bottomPad = computed(() => Math.max(0, total.value * ROW_HEIGHT - visibleEnd.value * ROW_HEIGHT));

const hasFilters = computed(
  () =>
    filterText.value.trim().length > 0 ||
    filterComponent.value.trim().length > 0 ||
    filterTrace.value.trim().length > 0 ||
    filterLevel.value !== '' ||
    sinceMs.value !== null,
);

const filterKey = computed(() =>
  JSON.stringify([
    filterText.value.trim(),
    filterComponent.value.trim(),
    filterTrace.value.trim(),
    filterLevel.value,
    sinceMs.value,
  ]),
);

const paused = computed(() => viewMeta.value.paused);
const pausedBehind = computed(() => Math.max(0, feed.cursor.value - viewMeta.value.lastSeq));
const connection = feed.connection;
const lastEventAt = feed.lastEventAt;
const streamError = feed.lastError;
const lifecycle = feed.lifecycle;

const lifecycleNotice = computed(() => {
  const state = lifecycle.value?.state;
  if (state === null || state === undefined || state.length === 0) return null;
  const normalized = state.toLowerCase();
  if (['healthy', 'ok', 'running', 'ready', 'started'].includes(normalized)) return null;
  const detail = lifecycle.value?.detail;
  return detail !== null && detail !== undefined && detail.length > 0
    ? `${state} — ${detail}`
    : state;
});

const connectionLabel = computed(() => {
  switch (connection.value) {
    case 'live':
      return 'LIVE';
    case 'connecting':
      return 'CONNECTING';
    case 'retrying':
      return 'RETRYING';
    case 'offline':
      return 'OFFLINE';
    default:
      return 'STREAM ERROR';
  }
});

const connectionTone = computed(() => {
  switch (connection.value) {
    case 'live':
      return 'ok';
    case 'connecting':
    case 'retrying':
      return 'warn';
    default:
      return 'bad';
  }
});

const connectionAge = computed(() => formatAge(lastEventAt.value, now.value));

const gapItems = computed(() =>
  feed.gaps.value.slice(-3).map((gap) => {
    const range = gap.to === null ? `after seq ${formatCount(gap.from)}` : `seq ${formatCount(gap.from)}–${formatCount(gap.to)}`;
    const count = gap.count !== undefined ? ` (${formatCount(gap.count)} dropped)` : '';
    return `${range} · ${gap.reason}${count}`;
  }),
);

const queryError = computed(() => queryState.value.error);
const queryCleared = computed(() => queryState.value.cleared);
const queryLoading = computed(() => queryState.value.loading);
const displayCount = computed(() => formatCount(total.value));

function applyViewRows(next: readonly LogRecord[], meta: LogViewMeta): void {
  const previous = rows.value;
  const element = scroller.value;
  const anchor =
    displayMode.value === 'live' && !follow.value && element !== null
      ? captureAnchor(previous, element.scrollTop)
      : null;
  rows.value = next;
  viewMeta.value = meta;
  nextTick(() => {
    if (displayMode.value !== 'live') return;
    if (follow.value) {
      scrollToBottom();
    } else if (anchor !== null) {
      restoreAnchor(next, anchor);
    }
  });
}

interface Anchor {
  sequence: number;
  offset: number;
}

function captureAnchor(list: readonly LogRecord[], top: number): Anchor | null {
  if (list.length === 0) return null;
  const index = Math.max(0, Math.min(list.length - 1, Math.floor(top / ROW_HEIGHT)));
  return { sequence: list[index].runtime_sequence, offset: top - index * ROW_HEIGHT };
}

function restoreAnchor(list: readonly LogRecord[], anchor: Anchor): void {
  const element = scroller.value;
  if (element === null) return;
  const index = list.findIndex((row) => row.runtime_sequence === anchor.sequence);
  if (index < 0) return;
  const top = index * ROW_HEIGHT + anchor.offset;
  element.scrollTop = top;
  scrollTop.value = top;
}

function isAtBottom(element: HTMLElement): boolean {
  return element.scrollHeight - element.scrollTop - element.clientHeight <= ROW_HEIGHT * 1.5;
}

function scrollToBottom(): void {
  const element = scroller.value;
  if (element === null) return;
  nextTick(() => {
    element.scrollTop = element.scrollHeight;
    scrollTop.value = element.scrollTop;
    follow.value = true;
  });
}

function onScroll(): void {
  const element = scroller.value;
  if (element === null) return;
  if (scrollFrame !== null) return;
  const apply = (): void => {
    scrollFrame = null;
    if (scroller.value === null) return;
    scrollTop.value = scroller.value.scrollTop;
    follow.value = isAtBottom(scroller.value);
  };
  if (typeof requestAnimationFrame === 'function') {
    scrollFrame = requestAnimationFrame(apply);
  } else {
    apply();
  }
}

function buildFilters(): QueryFilters {
  const filters: QueryFilters = {};
  const q = filterText.value.trim();
  const component = filterComponent.value.trim();
  const trace = filterTrace.value.trim();
  if (q.length > 0) filters.q = q;
  if (component.length > 0) filters.component = component;
  if (trace.length > 0) filters.trace = trace;
  if (filterLevel.value !== '') filters.level = filterLevel.value;
  if (sinceMs.value !== null) filters.since_ms = sinceMs.value;
  return filters;
}

function scheduleFilter(): void {
  if (debounceTimer !== null) clearTimeout(debounceTimer);
  // Abort immediately on a filter edit; the 250ms debounce only delays the next
  // request, never leaves the previous one racing the new filter.
  if (displayMode.value === 'query') querySession.cancel();
  debounceTimer = setTimeout(() => {
    debounceTimer = null;
    applyFilters();
  }, 250);
}

function applyFilters(): void {
  if (!hasFilters.value) {
    if (displayMode.value === 'query') void returnToLive();
    return;
  }
  displayMode.value = 'query';
  view.suspend();
  selectedSeq.value = null;
  inspectedSeq.value = null;
  follow.value = false;
  const element = scroller.value;
  if (element !== null) {
    element.scrollTop = 0;
    scrollTop.value = 0;
  }
  void querySession.start(buildFilters());
}

function runFiltersNow(): void {
  if (debounceTimer !== null) {
    clearTimeout(debounceTimer);
    debounceTimer = null;
  }
  applyFilters();
}

function clearFilters(): void {
  filterText.value = '';
  filterComponent.value = '';
  filterTrace.value = '';
  filterLevel.value = '';
  sinceMs.value = null;
}

async function returnToLive(): Promise<void> {
  if (displayMode.value === 'live') return;
  querySession.stop();
  displayMode.value = 'live';
  selectedSeq.value = null;
  inspectedSeq.value = null;
  follow.value = true;
  await view.resume();
  scrollToBottom();
}

async function togglePause(): Promise<void> {
  if (displayMode.value === 'query' || resumeBusy.value) return;
  if (paused.value) {
    resumeBusy.value = true;
    try {
      await view.resume();
      scrollToBottom();
    } finally {
      resumeBusy.value = false;
    }
  } else {
    view.pause();
  }
}

function clearView(): void {
  selectedSeq.value = null;
  inspectedSeq.value = null;
  if (displayMode.value === 'query') {
    querySession.clearRows();
    return;
  }
  view.clear();
  scrollTop.value = 0;
  if (scroller.value !== null) scroller.value.scrollTop = 0;
}

function refresh(): void {
  if (displayMode.value === 'query') {
    void querySession.retry();
    return;
  }
  feed.retry();
}

function loadMore(): void {
  void querySession.loadMore();
}

function rerunQuery(): void {
  void querySession.retry();
}

function selectRow(row: LogRecord, raise: boolean): void {
  selectedSeq.value = row.runtime_sequence;
  inspectedSeq.value = row.runtime_sequence;
  if (raise) void interop.selectLog(row);
}

function closeInspector(): void {
  inspectedSeq.value = null;
}

function focusTraceFromInspector(traceId: string): void {
  focusTrace(traceId);
  closeInspector();
  investigation.requestPanel('traces');
}

function focusComponent(component: string): void {
  if (component.length === 0 || component === 'unknown') return;
  filterComponent.value = component;
  investigation.setComponent(component);
}

function focusTrace(traceId: string | null): void {
  if (traceId === null || traceId.length === 0) return;
  filterTrace.value = traceId;
  investigation.setTrace(traceId);
}

function ensureVisible(index: number): void {
  const element = scroller.value;
  if (element === null) return;
  const top = index * ROW_HEIGHT;
  if (top < element.scrollTop) {
    element.scrollTop = top;
  } else if (top + ROW_HEIGHT > element.scrollTop + element.clientHeight) {
    element.scrollTop = top + ROW_HEIGHT - element.clientHeight;
  }
  scrollTop.value = element.scrollTop;
  follow.value = isAtBottom(element);
}

function onKeydown(event: KeyboardEvent): void {
  const list = displayRows.value;
  if (list.length === 0) return;
  let index = list.findIndex((row) => row.runtime_sequence === selectedSeq.value);
  if (index < 0) index = 0;
  let next = index;
  switch (event.key) {
    case 'ArrowDown':
      next = Math.min(list.length - 1, index + 1);
      break;
    case 'ArrowUp':
      next = Math.max(0, index - 1);
      break;
    case 'Home':
      next = 0;
      break;
    case 'End':
      next = list.length - 1;
      break;
    case 'PageDown':
      next = Math.min(list.length - 1, index + 10);
      break;
    case 'PageUp':
      next = Math.max(0, index - 10);
      break;
    case 'Enter':
      selectRow(list[index], true);
      event.preventDefault();
      return;
    case 'Escape':
      if (inspectedSeq.value !== null) {
        inspectedSeq.value = null;
      } else {
        selectedSeq.value = null;
      }
      event.preventDefault();
      return;
    default:
      return;
  }
  event.preventDefault();
  selectRow(list[next], false);
  ensureVisible(next);
}

function onResize(): void {
  const element = scroller.value;
  if (element === null) return;
  viewportHeight.value = element.clientHeight;
  if (follow.value) scrollToBottom();
}

watch(filterKey, () => scheduleFilter());

watch(
  () => investigation.component.value,
  (value) => {
    if (filterComponent.value !== value) filterComponent.value = value;
  },
);

watch(
  () => investigation.trace.value,
  (value) => {
    if (filterTrace.value !== value) filterTrace.value = value;
  },
);

watch(
  () => investigation.sinceMs.value,
  (value) => {
    if (sinceMs.value !== value) sinceMs.value = value;
  },
);

onMounted(() => {
  view.start();
  const element = scroller.value;
  if (element !== null) {
    viewportHeight.value = element.clientHeight;
    scrollToBottom();
  }
  if (typeof ResizeObserver !== 'undefined' && element !== null) {
    resizeObserver = new ResizeObserver(() => onResize());
    resizeObserver.observe(element);
  } else if (typeof window !== 'undefined') {
    windowResizeHandler = () => onResize();
    window.addEventListener('resize', windowResizeHandler);
  }
  if (hasFilters.value) applyFilters();
});

onBeforeUnmount(() => {
  view.dispose();
  querySession.dispose();
  if (debounceTimer !== null) clearTimeout(debounceTimer);
  if (scrollFrame !== null && typeof cancelAnimationFrame === 'function') {
    cancelAnimationFrame(scrollFrame);
  }
  if (resizeObserver !== null) {
    resizeObserver.disconnect();
    resizeObserver = null;
  }
  if (windowResizeHandler !== null && typeof window !== 'undefined') {
    window.removeEventListener('resize', windowResizeHandler);
    windowResizeHandler = null;
  }
});
</script>

<template>
  <section class="log-tail" aria-label="Log tail">
    <div class="log-toolbar">
      <div class="toolbar-row">
        <span class="stream-state" :class="`tone--${connectionTone}`">
          <span class="dot" aria-hidden="true"></span>
          <span role="status" aria-live="polite">{{ connectionLabel }}</span>
          <span class="muted" aria-hidden="true">· {{ connectionAge }}</span>
        </span>
        <span class="mode-chip" :class="{ 'mode-chip--query': displayMode === 'query' }">
          {{ displayMode === 'query' ? 'SERVER QUERY' : 'LIVE VIEW' }}
        </span>
        <span class="row-count nai-numbers">{{ displayCount }} rows</span>
        <span v-if="paused" class="behind nai-numbers">
          VIEW FROZEN · {{ formatCount(pausedBehind) }} ingested behind
        </span>
        <span class="toolbar-spacer" />
        <button
          type="button"
          :disabled="displayMode === 'query' || resumeBusy"
          :title="displayMode === 'query' ? 'Return to live before pausing' : 'Freeze this view; ingestion continues'"
          @click="togglePause"
        >
          {{ paused ? 'RESUME' : 'PAUSE' }}
        </button>
        <button type="button" title="Clear this view only; persisted history is untouched" @click="clearView">
          CLEAR
        </button>
        <button type="button" title="Reconnect the stream / retry the active server query" @click="refresh">
          REFRESH
        </button>
        <button
          v-if="displayMode === 'query'"
          type="button"
          title="Resume the live view from its previous cursor"
          @click="returnToLive"
        >
          BACK TO LIVE
        </button>
      </div>

      <form class="filter-row" role="search" @submit.prevent="runFiltersNow">
        <label class="filter-field filter-field--grow">
          <span>message</span>
          <input v-model="filterText" type="search" autocomplete="off" spellcheck="false" placeholder="contains…" />
        </label>
        <label class="filter-field">
          <span>component</span>
          <input v-model="filterComponent" type="text" autocomplete="off" spellcheck="false" placeholder="exact" />
        </label>
        <label class="filter-field">
          <span>trace</span>
          <input v-model="filterTrace" type="text" autocomplete="off" spellcheck="false" placeholder="exact id" />
        </label>
        <label class="filter-field">
          <span>min level</span>
          <select v-model="filterLevel">
            <option value="">any</option>
            <option v-for="level in LOG_LEVELS" :key="level" :value="level">{{ level }}</option>
          </select>
        </label>
        <button type="submit" class="search-button">SEARCH</button>
        <button v-if="hasFilters" type="button" @click="clearFilters">CLEAR FILTERS</button>
        <span v-if="sinceMs !== null" class="chip">
          since {{ formatDateTime(sinceMs) }}
          <button type="button" aria-label="Clear time filter" @click="sinceMs = null">CLEAR</button>
        </span>
      </form>
    </div>

    <div v-if="streamError !== null && connection !== 'live'" class="notice notice--warn" role="status">
      Stream {{ connection }}: {{ streamError }}. REFRESH reconnects from cursor {{ formatCount(feed.cursor.value) }}.
    </div>
    <div v-if="lifecycleNotice !== null" class="notice notice--warn" role="status">
      Host lifecycle: {{ lifecycleNotice }}. The stream may be draining; displayed rows remain measured.
    </div>
    <div v-if="paused" class="notice notice--warn" role="status">
      VIEW FROZEN — ingestion continues into the live ring. RESUME replays the ring and catches up from cursor
      {{ formatCount(viewMeta.lastSeq) }}.
    </div>
    <div v-if="viewMeta.catchUp === 'catching-up'" class="notice notice--info" role="status">
      Resuming — replaying the ring and querying the persisted interval from cursor {{ formatCount(viewMeta.lastSeq) }}.
    </div>
    <div v-if="viewMeta.catchUp === 'incomplete'" class="notice notice--warn" role="status">
      Catch-up incomplete{{ viewMeta.catchUpError !== null ? `: ${viewMeta.catchUpError}` : '' }}. The displayed rows are
      real; retained history may be missing. Refresh to retry.
    </div>
    <div v-if="gapItems.length > 0" class="notice notice--warn" role="status">
      Cursor gap in the delivered stream — {{ gapItems.join(' · ') }}. This can be a dropped batch or retention loss;
      retained records remain visible.
    </div>
    <div v-if="displayMode === 'query'" class="notice notice--info" role="status">
      Server-side query replaces the live view. Live view is suspended at cursor {{ formatCount(viewMeta.lastSeq) }};
      BACK TO LIVE resumes from there.
    </div>

    <div class="log-body">
      <div class="log-header" role="presentation">
        <span>time</span>
        <span>level</span>
        <span>component</span>
        <span>trace</span>
        <span>message</span>
      </div>
      <div
        ref="scroller"
        class="log-scroller"
        role="listbox"
        aria-label="Log records"
        :aria-busy="queryLoading"
        tabindex="0"
        :aria-activedescendant="selectedSeq === null ? undefined : `log-row-${instanceId ?? 'x'}-${selectedSeq}`"
        @scroll="onScroll"
        @keydown="onKeydown"
      >
        <div v-if="total === 0" class="empty-state">
          <template v-if="displayMode === 'query'">
            <span v-if="queryLoading">Searching the host ledger…</span>
            <span v-else-if="queryCleared">Results cleared. Run SEARCH to query again.</span>
            <span v-else-if="queryError !== null" class="tone--bad">{{ queryError }}</span>
            <span v-else>No matching records (server-side query).</span>
          </template>
          <template v-else>
            <span v-if="connection === 'live'">No records in the retained live window yet.</span>
            <span v-else-if="connection === 'connecting'">Connecting to the event stream…</span>
            <span v-else-if="connection === 'retrying'">Stream retrying; no retained rows to display.</span>
            <span v-else class="tone--bad">Stream unavailable. No rows are being served.</span>
          </template>
        </div>
        <div class="log-spacer" :style="{ height: `${topPad}px` }" />
        <div
          v-for="(row, index) in visibleRows"
          :id="`log-row-${instanceId ?? 'x'}-${row.runtime_sequence}`"
          :key="row.runtime_sequence"
          class="log-row"
          :class="{ 'log-row--selected': selectedSeq === row.runtime_sequence }"
          role="option"
          :aria-selected="selectedSeq === row.runtime_sequence"
          :aria-posinset="visibleStart + index + 1"
          :aria-setsize="total"
          :style="{ height: `${ROW_HEIGHT}px` }"
          :title="`seq ${row.runtime_sequence}${row.caller !== null && row.caller !== undefined ? ` · ${row.caller}` : ''}`"
          @click="selectRow(row, true)"
        >
          <span class="cell cell--time nai-numbers">{{ formatTimestamp(row.timestamp_utc_ms) }}</span>
          <span class="cell cell--level" :class="levelCssClass(row.level)">{{ displayLevel(row.level) }}</span>
          <span class="cell cell--component">
            <button
              type="button"
              tabindex="-1"
              class="cell-link"
              title="Filter by this component"
              @click.stop="focusComponent(row.component)"
            >
              {{ row.component }}
            </button>
          </span>
          <span class="cell cell--trace">
            <button
              v-if="row.trace_id !== null && row.trace_id.length > 0"
              type="button"
              tabindex="-1"
              class="cell-link"
              :title="row.trace_id"
              @click.stop="focusTrace(row.trace_id)"
            >
              {{ shortId(row.trace_id) }}
            </button>
            <span v-else class="muted">—</span>
          </span>
          <span class="cell cell--message">{{ row.message }}</span>
        </div>
        <div class="log-spacer" :style="{ height: `${bottomPad}px` }" />
      </div>

      <div v-if="displayMode === 'query'" class="query-footer">
        <span>{{ queryLoading ? 'Searching…' : `${formatCount(total)} result rows` }}</span>
        <span v-if="queryState.truncated" class="tone--warn">result cache capped; oldest rows dropped</span>
        <span v-if="queryError !== null" class="tone--bad">{{ queryError }}</span>
        <span v-if="queryCleared" class="tone--warn">results cleared</span>
        <span class="toolbar-spacer" />
        <button
          v-if="queryState.hasMore || queryError !== null || queryCleared"
          type="button"
          :disabled="queryLoading"
          @click="queryCleared || queryError !== null ? rerunQuery() : loadMore()"
        >
          {{ queryCleared || queryError !== null ? 'RUN SEARCH' : 'LOAD MORE' }}
        </button>
      </div>
      <div v-else class="query-footer">
        <span v-if="viewMeta.truncated > 0" class="tone--warn">
          view holds the newest 5000 rows; {{ formatCount(viewMeta.truncated) }} older rows dropped from this view
        </span>
        <span v-else>Retained view rows are newest-last; follow is {{ follow ? 'on' : 'paused by scroll' }}.</span>
      </div>

      <RecordInspector
        :record="inspectedRecord"
        @close="closeInspector"
        @focus-trace="focusTraceFromInspector"
      />
    </div>
  </section>
</template>

<style scoped>
.log-tail {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  background: var(--nai-panel-2);
}

.log-toolbar {
  flex: 0 0 auto;
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
}

.toolbar-row {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  min-height: var(--nai-toolbar-h);
  padding: var(--nai-space-2) var(--nai-space-4);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
}

.stream-state {
  display: inline-flex;
  align-items: center;
  gap: var(--nai-space-3);
  font-size: var(--nai-font-size-xs);
  text-transform: uppercase;
  letter-spacing: 0.6px;
  white-space: nowrap;
}

.stream-state .dot {
  width: 6px;
  height: 6px;
  background: currentColor;
  box-shadow: 0 0 8px currentColor;
}

.mode-chip {
  height: 18px;
  padding: 0 var(--nai-space-3);
  border: var(--nai-hairline) solid var(--nai-line-strong);
  color: var(--nai-muted);
  font-size: 9px;
  letter-spacing: 0.6px;
  text-transform: uppercase;
  display: inline-flex;
  align-items: center;
}

.mode-chip--query {
  color: var(--nai-search);
  border-color: var(--nai-search);
}

.row-count {
  color: var(--nai-muted);
  font-size: var(--nai-font-size-xs);
  white-space: nowrap;
}

.behind {
  color: var(--nai-warn);
  font-size: 9px;
  letter-spacing: 0.4px;
  white-space: nowrap;
}

.toolbar-spacer {
  flex: 1 1 auto;
}

button {
  height: 22px;
  padding: 0 var(--nai-space-4);
  background: transparent;
  border: var(--nai-hairline) solid var(--nai-line);
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
  cursor: not-allowed;
}

.filter-row {
  display: flex;
  align-items: flex-end;
  gap: var(--nai-space-4);
  padding: var(--nai-space-2) var(--nai-space-4) var(--nai-space-3);
  flex-wrap: wrap;
}

.filter-field {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 110px;
}

.filter-field--grow {
  flex: 1 1 220px;
}

.filter-field span {
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.filter-field input,
.filter-field select {
  height: 22px;
  padding: 0 var(--nai-space-3);
  border: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel-2);
  color: var(--nai-fg);
  font-size: var(--nai-font-size-sm);
  border-radius: 0;
  min-width: 0;
}

.filter-field input:focus-visible,
.filter-field select:focus-visible {
  border-color: var(--nai-accent);
}

.search-button {
  color: var(--nai-accent);
  border-color: var(--nai-accent);
}

.chip {
  display: inline-flex;
  align-items: center;
  gap: var(--nai-space-3);
  height: 22px;
  padding: 0 var(--nai-space-3);
  border: var(--nai-hairline) solid var(--nai-search);
  color: var(--nai-search);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.chip button {
  height: 16px;
  padding: 0 var(--nai-space-2);
  border: none;
  color: var(--nai-search);
}

.notice {
  flex: 0 0 auto;
  padding: var(--nai-space-2) var(--nai-space-4);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-warn-bg);
  font-size: var(--nai-font-size-xs);
  line-height: 1.4;
}

.notice--warn {
  color: var(--nai-warn);
  border-left: 2px solid var(--nai-warn);
}

.notice--info {
  color: var(--nai-accent);
  border-left: 2px solid var(--nai-accent);
  background: var(--nai-accent-bg);
}

.log-body {
  position: relative;
  display: flex;
  flex-direction: column;
  flex: 1 1 auto;
  min-height: 0;
}

.log-header,
.log-row {
  display: grid;
  grid-template-columns: 92px 52px 150px 104px minmax(0, 1fr);
  align-items: center;
  gap: 0;
}

.log-header {
  flex: 0 0 auto;
  height: 20px;
  padding: 0 var(--nai-space-3);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.6px;
}

.log-header span {
  padding: 0 var(--nai-space-3);
  border-right: var(--nai-hairline) solid var(--nai-line);
}

.log-header span:last-child {
  border-right: none;
}

.log-scroller {
  position: relative;
  flex: 1 1 auto;
  min-height: 0;
  overflow: auto;
  outline: none;
  background: var(--nai-panel-2);
  font-size: 11.5px;
  line-height: 21px;
  scrollbar-gutter: stable;
}

.log-scroller:focus-visible {
  box-shadow: inset 0 0 0 1px var(--nai-focus);
}

.log-spacer {
  width: 1px;
  pointer-events: none;
}

.log-row {
  padding: 0 var(--nai-space-3);
  border-bottom: var(--nai-hairline) solid var(--nai-row-line);
  cursor: default;
  overflow: hidden;
  content-visibility: auto;
  contain-intrinsic-size: auto 22px;
}

.log-row:hover {
  background: rgba(25, 194, 216, 0.05);
}

.log-row--selected {
  background: var(--nai-selected);
  box-shadow: inset 2px 0 0 var(--nai-accent);
}

.cell {
  min-width: 0;
  padding: 0 var(--nai-space-3);
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.cell--time {
  color: var(--nai-muted);
}

.cell--level {
  font-weight: 700;
  font-size: 10px;
  letter-spacing: 0.4px;
}

.level--trace,
.level--debug {
  color: var(--nai-dim);
}

.level--info {
  color: var(--nai-accent);
}

.level--warn {
  color: var(--nai-warn);
}

.level--error {
  color: var(--nai-bad);
}

.cell--component,
.cell--trace {
  color: var(--nai-fg-soft);
}

.cell--message {
  color: var(--nai-fg);
}

.cell-link {
  height: auto;
  padding: 0;
  border: none;
  background: transparent;
  color: inherit;
  font-size: inherit;
  letter-spacing: normal;
  text-transform: none;
  text-align: left;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 100%;
}

.cell-link:hover {
  color: var(--nai-accent);
  text-decoration: underline;
  border: none;
  background: transparent;
}

.empty-state {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100%;
  min-height: 80px;
  color: var(--nai-muted);
  font-size: var(--nai-font-size-sm);
}

.query-footer {
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
  text-transform: uppercase;
  letter-spacing: 0.4px;
}

.muted {
  color: var(--nai-dim);
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
</style>
