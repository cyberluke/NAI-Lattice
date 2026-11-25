<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import type { LogRecord } from '../../telemetry/types';
import { displayLevel, formatTimestamp } from '../../ui/format';

const props = defineProps<{ record: LogRecord | null }>();
const emit = defineEmits<{ close: []; 'focus-trace': [traceId: string] }>();

const drawer = ref<HTMLElement | null>(null);
let escapeHandler: ((event: KeyboardEvent) => void) | null = null;

const traceId = computed(() => {
  const value = props.record?.trace_id ?? null;
  return value !== null && value.length > 0 ? value : null;
});

const timestamp = computed(() => {
  const value = props.record?.timestamp_utc_ms ?? 0;
  return {
    local: formatTimestamp(value),
    iso: value > 0 ? new Date(value).toISOString() : '—',
    raw: value > 0 ? String(value) : '—',
  };
});

function pretty(value: unknown): string {
  if (value === undefined) return '—';
  if (value === null) return 'null';
  if (typeof value === 'string') return value;
  try {
    return JSON.stringify(value, null, 2) ?? String(value);
  } catch {
    return String(value);
  }
}

function focusTrace(): void {
  const id = traceId.value;
  if (id !== null) emit('focus-trace', id);
}

watch(
  () => props.record,
  async (record) => {
    if (record === null) return;
    await nextTick();
    drawer.value?.focus();
  },
  { immediate: true },
);

onMounted(() => {
  escapeHandler = (event: KeyboardEvent) => {
    if (props.record === null || event.key !== 'Escape') return;
    event.preventDefault();
    emit('close');
  };
  window.addEventListener('keydown', escapeHandler);
});

onBeforeUnmount(() => {
  if (escapeHandler !== null) {
    window.removeEventListener('keydown', escapeHandler);
    escapeHandler = null;
  }
});
</script>

<template>
  <aside
    v-if="record !== null"
    ref="drawer"
    class="record-inspector"
    role="dialog"
    aria-modal="false"
    aria-label="Record inspector"
    tabindex="-1"
    @keydown.esc.stop.prevent="emit('close')"
  >
    <header class="inspector-head">
      <div class="inspector-title">
        <span class="eyebrow">record inspector</span>
        <span class="seq nai-numbers">seq {{ record.runtime_sequence }}</span>
      </div>
      <button type="button" class="button" @click="emit('close')">CLOSE</button>
    </header>

    <div class="inspector-body">
      <dl class="field-list">
        <div class="field">
          <dt>runtime_sequence</dt>
          <dd class="nai-numbers">{{ record.runtime_sequence }}</dd>
        </div>
        <div class="field">
          <dt>timestamp</dt>
          <dd>
            <span class="nai-numbers">{{ timestamp.local }}</span>
            <span class="muted nai-numbers"> · {{ timestamp.iso }}</span>
            <span class="muted nai-numbers"> · {{ timestamp.raw }}</span>
          </dd>
        </div>
        <div class="field">
          <dt>level</dt>
          <dd>{{ displayLevel(record.level) }}</dd>
        </div>
        <div class="field">
          <dt>component</dt>
          <dd>{{ record.component }}</dd>
        </div>
        <div class="field">
          <dt>trace_id</dt>
          <dd class="break">{{ traceId ?? '—' }}</dd>
        </div>
        <div class="field">
          <dt>caller</dt>
          <dd class="break">{{ record.caller ?? '—' }}</dd>
        </div>
      </dl>

      <section class="block">
        <h2>message</h2>
        <pre class="raw raw--message">{{ record.message }}</pre>
      </section>

      <section class="block">
        <h2>extras</h2>
        <pre class="raw">{{ pretty(record.extras) }}</pre>
      </section>

      <section v-if="record.payload !== undefined" class="block">
        <h2>payload</h2>
        <pre class="raw">{{ pretty(record.payload) }}</pre>
      </section>
    </div>

    <footer class="inspector-foot">
      <button
        type="button"
        class="button button--accent"
        :disabled="traceId === null"
        :title="traceId === null ? 'This record has no trace id' : 'Focus the traces panel on this trace'"
        @click="focusTrace"
      >
        FOCUS TRACE
      </button>
      <span class="muted">ESC closes · raw fields are shown verbatim</span>
    </footer>
  </aside>
</template>

<style scoped>
.record-inspector {
  position: absolute;
  top: 0;
  right: 0;
  bottom: 0;
  z-index: 12;
  display: flex;
  flex-direction: column;
  width: min(440px, 92%);
  border-left: var(--nai-hairline) solid var(--nai-line-strong);
  background: var(--nai-panel);
  box-shadow: var(--nai-shadow);
  outline: none;
}

.inspector-head {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex: 0 0 auto;
  min-height: var(--nai-toolbar-h);
  padding: 0 var(--nai-space-4);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel-3);
}

.inspector-title {
  display: flex;
  align-items: baseline;
  gap: var(--nai-space-4);
  min-width: 0;
}

.eyebrow {
  color: var(--nai-accent);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.8px;
}

.seq {
  color: var(--nai-dim);
  font-size: 9px;
}

.inspector-body {
  flex: 1 1 auto;
  min-height: 0;
  overflow: auto;
  scrollbar-gutter: stable;
  padding: var(--nai-space-4);
}

.field-list {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-2);
  margin: 0 0 var(--nai-space-4);
}

.field {
  display: grid;
  grid-template-columns: 118px minmax(0, 1fr);
  gap: var(--nai-space-3);
  align-items: baseline;
}

.field dt {
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.4px;
}

.field dd {
  margin: 0;
  min-width: 0;
  color: var(--nai-fg);
  font-size: var(--nai-font-size-sm);
}

.break {
  overflow-wrap: anywhere;
}

.block {
  margin-top: var(--nai-space-4);
}

.block h2 {
  margin-bottom: var(--nai-space-2);
  color: var(--nai-accent);
  font-size: 9px;
  font-weight: 400;
  text-transform: uppercase;
  letter-spacing: 0.8px;
}

.raw {
  max-height: 260px;
  margin: 0;
  padding: var(--nai-space-3);
  border: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel-2);
  color: var(--nai-fg-soft);
  font-family: var(--nai-font-ui);
  font-size: 10px;
  line-height: 1.5;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  overflow: auto;
}

.raw--message {
  color: var(--nai-fg);
  font-size: var(--nai-font-size-sm);
}

.inspector-foot {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex: 0 0 auto;
  padding: var(--nai-space-3) var(--nai-space-4);
  border-top: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
}

.button {
  height: 22px;
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

.button:hover:not(:disabled) {
  color: var(--nai-accent);
  border-color: var(--nai-accent);
  background: var(--nai-hover);
}

.button:disabled {
  opacity: 0.4;
  cursor: not-allowed;
}

.button--accent {
  color: var(--nai-accent);
  border-color: var(--nai-accent);
}

.muted {
  color: var(--nai-dim);
  font-size: 9px;
}
</style>
