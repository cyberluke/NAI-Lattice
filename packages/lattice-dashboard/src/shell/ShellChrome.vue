<script setup lang="ts">
import { computed, inject, nextTick, onBeforeUnmount, onMounted, ref } from 'vue';
import { APP_CONTEXT_KEY } from '../app-context';
import { formatAge, formatClock, formatCount, formatUptime } from '../ui/format';
import { parseDeepLinks } from './deeplinks';
import { useNow } from './useNow';
import type { WorkspaceAction } from './workspace-actions';

const emit = defineEmits<{ action: [action: WorkspaceAction] }>();

const context = inject(APP_CONTEXT_KEY);
if (context === undefined) throw new Error('shell context is missing');

/**
 * NAI OS owns native windows in embed mode, so a same-origin Dockview popout is
 * not offered there. The dashboard page keeps it. App also omits the whole
 * chrome in embed, but this guard keeps the contract local to the controls.
 */
const embed =
  typeof window === 'undefined' ? false : parseDeepLinks(window.location.search).embed;

/** RESET discards the saved layout, so it takes an explicit second step. */
const resetArmed = ref(false);
const resetButton = ref<HTMLButtonElement | null>(null);
const confirmButton = ref<HTMLButtonElement | null>(null);
let resetTimer: ReturnType<typeof setTimeout> | null = null;

function clearResetTimer(): void {
  if (resetTimer !== null) {
    clearTimeout(resetTimer);
    resetTimer = null;
  }
}

function armReset(): void {
  resetArmed.value = true;
  clearResetTimer();
  // The armed RESET button is replaced, so move focus to the explicit choice.
  void nextTick(() => confirmButton.value?.focus());
  resetTimer = setTimeout(() => {
    resetTimer = null;
    cancelReset();
  }, 8000);
}

function cancelReset(restoreFocus = true): void {
  clearResetTimer();
  if (resetArmed.value) {
    resetArmed.value = false;
    if (restoreFocus) void nextTick(() => resetButton.value?.focus());
  }
}

function confirmReset(): void {
  cancelReset(false);
  emit('action', 'reset');
}

function requestAction(action: WorkspaceAction): void {
  cancelReset(false);
  emit('action', action);
}

function onKeydown(event: KeyboardEvent): void {
  if (event.key === 'Escape' && resetArmed.value) cancelReset();
}

onMounted(() => {
  if (typeof window !== 'undefined') window.addEventListener('keydown', onKeydown);
});

onBeforeUnmount(() => {
  clearResetTimer();
  if (typeof window !== 'undefined') window.removeEventListener('keydown', onKeydown);
});

const now = useNow();
const summaryRef = context.summary.summary;
const summaryError = context.summary.error;
const stale = context.summary.stale;
const lastFetchedAt = context.summary.lastFetchedAt;
const connection = context.feed.connection;
const cursor = context.feed.cursor;
const lastEventAt = context.feed.lastEventAt;
const fdc3Status = context.interop.status;
const fdc3Detail = context.interop.detail;
const agentName = context.interop.agentName;
const fdc3Channel = context.interop.channel;
const fdc3ChannelMode = context.interop.channelMode;

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
const cursorLabel = computed(() => formatCount(cursor.value));
const clockLabel = computed(() => formatClock(now.value));

const fdc3Label = computed(() => {
  switch (fdc3Status.value) {
    case 'ready': {
      const channel =
        fdc3Channel.value !== null
          ? `#${fdc3Channel.value}`
          : fdc3ChannelMode.value === 'default'
            ? 'default channel'
            : 'channel unknown';
      return `FDC3: ${agentName.value ?? 'ready'} · ${channel}`;
    }
    case 'unavailable':
      return 'FDC3: no desktop agent';
    case 'error':
      return 'FDC3: listener error';
    default:
      return 'FDC3: discovering…';
  }
});

const fdc3Tone = computed(() => {
  switch (fdc3Status.value) {
    case 'ready':
      return 'ok';
    case 'unavailable':
      return 'muted';
    case 'error':
      return 'bad';
    default:
      return 'warn';
  }
});

const fdc3Title = computed(() => fdc3Detail.value ?? 'FDC3 status');

interface Kpi {
  id: string;
  label: string;
  value: string;
  tone: 'default' | 'ok' | 'warn' | 'bad';
  title: string;
}

const kpis = computed<Kpi[]>(() => {
  const s = summaryRef.value;
  const stream = s?.stream ?? null;
  const componentCount = s === null ? null : Object.keys(s.components).length;
  const activeStreams =
    stream === null
      ? null
      : stream.activeSseClients !== null || stream.maxSseClients !== null
        ? `${stream.activeSseClients ?? '—'}/${stream.maxSseClients ?? '—'}`
        : null;
  const errors = s?.errors ?? null;
  return [
    {
      id: 'lines',
      label: 'Live Log Lines',
      value: formatCount(s?.window ?? null),
      tone: 'default',
      title: 'Records measured in the host summary window',
    },
    {
      id: 'errors',
      label: 'Warn + Error',
      value: formatCount(errors),
      tone: errors !== null && errors > 0 ? 'warn' : 'default',
      title: 'Warn and error records in the summary window',
    },
    {
      id: 'traces',
      label: 'Distinct TraceID',
      value: formatCount(s?.distinctTraceIds ?? null),
      tone: 'default',
      title: 'Distinct non-empty trace ids in the summary window',
    },
    {
      id: 'components',
      label: 'Components',
      value: formatCount(componentCount),
      tone: 'default',
      title: 'Components with activity in the summary window',
    },
    {
      id: 'pushes',
      label: 'SSE Pushes',
      value: formatCount(stream?.pushTotal ?? null),
      tone: 'default',
      title: 'Measured successful stream pushes, not UI ticks',
    },
    {
      id: 'streams',
      label: 'Active Streams',
      value: activeStreams ?? '—',
      tone:
        stream !== null &&
        stream.activeSseClients !== null &&
        stream.maxSseClients !== null &&
        stream.activeSseClients >= stream.maxSseClients
          ? 'warn'
          : 'default',
      title: 'Active / maximum SSE clients',
    },
  ];
});

const healthState = computed(() => summaryRef.value?.health?.state ?? null);
const healthTone = computed(() => {
  const state = (healthState.value ?? '').toLowerCase();
  if (state === 'healthy' || state === 'ok') return 'ok';
  if (state === 'degraded' || state === 'draining' || state === 'starting') return 'warn';
  if (state === 'unhealthy' || state === 'error' || state === 'failed') return 'bad';
  return 'muted';
});
const healthUptime = computed(() => formatUptime(summaryRef.value?.health?.uptimeMs ?? null));
const summaryAge = computed(() => formatAge(lastFetchedAt.value, now.value));
</script>

<template>
  <header class="shell-header" role="banner">
    <div class="brand">
      <span class="brand-mark" aria-hidden="true"><i></i><i></i><i></i></span>
      <div class="brand-text">
        <h1>NAI LATTICE</h1>
        <span class="brand-sub">OBSERVABILITY TERMINAL</span>
      </div>
    </div>

    <div class="header-field">
      <span class="header-label">namespace</span>
      <span class="header-value">observability.log.v1</span>
    </div>

    <div class="header-field">
      <span class="header-label">clock</span>
      <time class="header-value nai-numbers" :datetime="new Date(now).toISOString()">{{ clockLabel }}</time>
    </div>

    <div class="header-field">
      <span class="header-label">health</span>
      <span class="header-value" :class="`tone--${healthTone}`">
        {{ healthState ?? 'unknown' }}
        <span class="muted">· up {{ healthUptime }}</span>
      </span>
    </div>

    <div class="header-spacer" />

    <span class="status-chip" :class="`tone--${fdc3Tone}`" :title="fdc3Title">{{ fdc3Label }}</span>

    <span class="status-chip connection" :class="`tone--${connectionTone}`">
      <span class="dot" aria-hidden="true"></span>
      <span role="status" aria-live="polite">{{ connectionLabel }}</span>
      <span class="muted" aria-hidden="true">· {{ connectionAge }}</span>
    </span>

    <span class="header-field cursor-field">
      <span class="header-label">cursor</span>
      <span class="header-value nai-numbers">{{ cursorLabel }}</span>
    </span>

    <nav class="workspace-actions" aria-label="Workspace layout actions">
      <template v-if="!resetArmed">
        <button
          ref="resetButton"
          type="button"
          title="Restore the canonical workspace layout (asks for confirmation)"
          aria-label="Reset workspace layout"
          @click="armReset"
        >
          RESET
        </button>
      </template>
      <template v-else>
        <span class="reset-confirm" role="alert" aria-live="assertive">
          Discard layout?
        </span>
        <button
          ref="confirmButton"
          type="button"
          class="reset-confirm__yes"
          title="Confirm: discard the saved layout and restore the canonical workspace"
          aria-label="Confirm reset workspace layout"
          @click="confirmReset"
        >
          CONFIRM
        </button>
        <button
          type="button"
          title="Keep the current workspace layout"
          aria-label="Cancel workspace reset"
          @click="cancelReset()"
        >
          CANCEL
        </button>
      </template>
      <button
        type="button"
        title="Float the active module inside the dashboard window"
        @click="requestAction('float')"
      >
        FLOAT
      </button>
      <button
        v-if="!embed"
        type="button"
        title="Detach the active module into a same-origin popout window (opt-in)"
        @click="requestAction('pop')"
      >
        POP
      </button>
      <button type="button" title="Maximize or restore the active group" @click="requestAction('maximize')">
        MAX
      </button>
      <button type="button" title="Duplicate the active module as a new instance" @click="requestAction('duplicate')">
        DUP
      </button>
    </nav>
  </header>

  <section class="kpi-ribbon" aria-label="Telemetry counters">
    <article
      v-for="kpi in kpis"
      :key="kpi.id"
      class="kpi"
      :class="`kpi--${kpi.tone}`"
      :title="kpi.title"
    >
      <span class="kpi-label">{{ kpi.label }}</span>
      <span class="kpi-value nai-numbers">{{ kpi.value }}</span>
    </article>
    <div class="kpi-meta">
      <span>summary {{ summaryAge }}</span>
      <span v-if="stale" class="tone--warn">stale · last measured values retained</span>
      <span v-else-if="summaryError !== null" class="tone--bad" :title="summaryError">
        summary unavailable
      </span>
      <span v-else>host measured</span>
    </div>
  </section>
</template>

<style scoped>
.shell-header {
  display: flex;
  align-items: center;
  gap: var(--nai-space-5);
  height: var(--nai-header-h);
  padding: 0 var(--nai-space-5);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-header-bg);
  flex: 0 0 auto;
  overflow: hidden;
}

.brand {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex: 0 0 auto;
}

.brand-mark {
  display: grid;
  grid-template-columns: repeat(3, 3px);
  gap: 2px;
  width: 13px;
  height: 16px;
  align-items: end;
}

.brand-mark i {
  display: block;
  background: var(--nai-accent);
  box-shadow: var(--nai-accent-glow);
}

.brand-mark i:nth-child(1) {
  height: 9px;
  opacity: 0.55;
}

.brand-mark i:nth-child(2) {
  height: 16px;
}

.brand-mark i:nth-child(3) {
  height: 12px;
  opacity: 0.8;
}

.brand-text {
  display: flex;
  flex-direction: column;
  line-height: 1.1;
}

.brand h1 {
  font-size: 13px;
  letter-spacing: 1.6px;
  color: var(--nai-accent);
  text-shadow: var(--nai-accent-glow);
  font-weight: 700;
}

.brand-sub {
  font-size: 9px;
  letter-spacing: 1.2px;
  color: var(--nai-dim);
}

.header-field {
  display: flex;
  align-items: baseline;
  gap: var(--nai-space-3);
  min-width: 0;
  flex: 0 1 auto;
}

.header-label {
  color: var(--nai-dim);
  font-size: var(--nai-font-size-xs);
  text-transform: uppercase;
  letter-spacing: 0.6px;
}

.header-value {
  color: var(--nai-fg);
  font-size: var(--nai-font-size-sm);
  white-space: nowrap;
}

.header-spacer {
  flex: 1 1 auto;
}

.status-chip {
  display: inline-flex;
  align-items: center;
  gap: var(--nai-space-3);
  height: 20px;
  padding: 0 var(--nai-space-4);
  border: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel-2);
  font-size: var(--nai-font-size-xs);
  text-transform: uppercase;
  letter-spacing: 0.5px;
  white-space: nowrap;
  flex: 0 0 auto;
}

.connection .dot {
  width: 6px;
  height: 6px;
  background: currentColor;
  box-shadow: 0 0 8px currentColor;
}

.cursor-field {
  flex: 0 0 auto;
}

.workspace-actions {
  display: flex;
  gap: var(--nai-space-1);
  flex: 0 0 auto;
}

.workspace-actions button {
  height: 20px;
  padding: 0 var(--nai-space-4);
  background: transparent;
  border: var(--nai-hairline) solid var(--nai-line);
  color: var(--nai-muted);
  font-size: 9px;
  letter-spacing: 0.7px;
  text-transform: uppercase;
}

.workspace-actions button:hover {
  color: var(--nai-accent);
  border-color: var(--nai-accent);
  background: var(--nai-hover);
}

.reset-confirm {
  display: inline-flex;
  align-items: center;
  color: var(--nai-warn);
  font-size: 9px;
  letter-spacing: 0.5px;
  text-transform: uppercase;
  white-space: nowrap;
}

.workspace-actions .reset-confirm__yes {
  border-color: var(--nai-warn);
  color: var(--nai-warn);
}

.workspace-actions .reset-confirm__yes:hover {
  border-color: var(--nai-bad);
  color: var(--nai-bad);
  background: rgba(255, 107, 107, 0.08);
}

.kpi-ribbon {
  display: grid;
  grid-template-columns: repeat(6, minmax(0, 1fr));
  gap: var(--nai-space-4);
  padding: var(--nai-space-4) var(--nai-space-5) var(--nai-space-3);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-bg);
  flex: 0 0 auto;
}

.kpi {
  position: relative;
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
  padding: var(--nai-space-3) var(--nai-space-4) var(--nai-space-4);
  border: var(--nai-hairline) solid var(--nai-kpi-border);
  border-radius: 0;
  background: linear-gradient(125deg, var(--nai-panel) 0%, var(--nai-kpi-bg-end) 100%);
  box-shadow: inset 0 1px rgba(255, 255, 255, 0.05);
}

.kpi::after {
  content: "";
  position: absolute;
  inset: 0 0 auto;
  height: 1px;
  background: linear-gradient(90deg, transparent, rgba(25, 194, 216, 0.55), transparent);
}

.kpi-label {
  color: var(--nai-muted);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.7px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.kpi-value {
  font-size: 18px;
  font-weight: 700;
  color: var(--nai-fg);
  letter-spacing: 0.2px;
}

.kpi--warn .kpi-value {
  color: var(--nai-warn);
}

.kpi--bad .kpi-value {
  color: var(--nai-bad);
}

.kpi--ok .kpi-value {
  color: var(--nai-ok);
}

.kpi-meta {
  grid-column: 1 / -1;
  display: flex;
  gap: var(--nai-space-5);
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
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

.muted {
  color: var(--nai-dim);
}

@media (max-width: 1180px) {
  .header-field:nth-of-type(3) {
    display: none;
  }

  .brand-sub {
    display: none;
  }
}

@media (max-width: 900px) {
  .kpi-ribbon {
    grid-template-columns: repeat(3, minmax(0, 1fr));
  }

  .header-field {
    display: none;
  }
}
</style>
