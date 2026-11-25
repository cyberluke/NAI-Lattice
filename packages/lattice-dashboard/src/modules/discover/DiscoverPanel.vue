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
import { LOG_LEVELS, type LogRecord } from '../../telemetry/types';
import {
  DiscoverSession,
  canonicalFacetLevel,
  mergeFacetComponents,
  type DiscoverState,
  type FacetFilters,
} from '../../query/discover';
import RecordInspector from '../inspector/RecordInspector.vue';
import { useNow } from '../../shell/useNow';
import {
  displayLevel,
  formatAge,
  formatCount,
  formatTimestamp,
  levelCssClass,
  shortId,
} from '../../ui/format';

defineProps<{ instanceId?: string }>();

const context = inject(APP_CONTEXT_KEY);
if (context === undefined) throw new Error('discover context is missing');

const summaryRef = context.summary.summary;
const summaryLastFetchedAt = context.summary.lastFetchedAt;
const investigation = context.investigation;
const now = useNow();

const selectedLevels = ref<string[]>([]);
const selectedComponents = ref<string[]>([]);
const state = shallowRef<DiscoverState>({
  rows: [],
  loading: false,
  error: null,
  hasMore: false,
  nextAfter: null,
  scanned: 0,
  pages: 0,
  scanStart: 0,
  scanEnd: null,
  truncated: false,
  exhausted: false,
  cleared: false,
});
const inspected = shallowRef<LogRecord | null>(null);
const fetchedAt = ref<number | null>(null);

const ROW_HEIGHT = 22;
const OVERSCAN = 8;
const scroller = ref<HTMLElement | null>(null);
const scrollTop = ref(0);
const viewportHeight = ref(480);
let scrollFrame: number | null = null;
let resizeObserver: ResizeObserver | null = null;
let filterTimer: ReturnType<typeof setTimeout> | null = null;

const session = new DiscoverSession(context.query, (next) => {
  state.value = next;
  if (!next.loading) fetchedAt.value = Date.now();
});

const filterKey = computed(() =>
  JSON.stringify([selectedLevels.value, selectedComponents.value]),
);

function currentFilters(): FacetFilters {
  return { levels: [...selectedLevels.value], components: [...selectedComponents.value] };
}

function runFilters(): void {
  if (filterTimer !== null) {
    clearTimeout(filterTimer);
    filterTimer = null;
  }
  inspected.value = null;
  scrollTop.value = 0;
  if (scroller.value !== null) scroller.value.scrollTop = 0;
  void session.start(currentFilters());
}

function scheduleFilters(): void {
  if (filterTimer !== null) clearTimeout(filterTimer);
  filterTimer = setTimeout(() => {
    filterTimer = null;
    runFilters();
  }, 250);
}

watch(filterKey, () => scheduleFilters());

const rows = computed(() => state.value.rows);
const total = computed(() => rows.value.length);
const visibleStart = computed(() =>
  Math.max(0, Math.floor(scrollTop.value / ROW_HEIGHT) - OVERSCAN),
);
const visibleEnd = computed(() =>
  Math.min(
    total.value,
    Math.ceil(
      (scrollTop.value + Math.max(viewportHeight.value, ROW_HEIGHT)) / ROW_HEIGHT,
    ) + OVERSCAN,
  ),
);
const visibleRows = computed(() => rows.value.slice(visibleStart.value, visibleEnd.value));
const topPad = computed(() => visibleStart.value * ROW_HEIGHT);
const bottomPad = computed(() =>
  Math.max(0, total.value * ROW_HEIGHT - visibleEnd.value * ROW_HEIGHT),
);

const age = computed(() => formatAge(fetchedAt.value, now.value));
const summaryAge = computed(() => formatAge(summaryLastFetchedAt.value, now.value));

const facetComponents = computed(() => {
  const merged = mergeFacetComponents(summaryRef.value?.components ?? {}, rows.value);
  const known = new Set(merged.map((facet) => facet.name));
  const selectedOnly = selectedComponents.value
    .filter((name) => !known.has(name))
    .map((name) => ({
      name,
      loaded: 0,
      summary: summaryRef.value?.components[name] ?? null,
    }));
  return [...merged, ...selectedOnly];
});

function loadedLevelCount(level: string): number {
  return rows.value.reduce(
    (count, row) => (canonicalFacetLevel(row.level) === level ? count + 1 : count),
    0,
  );
}

function summaryLevelCount(level: string): number | null {
  const levels = summaryRef.value?.levels ?? null;
  if (levels === null) return null;
  return Object.prototype.hasOwnProperty.call(levels, level) ? levels[level] : null;
}

const windowLabel = computed(() => {
  const info = state.value;
  if (info.scanEnd === null) {
    return info.scanned > 0
      ? `after seq ${formatCount(info.scanStart)}`
      : 'no records scanned';
  }
  return `scanned seq ${formatCount(info.scanStart)}–${formatCount(info.scanEnd)}`;
});

function onScroll(): void {
  const element = scroller.value;
  if (element === null) return;
  if (scrollFrame !== null) return;
  const apply = (): void => {
    scrollFrame = null;
    if (scroller.value === null) return;
    scrollTop.value = scroller.value.scrollTop;
  };
  if (typeof requestAnimationFrame === 'function') scrollFrame = requestAnimationFrame(apply);
  else apply();
}

function onResize(): void {
  const element = scroller.value;
  if (element === null) return;
  viewportHeight.value = element.clientHeight;
}

function loadMore(): void {
  void session.loadMore();
}

function clearFacets(): void {
  selectedLevels.value = [];
  selectedComponents.value = [];
}

function selectRow(row: LogRecord): void {
  inspected.value = row;
  void context!.interop.selectLog(row);
}

function closeInspector(): void {
  inspected.value = null;
}

function focusTrace(traceId: string): void {
  investigation.setTrace(traceId);
  investigation.requestPanel('traces');
  inspected.value = null;
}

onMounted(async () => {
  void session.start(currentFilters());
  await nextTick();
  const element = scroller.value;
  if (element !== null) {
    viewportHeight.value = element.clientHeight || 480;
  }
  if (typeof ResizeObserver !== 'undefined' && element !== null) {
    resizeObserver = new ResizeObserver(onResize);
    resizeObserver.observe(element);
  }
});

onBeforeUnmount(() => {
  session.dispose();
  if (filterTimer !== null) clearTimeout(filterTimer);
  if (scrollFrame !== null && typeof cancelAnimationFrame === 'function') {
    cancelAnimationFrame(scrollFrame);
  }
  if (resizeObserver !== null) {
    resizeObserver.disconnect();
    resizeObserver = null;
  }
});
</script>

<template>
  <section class="discover-panel" aria-label="Discover">
    <header class="panel-toolbar">
      <span class="panel-title">discover</span>
      <span class="panel-meta">
        {{ formatCount(total) }} match(es) · scanned {{ formatCount(state.scanned) }}
      </span>
      <span class="panel-meta">{{ windowLabel }}</span>
      <span class="toolbar-spacer" />
      <span v-if="state.error !== null" class="tone--bad" :title="state.error">query failed</span>
      <span v-else-if="state.loading" class="panel-meta">scanning…</span>
      <span v-else class="panel-meta">measured {{ age }}</span>
      <button type="button" :disabled="state.loading" @click="runFilters">REFRESH</button>
    </header>

    <div class="discover-body">
      <aside class="facet-column" aria-label="Facets">
        <div class="facet-head">
          <span>levels</span>
          <button
            v-if="selectedLevels.length > 0 || selectedComponents.length > 0"
            type="button"
            class="tiny"
            @click="clearFacets"
          >
            CLEAR
          </button>
        </div>
        <div class="facet-list facet-list--levels">
          <label v-for="level in LOG_LEVELS" :key="level" class="facet-item">
            <input v-model="selectedLevels" type="checkbox" :value="level" />
            <span class="facet-name" :class="levelCssClass(level)">{{ displayLevel(level) }}</span>
            <span class="facet-count nai-numbers">
              {{ formatCount(loadedLevelCount(level)) }}
              <template v-if="summaryLevelCount(level) !== null">
                / {{ formatCount(summaryLevelCount(level)) }}
              </template>
            </span>
          </label>
        </div>

        <div class="facet-head">
          <span>components</span>
          <span class="facet-note">loaded / summary</span>
        </div>
        <div class="facet-list facet-list--components">
          <label v-for="facet in facetComponents" :key="facet.name" class="facet-item">
            <input v-model="selectedComponents" type="checkbox" :value="facet.name" />
            <span class="facet-name" :title="facet.name">{{ facet.name }}</span>
            <span class="facet-count nai-numbers">
              {{ formatCount(facet.loaded) }}
              <template v-if="facet.summary !== null">/ {{ formatCount(facet.summary) }}</template>
            </span>
          </label>
          <span v-if="facetComponents.length === 0" class="facet-empty">
            No component facet measured yet.
          </span>
        </div>
        <div class="facet-foot">
          <span class="muted">
            summary {{ summaryAge }} · empty selection means all levels and components.
          </span>
          <span class="muted">counts are from the loaded result and the summary window only.</span>
        </div>
      </aside>

      <div class="result-column">
        <div class="result-head" role="presentation">
          <span>time</span>
          <span>level</span>
          <span>component</span>
          <span>trace</span>
          <span>message</span>
          <span>seq</span>
        </div>
        <div
          ref="scroller"
          class="result-scroller"
          role="listbox"
          aria-label="Discover records"
          :aria-busy="state.loading"
          tabindex="0"
          @scroll="onScroll"
        >
          <div v-if="total === 0" class="result-empty">
            <template v-if="state.loading">Scanning the host ledger…</template>
            <span v-else-if="state.error !== null" class="tone--bad">{{ state.error }}</span>
            <template v-else-if="state.cleared">Result cleared.</template>
            <template v-else>
              No records match the selected facets in the scanned window.
            </template>
          </div>
          <div class="result-spacer" :style="{ height: `${topPad}px` }" />
          <div
            v-for="(row, index) in visibleRows"
            :key="row.runtime_sequence"
            class="result-row"
            :class="{ 'result-row--selected': inspected?.runtime_sequence === row.runtime_sequence }"
            role="option"
            :aria-selected="inspected?.runtime_sequence === row.runtime_sequence"
            :aria-posinset="visibleStart + index + 1"
            :aria-setsize="total"
            :style="{ height: `${ROW_HEIGHT}px` }"
            @click="selectRow(row)"
          >
            <span class="cell nai-numbers">{{ formatTimestamp(row.timestamp_utc_ms) }}</span>
            <span class="cell level" :class="levelCssClass(row.level)">{{ displayLevel(row.level) }}</span>
            <span class="cell">{{ row.component }}</span>
            <span class="cell">
              <span v-if="row.trace_id !== null && row.trace_id.length > 0" :title="row.trace_id">
                {{ shortId(row.trace_id) }}
              </span>
              <span v-else class="muted">—</span>
            </span>
            <span class="cell message">{{ row.message }}</span>
            <span class="cell nai-numbers seq">{{ row.runtime_sequence }}</span>
          </div>
          <div class="result-spacer" :style="{ height: `${bottomPad}px` }" />
        </div>

        <div class="result-foot">
          <span class="nai-numbers">
            {{ formatCount(total) }} loaded · scanned {{ formatCount(state.scanned) }} ·
            {{ formatCount(state.pages) }} page(s)
          </span>
          <span v-if="state.truncated" class="tone--warn">display cap reached; oldest loaded rows dropped</span>
          <span v-else-if="!state.hasMore && !state.loading && total > 0" class="muted">
            end of the scanned window
          </span>
          <span class="toolbar-spacer" />
          <button type="button" :disabled="state.loading || !state.hasMore" @click="loadMore">
            {{ state.loading ? 'LOADING…' : 'LOAD MORE' }}
          </button>
        </div>
      </div>

      <RecordInspector :record="inspected" @close="closeInspector" @focus-trace="focusTrace" />
    </div>
  </section>
</template>

<style scoped>
.discover-panel {
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
}

.panel-meta {
  color: var(--nai-dim);
  font-size: 9px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
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
  opacity: 0.4;
  cursor: not-allowed;
}

.discover-body {
  position: relative;
  display: flex;
  flex: 1 1 auto;
  min-height: 0;
  overflow: hidden;
}

.facet-column {
  display: flex;
  flex-direction: column;
  flex: 0 0 220px;
  min-height: 0;
  border-right: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
}

.facet-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--nai-space-3);
  flex: 0 0 auto;
  min-height: 22px;
  padding: 0 var(--nai-space-4);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.6px;
}

.facet-note {
  color: var(--nai-dim);
  font-size: 8px;
  letter-spacing: 0.3px;
}

.tiny {
  height: 16px;
  padding: 0 var(--nai-space-2);
  font-size: 8px;
}

.facet-list {
  flex: 0 1 auto;
  min-height: 44px;
  overflow: auto;
  scrollbar-gutter: stable;
  border-bottom: var(--nai-hairline) solid var(--nai-line);
}

.facet-list--levels {
  max-height: 132px;
}

.facet-list--components {
  flex: 1 1 auto;
}

.facet-item {
  display: grid;
  grid-template-columns: 14px minmax(0, 1fr) auto;
  align-items: center;
  gap: var(--nai-space-2);
  min-height: 20px;
  padding: 0 var(--nai-space-4);
  color: var(--nai-muted);
  font-size: 10px;
  cursor: pointer;
}

.facet-item:hover {
  background: var(--nai-hover);
}

.facet-item input {
  accent-color: var(--nai-accent);
  margin: 0;
}

.facet-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.facet-count {
  color: var(--nai-dim);
  font-size: 9px;
}

.facet-empty {
  display: block;
  padding: var(--nai-space-3) var(--nai-space-4);
  color: var(--nai-dim);
  font-size: 10px;
}

.facet-foot {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-2);
  flex: 0 0 auto;
  padding: var(--nai-space-3) var(--nai-space-4);
  border-top: var(--nai-hairline) solid var(--nai-line);
}

.result-column {
  display: flex;
  flex-direction: column;
  flex: 1 1 auto;
  min-width: 0;
  min-height: 0;
}

.result-head,
.result-row {
  display: grid;
  grid-template-columns: 92px 52px 150px 96px minmax(0, 1fr) 64px;
  align-items: center;
  gap: 0;
}

.result-head {
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

.result-head span {
  padding: 0 var(--nai-space-3);
  border-right: var(--nai-hairline) solid var(--nai-line);
}

.result-head span:last-child {
  border-right: none;
}

.result-scroller {
  position: relative;
  flex: 1 1 auto;
  min-height: 0;
  overflow: auto;
  outline: none;
  scrollbar-gutter: stable;
  font-size: 11.5px;
  line-height: 21px;
  background: var(--nai-panel-2);
}

.result-scroller:focus-visible {
  box-shadow: inset 0 0 0 1px var(--nai-focus);
}

.result-empty {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100%;
  min-height: 80px;
  padding: var(--nai-space-4);
  color: var(--nai-muted);
  font-size: var(--nai-font-size-sm);
  text-align: center;
}

.result-spacer {
  width: 1px;
  pointer-events: none;
}

.result-row {
  padding: 0 var(--nai-space-3);
  border-bottom: var(--nai-hairline) solid var(--nai-row-line);
  cursor: pointer;
  overflow: hidden;
  content-visibility: auto;
  contain-intrinsic-size: auto 22px;
}

.result-row:hover {
  background: rgba(25, 194, 216, 0.05);
}

.result-row--selected {
  background: var(--nai-selected);
  box-shadow: inset 2px 0 0 var(--nai-accent);
}

.cell {
  min-width: 0;
  padding: 0 var(--nai-space-3);
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
  color: var(--nai-muted);
}

.level {
  font-weight: 700;
  font-size: 10px;
  letter-spacing: 0.4px;
}

.message {
  color: var(--nai-fg);
}

.seq {
  color: var(--nai-dim);
  text-align: right;
}

.result-foot {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex: 0 0 auto;
  min-height: 26px;
  padding: 0 var(--nai-space-4);
  border-top: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.3px;
  overflow: hidden;
}

.result-foot > span {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.muted {
  color: var(--nai-dim);
}

.tone--warn {
  color: var(--nai-warn);
}

.tone--bad {
  color: var(--nai-bad);
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
</style>
