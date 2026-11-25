import { markRaw, type Component } from 'vue';
import type { ModuleType } from '../workspace/schema';
import LogTail from '../modules/log-tail/LogTail.vue';
import ComponentsPanel from '../modules/components/ComponentsPanel.vue';
import TracesPanel from '../modules/traces/TracesPanel.vue';
import HealthPanel from '../modules/health/HealthPanel.vue';
import StoragePanel from '../modules/storage/StoragePanel.vue';
import OverviewPanel from '../modules/overview/OverviewPanel.vue';
import ErrorIntelligence from '../modules/errors/ErrorIntelligence.vue';
import DiscoverPanel from '../modules/discover/DiscoverPanel.vue';
import HostPanel from '../modules/host/HostPanel.vue';
import TopologyPanel from '../modules/topology/TopologyPanel.vue';
import ExecutionChain from '../modules/traces/ExecutionChain.vue';

/** Stable module type id -> implementation. Instance identity lives in params. */
export const MODULE_COMPONENTS: Record<ModuleType, Component> = markRaw({
  'log-tail': markRaw(LogTail),
  components: markRaw(ComponentsPanel),
  traces: markRaw(TracesPanel),
  health: markRaw(HealthPanel),
  storage: markRaw(StoragePanel),
  overview: markRaw(OverviewPanel),
  errors: markRaw(ErrorIntelligence),
  discover: markRaw(DiscoverPanel),
  host: markRaw(HostPanel),
  topology: markRaw(TopologyPanel),
  'execution-chain': markRaw(ExecutionChain),
});
