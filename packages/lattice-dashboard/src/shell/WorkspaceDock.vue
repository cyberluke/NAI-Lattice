<script setup lang="ts">
import { computed, inject, markRaw, onBeforeUnmount, shallowRef, watch } from 'vue';
import { DockviewVue, type DockviewApi, type DockviewReadyEvent, type VueComponent } from 'dockview-vue';
import { APP_CONTEXT_KEY } from '../app-context';
import { nextInstanceId, seedInstanceId, isModuleParams, MODULE_TITLES, type ModuleParams, type ModuleType } from '../workspace/schema';
import { WorkspaceStore } from '../workspace/store';
import type { WorkspaceAction } from './workspace-actions';
import { popoutActivePanel, type PopoutCapableApi, type PopoutOutcome } from './popout';
import PanelHost from './PanelHost.vue';

interface Disposable {
  dispose(): void;
}

const context = inject(APP_CONTEXT_KEY);
if (context === undefined) throw new Error('workspace context is missing');
const appContext: NonNullable<typeof context> = context;

const api = shallowRef<DockviewApi | null>(null);
const maximized = shallowRef(false);
const notice = shallowRef<string | null>(null);
const noticeTone = shallowRef<'warn' | 'info'>('warn');
const store = new WorkspaceStore();
const disposables: Disposable[] = [];
let hydrating = true;
let noticeTimer: ReturnType<typeof setTimeout> | null = null;

const components = markRaw({
  module: PanelHost,
}) as unknown as Record<string, VueComponent>;

function showNotice(message: string, tone: 'warn' | 'info' = 'warn', durationMs = 12_000): void {
  notice.value = message;
  noticeTone.value = tone;
  if (noticeTimer !== null) clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => {
    noticeTimer = null;
    notice.value = null;
  }, durationMs);
}

function dismissNotice(): void {
  if (noticeTimer !== null) {
    clearTimeout(noticeTimer);
    noticeTimer = null;
  }
  notice.value = null;
}

function onReady(event: DockviewReadyEvent): void {
  const instance = markRaw(event.api);
  api.value = instance;
  const loaded = store.load();
  if (loaded.notice !== null) showNotice(loaded.notice, 'warn', 14_000);

  try {
    if (loaded.layout !== null) instance.fromJSON(loaded.layout);
    if (instance.panels.length === 0) {
      seed(instance);
      // A reset/migration is persisted immediately so the canonical layout is
      // restored silently on the next reload instead of re-triggering the notice.
      if (loaded.reason !== 'missing') store.save(instance.toJSON());
    }
  } catch {
    resetWorkspace(instance);
    showNotice(
      'The saved workspace could not be restored by Dockview; the canonical layout was restored.',
      'warn',
      14_000,
    );
  }

  hydrating = false;
  disposables.push(
    instance.onDidLayoutChange(() => {
      if (hydrating) return;
      store.scheduleSave(instance.toJSON());
    }),
  );
  disposables.push(
    instance.onDidMaximizedGroupChange((change) => {
      maximized.value = change.isMaximized;
    }),
  );

  const request = appContext.investigation.panelRequest.value;
  if (request !== null) activateModule(request.panel);
}

/**
 * Drops the persisted layout, rebuilds the canonical one and persists it, so a
 * layout Dockview cannot restore never survives to the next reload.
 */
function resetWorkspace(instance: DockviewApi): void {
  hydrating = true;
  try {
    store.clear();
    instance.clear();
    seed(instance);
    store.save(instance.toJSON());
  } finally {
    hydrating = false;
  }
}

function seed(instance: DockviewApi): void {
  const logId = seedInstanceId('log-tail');
  const overviewId = seedInstanceId('overview');
  const tracesId = seedInstanceId('traces');
  const errorsId = seedInstanceId('errors');
  const healthId = seedInstanceId('health');
  const componentsId = seedInstanceId('components');
  const storageId = seedInstanceId('storage');
  const hostId = seedInstanceId('host');
  const topologyId = seedInstanceId('topology');
  const executionChainId = seedInstanceId('execution-chain');

  addModule(instance, 'log-tail', {
    id: logId,
    initialWidth: 960,
    initialHeight: 620,
  });
  addModule(instance, 'overview', {
    id: overviewId,
    position: { referencePanel: logId, direction: 'within' },
  });
  addModule(instance, 'traces', {
    id: tracesId,
    initialHeight: 268,
    position: { referencePanel: logId, direction: 'below' },
  });
  addModule(instance, 'errors', {
    id: errorsId,
    position: { referencePanel: tracesId, direction: 'within' },
  });
  addModule(instance, 'topology', {
    id: topologyId,
    position: { referencePanel: tracesId, direction: 'within' },
  });
  // Fourth tab in the investigation group. Existing v4 layouts stay valid:
  // the module is added to fresh/reset layouts without a schema bump.
  addModule(instance, 'execution-chain', {
    id: executionChainId,
    position: { referencePanel: tracesId, direction: 'within' },
  });
  addModule(instance, 'health', {
    id: healthId,
    initialWidth: 600,
    position: { referencePanel: logId, direction: 'right' },
  });
  // One right-hand group (Health / Storage / Components / Host & GPU) so the
  // Storage class table gets the full column height instead of a 200 px sliver.
  addModule(instance, 'storage', {
    id: storageId,
    position: { referencePanel: healthId, direction: 'within' },
  });
  addModule(instance, 'components', {
    id: componentsId,
    position: { referencePanel: healthId, direction: 'within' },
  });
  addModule(instance, 'host', {
    id: hostId,
    position: { referencePanel: healthId, direction: 'within' },
  });

  // Each group keeps its own active tab: show Health (not the last tab added)
  // on the right, Traces in the bottom group, and the log tail on the left.
  const healthPanel = instance.getPanel(healthId);
  if (healthPanel !== undefined) healthPanel.api.setActive();

  const tracesPanel = instance.getPanel(tracesId);
  if (tracesPanel !== undefined) tracesPanel.api.setActive();

  const logPanel = instance.getPanel(logId);
  if (logPanel !== undefined) logPanel.api.setActive();
}

function addModule(
  instance: DockviewApi,
  type: ModuleType,
  placement: {
    id: string;
    initialWidth?: number;
    initialHeight?: number;
    position?: { referencePanel: string; direction: 'left' | 'right' | 'above' | 'below' | 'within' };
  },
): void {
  const params: ModuleParams = { moduleType: type, instanceId: placement.id };
  instance.addPanel({
    id: placement.id,
    component: 'module',
    title: MODULE_TITLES[type],
    params,
    initialWidth: placement.initialWidth,
    initialHeight: placement.initialHeight,
    ...(placement.position !== undefined ? { position: placement.position } : {}),
  });
}

function findPanel(type: ModuleType) {
  const instance = api.value;
  if (instance === null) return undefined;
  return instance.panels.find(
    (panel) =>
      panel.view.contentComponent === 'module' &&
      isModuleParams(panel.params) &&
      panel.params.moduleType === type,
  );
}

function activateModule(type: ModuleType): void {
  const instance = api.value;
  if (instance === null) return;
  try {
    const existing = findPanel(type);
    if (existing !== undefined) {
      existing.api.setActive();
      return;
    }
    const ids = instance.panels.map((panel) => panel.id);
    const instanceId = nextInstanceId(type, ids);
    const first = instance.panels[0];
    addModule(instance, type, {
      id: instanceId,
      initialWidth: type === 'log-tail' ? 720 : 460,
      initialHeight: type === 'traces' ? 280 : 420,
      ...(first !== undefined
        ? { position: { referencePanel: first.id, direction: 'right' as const } }
        : {}),
    });
  } catch (error) {
    showNotice(
      `Could not open module: ${error instanceof Error ? error.message : 'unknown error'}`,
      'warn',
      8000,
    );
  }
}

function popoutNotice(outcome: PopoutOutcome): void {
  const label = outcome.status === 'no-active-panel' ? 'module' : (outcome.title ?? outcome.panelId);
  switch (outcome.status) {
    case 'detached':
      showNotice(
        `Detached “${label}” into a popout window. The layout keeps the panel; closing the window docks it back.`,
        'info',
        7000,
      );
      break;
    case 'blocked':
      showNotice(
        `Popout was blocked by the window manager; “${label}” stays docked in the dashboard.`,
        'warn',
        9000,
      );
      break;
    case 'error':
      showNotice(`Popout failed: ${outcome.message}; the panel stays docked.`, 'warn', 9000);
      break;
    case 'no-active-panel':
      showNotice('No active module to pop out.', 'warn', 6000);
      break;
  }
}

async function popoutActive(instance: DockviewApi): Promise<void> {
  // `DockviewApi.addPopoutGroup` resolves false when the popout module is
  // unavailable and rejects on a refused/scriptable-blocked window; the helper
  // normalises both into an outcome so the panel always stays recoverable.
  const outcome = await popoutActivePanel(instance as unknown as PopoutCapableApi);
  popoutNotice(outcome);
}

function perform(action: WorkspaceAction): void {
  const instance = api.value;
  if (instance === null) return;
  try {
    switch (action) {
    case 'reset': {
      resetWorkspace(instance);
      showNotice('Canonical workspace restored. Your previous layout was discarded.', 'info', 8000);
      break;
    }
    case 'float': {
      const active = instance.activePanel;
      if (active === undefined) return;
      instance.addFloatingGroup(active, { x: 64, y: 52, width: 680, height: 420 });
      showNotice('Module floated inside the dashboard window.', 'info', 5000);
      break;
    }
    case 'pop': {
      void popoutActive(instance);
      break;
    }
    case 'maximize': {
      if (instance.hasMaximizedGroup()) {
        instance.exitMaximizedGroup();
      } else {
        const active = instance.activePanel;
        if (active !== undefined) instance.maximizeGroup(active);
      }
      break;
    }
    case 'duplicate': {
      const active = instance.activePanel;
      if (active === undefined || !isModuleParams(active.params)) return;
      const ids = instance.panels.map((panel) => panel.id);
      const instanceId = nextInstanceId(active.params.moduleType, ids);
      const suffix = instanceId.slice(instanceId.indexOf('@') + 1);
      instance.addPanel({
        id: instanceId,
        component: 'module',
        title: `${MODULE_TITLES[active.params.moduleType]} · ${suffix}`,
        params: { moduleType: active.params.moduleType, instanceId },
        position: { referencePanel: active.id, direction: 'within' },
      });
      break;
    }
    default:
      break;
    }
  } catch (error) {
    hydrating = false;
    showNotice(
      `Workspace action failed: ${error instanceof Error ? error.message : 'unknown error'}`,
      'warn',
      8000,
    );
  }
}

const stopRequestWatch = watch(
  () => appContext.investigation.panelRequest.value,
  (request) => {
    if (request !== null) activateModule(request.panel);
  },
);

const maximizedLabel = computed(() => (maximized.value ? 'RESTORE' : 'MAX'));

onBeforeUnmount(() => {
  stopRequestWatch();
  store.flush();
  for (const disposable of disposables.splice(0)) disposable.dispose();
  if (noticeTimer !== null) {
    clearTimeout(noticeTimer);
    noticeTimer = null;
  }
});

defineExpose({ perform, activateModule, maximizedLabel });
</script>

<template>
  <div class="workspace-root">
    <!--
      `style` is deliberately inline: DockviewVue renders its own root element
      and does not inherit the parent's scoped-CSS data attribute, so the scoped
      `.dockview-host { height: 100% }` rule never matched and the dock collapsed
      to 0 px (header visible, workspace blank). Attribute fallthrough keeps the
      host sized regardless of scoping.
    -->
    <DockviewVue
      class="dockview-host dockview-theme-nai dockview-theme-dark"
      style="width: 100%; height: 100%; min-height: 0"
      :components="components"
      :floating-group-bounds="'boundedWithinViewport'"
      :floating-group-drag-handle="'titlebar'"
      :proportional-layout="true"
      :single-tab-mode="'default'"
      :disable-floating-groups="false"
      @ready="onReady"
    />
    <div v-if="notice !== null" class="workspace-notice" :class="`workspace-notice--${noticeTone}`" role="status">
      <span>{{ notice }}</span>
      <button type="button" aria-label="Dismiss notice" @click="dismissNotice">DISMISS</button>
    </div>
  </div>
</template>

<style scoped>
.workspace-root {
  position: relative;
  width: 100%;
  height: 100%;
  min-height: 0;
  background: var(--nai-bg);
}

.dockview-host {
  width: 100%;
  height: 100%;
}

.workspace-notice {
  position: absolute;
  right: var(--nai-space-5);
  bottom: var(--nai-space-5);
  z-index: var(--dv-overlay-z-index, 30);
  display: flex;
  align-items: center;
  gap: var(--nai-space-5);
  max-width: min(620px, 80vw);
  padding: var(--nai-space-4) var(--nai-space-5);
  border: var(--nai-hairline) solid var(--nai-line-strong);
  background: var(--nai-panel);
  box-shadow: var(--nai-shadow);
  font-size: var(--nai-font-size-sm);
}

.workspace-notice--warn {
  border-left: 2px solid var(--nai-warn);
}

.workspace-notice--info {
  border-left: 2px solid var(--nai-accent);
}

.workspace-notice button {
  flex: 0 0 auto;
  height: 20px;
  padding: 0 var(--nai-space-4);
  border: var(--nai-hairline) solid var(--nai-line);
  background: transparent;
  color: var(--nai-muted);
  font-size: 9px;
  letter-spacing: 0.6px;
}

.workspace-notice button:hover {
  color: var(--nai-accent);
  border-color: var(--nai-accent);
}
</style>
