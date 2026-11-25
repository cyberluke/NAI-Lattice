<script setup lang="ts">
import { computed, inject } from 'vue';
import { APP_CONTEXT_KEY } from '../../app-context';
import { formatAge, formatCount, formatDateTime, formatUptime } from '../../ui/format';
import { useNow } from '../../shell/useNow';

defineProps<{ instanceId?: string }>();

const context = inject(APP_CONTEXT_KEY);
if (context === undefined) throw new Error('health context is missing');

const summaryRef = context.summary.summary;
const error = context.summary.error;
const stale = context.summary.stale;
const lastFetchedAt = context.summary.lastFetchedAt;
const loading = context.summary.loading;
const now = useNow();
const fdc3Status = context.interop.status;
const fdc3Detail = context.interop.detail;
const fdc3Error = context.interop.lastError;
const fdc3Agent = context.interop.agentName;
const fdc3Channel = context.interop.channel;
const fdc3ChannelMode = context.interop.channelMode;
const fdc3Label = computed(() => {
  switch (fdc3Status.value) {
    case 'ready':
      return 'ready';
    case 'unavailable':
      return 'no desktop agent';
    case 'error':
      return 'listener error';
    default:
      return 'discovering…';
  }
});
const fdc3Tone = computed(() => {
  switch (fdc3Status.value) {
    case 'ready':
      return 'ok';
    case 'error':
      return 'bad';
    case 'initializing':
      return 'warn';
    default:
      return 'muted';
  }
});

const health = computed(() => summaryRef.value?.health ?? null);
const stream = computed(() => summaryRef.value?.stream ?? null);
const version = computed(() => summaryRef.value?.version ?? null);
const features = computed(() => summaryRef.value?.features ?? []);
const age = computed(() => formatAge(lastFetchedAt.value, now.value));

const healthTone = computed(() => {
  const state = (health.value?.state ?? '').toLowerCase();
  if (state === 'healthy' || state === 'ok') return 'ok';
  if (state === 'degraded' || state === 'draining' || state === 'starting') return 'warn';
  if (state === 'unhealthy' || state === 'error' || state === 'failed') return 'bad';
  return 'muted';
});

function valueOrDash(value: string | number | null | undefined): string {
  if (value === null || value === undefined) return '—';
  if (typeof value === 'number') return formatCount(value);
  return value.length > 0 ? value : '—';
}

function refresh(): void {
  void context?.summary.refresh(true);
}
</script>

<template>
  <section class="health-panel" aria-label="Host health and stream">
    <div class="panel-toolbar">
      <span class="panel-title">host health</span>
      <span class="health-state" :class="`tone--${healthTone}`">
        {{ health?.state ?? 'unknown' }}
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
    <div v-else class="health-scroll nai-scroll">
      <section class="block">
        <h2>lifecycle</h2>
        <dl class="field-grid">
          <div><dt>state</dt><dd :class="`tone--${healthTone}`">{{ valueOrDash(health?.state) }}</dd></div>
          <div><dt>detail</dt><dd>{{ valueOrDash(health?.detail) }}</dd></div>
          <div><dt>last error</dt><dd :class="{ 'tone--bad': health?.lastError }">{{ valueOrDash(health?.lastError) }}</dd></div>
          <div><dt>since</dt><dd class="nai-numbers">{{ formatDateTime(health?.sinceUnixMs ?? null) }}</dd></div>
          <div><dt>uptime</dt><dd class="nai-numbers">{{ formatUptime(health?.uptimeMs ?? null) }}</dd></div>
          <div><dt>summary window</dt><dd class="nai-numbers">{{ formatCount(summaryRef?.window ?? null) }}</dd></div>
        </dl>
      </section>

      <section class="block">
        <h2>stream</h2>
        <dl class="field-grid">
          <div><dt>sse clients</dt><dd class="nai-numbers">{{ valueOrDash(stream?.activeSseClients) }} / {{ valueOrDash(stream?.maxSseClients) }}</dd></div>
          <div><dt>subscribers</dt><dd class="nai-numbers">{{ valueOrDash(stream?.subscribers) }}</dd></div>
          <div><dt>pushes</dt><dd class="nai-numbers">{{ valueOrDash(stream?.pushTotal) }}</dd></div>
          <div><dt>broker scans</dt><dd class="nai-numbers">{{ valueOrDash(stream?.brokerScansTotal) }}</dd></div>
          <div><dt>dropped</dt><dd class="nai-numbers" :class="{ 'tone--bad': (stream?.droppedTotal ?? 0) > 0 }">{{ valueOrDash(stream?.droppedTotal) }}</dd></div>
          <div><dt>last scan</dt><dd class="nai-numbers">{{ formatDateTime(stream?.lastScanUnixMs ?? null) }}</dd></div>
        </dl>
      </section>

      <section class="block">
        <h2>interop</h2>
        <dl class="field-grid">
          <div><dt>fdc3</dt><dd :class="`tone--${fdc3Tone}`">{{ fdc3Label }}</dd></div>
          <div><dt>agent</dt><dd>{{ valueOrDash(fdc3Agent) }}</dd></div>
          <div>
            <dt>channel</dt>
            <dd>
              {{ fdc3Channel !== null ? `#${fdc3Channel}` : fdc3ChannelMode === 'default' ? 'default channel' : '—' }}
            </dd>
          </div>
          <div><dt>detail</dt><dd>{{ valueOrDash(fdc3Detail) }}</dd></div>
          <div v-if="fdc3Error !== null"><dt>error</dt><dd class="tone--bad">{{ fdc3Error }}</dd></div>
        </dl>
        <p class="honest-gap">
          Row selection raises <code>nai.lattice.logSelection</code> v1 only when a Desktop Agent is connected. Without
          an agent the dashboard stays fully usable; no fake agent is created.
        </p>
      </section>

      <section class="block">
        <h2>version</h2>
        <dl class="field-grid">
          <div><dt>host</dt><dd>{{ valueOrDash(version?.host) }}</dd></div>
          <div><dt>dashboard</dt><dd>{{ valueOrDash(version?.dashboard) }}</dd></div>
          <div><dt>protocol</dt><dd class="nai-numbers">{{ valueOrDash(version?.protocolMin) }}–{{ valueOrDash(version?.protocolMax) }}</dd></div>
          <div><dt>storage schema</dt><dd class="nai-numbers">{{ valueOrDash(version?.storageSchema) }}</dd></div>
          <div><dt>features</dt><dd>{{ features.length > 0 ? features.join(' · ') : '—' }}</dd></div>
        </dl>
      </section>

      <section class="block block--notes">
        <h2>readings</h2>
        <dl class="field-grid">
          <div><dt>levels</dt><dd class="nai-numbers">{{ summaryRef?.levels?.info ?? '—' }} info · {{ summaryRef?.errors ?? '—' }} warn+error</dd></div>
          <div><dt>traces</dt><dd class="nai-numbers">{{ formatCount(summaryRef?.distinctTraceIds ?? null) }} distinct</dd></div>
          <div><dt>age</dt><dd class="nai-numbers">{{ age }}</dd></div>
        </dl>
        <p class="honest-gap">
          Unknown values are shown as “—” and never synthesized. Storage budget, class sizes and retention live in the
          Storage module.
        </p>
      </section>
    </div>
  </section>
</template>

<style scoped>
.health-panel {
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

.health-state {
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.6px;
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

.health-scroll {
  flex: 1 1 auto;
  min-height: 0;
  overflow: auto;
  scrollbar-gutter: stable;
}

.block {
  padding: var(--nai-space-4) var(--nai-space-5);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
}

.block h2 {
  margin-bottom: var(--nai-space-3);
  color: var(--nai-accent);
  font-size: 9px;
  font-weight: 400;
  text-transform: uppercase;
  letter-spacing: 0.8px;
}

.field-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
  gap: var(--nai-space-2) var(--nai-space-5);
  margin: 0;
}

.field-grid > div {
  display: flex;
  align-items: baseline;
  gap: var(--nai-space-3);
  min-width: 0;
}

.field-grid dt {
  flex: 0 0 86px;
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.4px;
}

.field-grid dd {
  margin: 0;
  min-width: 0;
  color: var(--nai-fg);
  font-size: var(--nai-font-size-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.honest-gap {
  color: var(--nai-muted);
  font-size: 10px;
  line-height: 1.5;
}

code {
  color: var(--nai-search);
  font-family: inherit;
}

.block--notes .honest-gap {
  margin-top: var(--nai-space-3);
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
