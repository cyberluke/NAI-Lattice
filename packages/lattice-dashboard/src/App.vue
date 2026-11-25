<script setup lang="ts">
import { onBeforeUnmount, onMounted, provide, ref } from 'vue';
import { APP_CONTEXT_KEY, type LatticeAppContext } from './app-context';
import { Fdc3Adapter } from './interop/adapter';
import { QueryClient } from './query/client';
import { TracesClient } from './query/traces';
import { parseDeepLinks } from './shell/deeplinks';
import ShellChrome from './shell/ShellChrome.vue';
import WorkspaceDock from './shell/WorkspaceDock.vue';
import type { WorkspaceAction } from './shell/workspace-actions';
import { installModuleSdk, uninstallModuleSdk, type NaiLatticeSdkV1 } from './sdk/module-sdk';
import { InvestigationStore } from './state/investigation';
import { TelemetryFeed } from './telemetry/feed';
import { SummaryStore } from './telemetry/summary';

const props = defineProps<{ context?: LatticeAppContext }>();

function createContext(): LatticeAppContext {
  const query = new QueryClient();
  return {
    feed: new TelemetryFeed({ queryClient: query }),
    summary: new SummaryStore(),
    query,
    traces: new TracesClient(),
    interop: new Fdc3Adapter(),
    investigation: new InvestigationStore(),
  };
}

const deepLinks =
  typeof window === 'undefined' ? parseDeepLinks('') : parseDeepLinks(window.location.search);
const ownsContext = props.context === undefined;
const context = props.context ?? createContext();
const embed = deepLinks.embed;
provide(APP_CONTEXT_KEY, context);

const dock = ref<InstanceType<typeof WorkspaceDock> | null>(null);
let unsubscribeFocus: (() => void) | null = null;
let moduleSdk: NaiLatticeSdkV1 | null = null;

function applyDeepLinks(): void {
  if (deepLinks.trace !== null) context.investigation.setTrace(deepLinks.trace);
  if (deepLinks.component !== null) context.investigation.setComponent(deepLinks.component);
  if (deepLinks.timeMs !== null) context.investigation.setSince(deepLinks.timeMs);
  const hasLink =
    deepLinks.panel !== null ||
    deepLinks.trace !== null ||
    deepLinks.component !== null ||
    deepLinks.timeMs !== null;
  // A plain reload must restore the saved layout untouched; only an explicit
  // deep link is allowed to focus or re-create a module.
  if (!hasLink) return;
  const target = deepLinks.panel ?? (deepLinks.trace !== null ? 'traces' : 'log-tail');
  context.investigation.requestPanel(target);
  // Dockview can still be restoring the saved layout when the deep link lands;
  // re-assert a trace focus once the workspace has settled so the selected
  // trace's CHAIN is visible instead of leaving an unrelated tab active.
  if (deepLinks.trace !== null && deepLinks.panel === null && typeof window !== 'undefined') {
    window.setTimeout(() => context.investigation.requestPanel('traces'), 150);
  }
}

function onWorkspaceAction(action: WorkspaceAction): void {
  dock.value?.perform(action);
}

onMounted(() => {
  moduleSdk = installModuleSdk(context);
  applyDeepLinks();
  unsubscribeFocus = context.interop.onFocusTrace((traceId) => {
    context.investigation.setTrace(traceId);
    context.investigation.requestPanel('traces');
  });
  context.feed.start();
  context.summary.start();
  void context.interop.connect();
});

onBeforeUnmount(() => {
  if (moduleSdk !== null) {
    uninstallModuleSdk(undefined, moduleSdk);
    moduleSdk = null;
  }
  if (unsubscribeFocus !== null) {
    unsubscribeFocus();
    unsubscribeFocus = null;
  }
  if (ownsContext) {
    context.feed.dispose();
    context.summary.dispose();
    context.interop.dispose();
  }
});
</script>

<template>
  <div class="lattice-app" :class="{ 'lattice-app--embed': embed }">
    <ShellChrome v-if="!embed" @action="onWorkspaceAction" />
    <main class="lattice-workspace" aria-label="NAI Lattice workspace">
      <WorkspaceDock ref="dock" />
    </main>
  </div>
</template>

<style scoped>
.lattice-app {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  background: var(--nai-bg);
  color: var(--nai-fg);
  overflow: hidden;
}

.lattice-workspace {
  position: relative;
  flex: 1 1 auto;
  min-height: 0;
  overflow: hidden;
}

.lattice-app--embed {
  height: 100vh;
}

.lattice-app--embed .lattice-workspace {
  height: 100vh;
}
</style>
