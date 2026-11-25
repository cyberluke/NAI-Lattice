<script setup lang="ts">
import { computed, inject } from 'vue';
import { APP_CONTEXT_KEY } from '../../app-context';
import {
  STORAGE_CLASS_KEYS,
  type StorageClassKey,
  type StorageInfo,
  type StoragePressure,
} from '../../telemetry/summary';
import { useNow } from '../../shell/useNow';
import {
  formatAge,
  formatBytes,
  formatCount,
  formatDateTime,
  formatPercent,
} from '../../ui/format';

defineProps<{ instanceId?: string }>();

const context = inject(APP_CONTEXT_KEY);
if (context === undefined) throw new Error('storage context is missing');

const CLASS_LABELS: Record<StorageClassKey, string> = {
  ledger: 'ledger',
  cas: 'cas',
  hot: 'hot',
  search: 'search',
  vector: 'vector',
  readModels: 'read models',
  logs: 'logs',
  backups: 'backups',
  audit: 'audit',
};

const summaryRef = context.summary.summary;
const error = context.summary.error;
const stale = context.summary.stale;
const lastFetchedAt = context.summary.lastFetchedAt;
const loading = context.summary.loading;
const now = useNow();

const storage = computed<StorageInfo | null>(() => summaryRef.value?.storage ?? null);
const pressure = computed<StoragePressure>(() => storage.value?.pressure ?? 'unknown');
const age = computed(() => formatAge(lastFetchedAt.value, now.value));

const usedPercent = computed<number | null>(() => {
  const used = storage.value?.usedBytes ?? null;
  const budget = storage.value?.budgetBytes ?? null;
  if (used === null || budget === null || budget <= 0) return null;
  return (used / budget) * 100;
});
const barWidth = computed(() =>
  usedPercent.value === null ? 0 : Math.max(0, Math.min(100, usedPercent.value)),
);
const overBudget = computed(() => {
  const used = storage.value?.usedBytes ?? null;
  const budget = storage.value?.budgetBytes ?? null;
  return used !== null && budget !== null && used > budget;
});

interface ClassRow {
  key: StorageClassKey;
  label: string;
  bytes: number | null;
  sharePercent: number | null;
  shareWidth: number;
}

const classRows = computed<ClassRow[]>(() => {
  const classes = storage.value?.classes ?? null;
  if (classes === null) return [];
  const used = storage.value?.usedBytes ?? null;
  return STORAGE_CLASS_KEYS.map((key) => {
    const bytes = classes[key];
    const sharePercent =
      bytes !== null && used !== null && used > 0 ? (bytes / used) * 100 : null;
    return {
      key,
      label: CLASS_LABELS[key],
      bytes,
      sharePercent,
      shareWidth: sharePercent === null ? 0 : Math.max(0, Math.min(100, sharePercent)),
    };
  });
});

function daysLabel(value: number | null): string {
  return value === null ? '—' : `${formatCount(value)} d`;
}

function keepLabel(value: number | null): string {
  return value === null ? '—' : `${formatCount(value)} versions`;
}

function bytesOrDash(value: number | null): string {
  return value === null ? '—' : formatBytes(value);
}

function refresh(): void {
  void context?.summary.refresh(true);
}
</script>

<template>
  <section class="storage-panel" aria-label="Storage budget, classes and retention">
    <div class="panel-toolbar">
      <span class="panel-title">storage</span>
      <span v-if="storage !== null" class="pressure" :class="`pressure--${pressure}`">
        {{ pressure }}
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
    <div v-else-if="summaryRef === null" class="panel-state">
      <span v-if="loading">Loading host summary…</span>
      <span v-else>Host summary is unavailable.</span>
    </div>
    <div v-else class="storage-scroll nai-scroll">
      <div v-if="storage === null" class="storage-unwired" role="status">
        <strong>storage not wired by this host</strong>
        <span>
          The summary does not expose <code>summary.storage</code> yet. No budget, class size or
          retention number is invented here.
        </span>
      </div>

      <template v-else>
        <section class="block">
          <div class="block-head">
            <h2>budget</h2>
            <span class="pressure" :class="`pressure--${pressure}`">{{ pressure }}</span>
            <span v-if="overBudget" class="over-budget">over budget</span>
          </div>
          <div class="budget-figures">
            <span class="budget-used nai-numbers">{{ bytesOrDash(storage.usedBytes) }}</span>
            <span class="budget-divider">/</span>
            <span class="budget-budget nai-numbers">{{ bytesOrDash(storage.budgetBytes) }}</span>
            <span class="budget-percent nai-numbers">{{ formatPercent(usedPercent) }}</span>
          </div>
          <div
            class="budget-track"
            :class="`budget-track--${pressure}`"
            role="meter"
            aria-label="Storage used against the host budget"
            aria-valuemin="0"
            :aria-valuemax="storage.budgetBytes ?? undefined"
            :aria-valuenow="storage.usedBytes ?? undefined"
          >
            <div class="budget-fill" :style="{ width: `${barWidth}%` }"></div>
          </div>
          <dl class="stat-grid">
            <div><dt>free</dt><dd class="nai-numbers">{{ bytesOrDash(storage.freeBytes) }}</dd></div>
            <div><dt>reserve</dt><dd class="nai-numbers">{{ bytesOrDash(storage.reserveBytes) }}</dd></div>
            <div class="path-field"><dt>root</dt><dd :title="storage.root ?? undefined">{{ storage.root ?? '—' }}</dd></div>
            <div class="path-field"><dt>base</dt><dd :title="storage.base ?? undefined">{{ storage.base ?? '—' }}</dd></div>
          </dl>
        </section>

        <section class="block">
          <h2>classes</h2>
          <p v-if="storage.classes === null" class="honest-gap">
            The host did not include a class breakdown; no sizes are synthesized.
          </p>
          <table v-else class="class-table">
            <thead>
              <tr>
                <th scope="col">class</th>
                <th scope="col">size</th>
                <th scope="col">share</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="row in classRows" :key="row.key">
                <td>{{ row.label }}</td>
                <td class="nai-numbers">{{ bytesOrDash(row.bytes) }}</td>
                <td>
                  <div class="share-cell">
                    <span class="share-track" aria-hidden="true">
                      <i :style="{ width: `${row.shareWidth}%` }"></i>
                    </span>
                    <span class="share-value nai-numbers">{{ formatPercent(row.sharePercent) }}</span>
                  </div>
                </td>
              </tr>
            </tbody>
          </table>
        </section>

        <section class="block">
          <h2>retention</h2>
          <p v-if="storage.retention === null" class="honest-gap">
            The host did not report retention policy values.
          </p>
          <dl v-else class="stat-grid">
            <div><dt>logs</dt><dd class="nai-numbers">{{ daysLabel(storage.retention.logsDays) }}</dd></div>
            <div><dt>audit</dt><dd class="nai-numbers">{{ daysLabel(storage.retention.auditDays) }}</dd></div>
            <div><dt>backups</dt><dd class="nai-numbers">{{ daysLabel(storage.retention.backupDays) }}</dd></div>
            <div><dt>runtime keep</dt><dd class="nai-numbers">{{ keepLabel(storage.retention.runtimeKeep) }}</dd></div>
          </dl>
        </section>

        <section class="block">
          <h2>last prune</h2>
          <p v-if="storage.lastPrune === null" class="honest-gap">No prune audit record exists.</p>
          <dl v-else class="stat-grid">
            <div><dt>ran</dt><dd class="nai-numbers">{{ formatDateTime(storage.lastPrune.runUnixMs) }}</dd></div>
            <div><dt>freed</dt><dd class="nai-numbers">{{ bytesOrDash(storage.lastPrune.bytesFreed) }}</dd></div>
          </dl>
        </section>
      </template>
    </div>
  </section>
</template>

<style scoped>
.storage-panel {
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
  align-items: center;
  justify-content: center;
  gap: var(--nai-space-5);
  flex: 1 1 auto;
  padding: var(--nai-space-5);
  color: var(--nai-muted);
  font-size: var(--nai-font-size-sm);
}

.panel-state--error {
  color: var(--nai-bad);
}

.storage-scroll {
  flex: 1;
  min-height: 0;
  overflow-x: hidden;
  overflow-y: auto;
  overscroll-behavior: contain;
  /* Keep the last row (and its border) clear of the panel's bottom edge. */
  padding-bottom: var(--nai-space-6);
  scrollbar-gutter: stable;
}

.storage-unwired {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-3);
  margin: var(--nai-space-5);
  padding: var(--nai-space-5);
  border: var(--nai-hairline) solid var(--nai-line-strong);
  border-left: 2px solid var(--nai-dim);
  border-radius: 0;
  background: var(--nai-panel);
  color: var(--nai-muted);
  font-size: var(--nai-font-size-sm);
  line-height: 1.5;
}

.storage-unwired strong {
  color: var(--nai-fg);
  font-weight: 400;
  text-transform: uppercase;
  letter-spacing: 0.6px;
  font-size: 10px;
}

.storage-unwired code {
  color: var(--nai-search);
  font-family: inherit;
}

.block {
  padding: var(--nai-space-4) var(--nai-space-5);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
}

.block-head {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  margin-bottom: var(--nai-space-3);
}

.block h2 {
  margin: 0 0 var(--nai-space-3);
  color: var(--nai-accent);
  font-size: 9px;
  font-weight: 400;
  text-transform: uppercase;
  letter-spacing: 0.8px;
}

.block-head h2 {
  margin: 0;
}

.pressure {
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.6px;
}

.pressure--ok {
  color: var(--nai-ok);
}

.pressure--warn {
  color: var(--nai-warn);
}

.pressure--bad {
  color: var(--nai-bad);
}

.pressure--muted,
.pressure--unknown {
  color: var(--nai-dim);
}

.over-budget {
  padding: 0 var(--nai-space-3);
  border: var(--nai-hairline) solid var(--nai-bad);
  color: var(--nai-bad);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.6px;
}

.budget-figures {
  display: flex;
  align-items: baseline;
  gap: var(--nai-space-3);
  margin-bottom: var(--nai-space-3);
}

.budget-used {
  color: var(--nai-fg);
  font-size: 18px;
  font-weight: 700;
}

.budget-divider,
.budget-budget {
  color: var(--nai-muted);
  font-size: var(--nai-font-size-sm);
}

.budget-percent {
  margin-left: auto;
  color: var(--nai-fg-soft);
  font-size: var(--nai-font-size-sm);
  font-variant-numeric: var(--nai-tabular);
}

.budget-track {
  height: 6px;
  border: var(--nai-hairline) solid var(--nai-line);
  border-radius: 0;
  background: var(--nai-bar-track);
  overflow: hidden;
}

.budget-fill {
  height: 100%;
  background: var(--nai-bar-start);
}

.budget-track--elevated .budget-fill {
  background: var(--nai-warn);
}

.budget-track--critical .budget-fill {
  background: var(--nai-bad);
}

.budget-track--unknown .budget-fill {
  background: var(--nai-dim);
}

.stat-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
  gap: var(--nai-space-2) var(--nai-space-5);
  margin: var(--nai-space-4) 0 0;
}

.stat-grid > div {
  display: flex;
  align-items: baseline;
  gap: var(--nai-space-3);
  min-width: 0;
}

.stat-grid dt {
  flex: 0 0 86px;
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.4px;
}

.stat-grid dd {
  margin: 0;
  min-width: 0;
  color: var(--nai-fg);
  font-size: var(--nai-font-size-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.class-table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--nai-font-size-sm);
  table-layout: fixed;
}

.class-table th {
  padding: var(--nai-space-2) var(--nai-space-3);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  color: var(--nai-dim);
  font-size: 9px;
  font-weight: 400;
  text-transform: uppercase;
  letter-spacing: 0.6px;
  text-align: left;
}

.class-table th:first-child {
  width: 34%;
}

.class-table th:nth-child(2) {
  width: 26%;
}

.class-table td {
  padding: 2px var(--nai-space-3);
  border-bottom: var(--nai-hairline) solid var(--nai-row-line);
  color: var(--nai-muted);
  white-space: nowrap;
}

.share-cell {
  display: flex;
  align-items: center;
  gap: var(--nai-space-3);
}

.share-track {
  flex: 1 1 auto;
  height: 4px;
  background: var(--nai-bar-track);
  overflow: hidden;
}

.share-track i {
  display: block;
  height: 100%;
  background: var(--nai-bar-start);
}

.share-value {
  flex: 0 0 48px;
  text-align: right;
  color: var(--nai-dim);
  font-size: 10px;
}

.honest-gap {
  margin: 0;
  color: var(--nai-muted);
  font-size: 10px;
  line-height: 1.5;
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

.path-field dd {
  font-size: 10px;
  color: var(--nai-fg-soft);
}
</style>
