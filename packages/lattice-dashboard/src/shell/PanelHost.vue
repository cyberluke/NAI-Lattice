<script setup lang="ts">
import { computed, onErrorCaptured, ref } from 'vue';
import type { IDockviewPanelProps } from 'dockview-vue';
import { isModuleParams, type ModuleParams } from '../workspace/schema';
import { MODULE_COMPONENTS } from './registry';

/**
 * dockview-vue mounts a panel component with a single `params` prop whose value
 * is the full panel context (`{ params, api, containerApi }`). The stable module
 * type and unique instance id live in the nested `params` object.
 */
const props = defineProps<{ params?: IDockviewPanelProps<ModuleParams> }>();

const moduleParams = computed(() =>
  props.params !== undefined && isModuleParams(props.params.params) ? props.params.params : null,
);
const moduleComponent = computed(() =>
  moduleParams.value === null ? null : MODULE_COMPONENTS[moduleParams.value.moduleType],
);

const failure = ref<string | null>(null);
const generation = ref(0);

onErrorCaptured((error) => {
  failure.value = error instanceof Error ? error.message : 'module render failed';
  return false;
});

function retry(): void {
  failure.value = null;
  generation.value += 1;
}
</script>

<template>
  <div v-if="failure !== null" class="module-unavailable" role="alert">
    <strong>Module error</strong>
    <span>{{ failure }}</span>
    <button type="button" @click="retry">RETRY MODULE</button>
  </div>
  <component
    :is="moduleComponent"
    v-else-if="moduleComponent !== null"
    :key="generation"
    :instance-id="moduleParams?.instanceId"
    class="module-root"
  />
  <div v-else class="module-unavailable" role="alert">
    <strong>Unavailable module</strong>
    <span>The persisted workspace references a module type this build does not provide.</span>
  </div>
</template>

<style scoped>
.module-root {
  display: block;
  width: 100%;
  height: 100%;
  min-height: 0;
}

.module-unavailable {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-3);
  height: 100%;
  padding: var(--nai-space-5);
  color: var(--nai-bad);
  font-size: var(--nai-font-size-sm);
  background: var(--nai-panel-2);
}

.module-unavailable span {
  color: var(--nai-muted);
}

.module-unavailable button {
  align-self: flex-start;
  height: 22px;
  padding: 0 var(--nai-space-4);
  border: var(--nai-hairline) solid var(--nai-bad);
  background: transparent;
  color: var(--nai-bad);
  font-size: 9px;
  letter-spacing: 0.6px;
  text-transform: uppercase;
}
</style>
