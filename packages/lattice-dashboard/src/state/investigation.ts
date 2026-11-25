import { shallowRef, type ShallowRef } from 'vue';
import type { ModuleType } from '../workspace/schema';

export interface PanelRequest {
  panel: ModuleType;
  nonce: number;
}

/**
 * Cross-panel investigation state and dock focus requests.
 *
 * Filters are explicit and removable. Panels never broadcast on receive, so
 * there is no context ping-pong.
 */
export class InvestigationStore {
  readonly trace: ShallowRef<string> = shallowRef('');
  readonly component: ShallowRef<string> = shallowRef('');
  readonly sinceMs: ShallowRef<number | null> = shallowRef(null);
  readonly panelRequest: ShallowRef<PanelRequest | null> = shallowRef(null);

  private nonce = 0;

  setTrace(traceId: string | null | undefined): void {
    this.trace.value = typeof traceId === 'string' ? traceId.trim() : '';
  }

  setComponent(component: string | null | undefined): void {
    this.component.value = typeof component === 'string' ? component.trim() : '';
  }

  setSince(timestampMs: number | null | undefined): void {
    this.sinceMs.value =
      typeof timestampMs === 'number' && Number.isFinite(timestampMs) && timestampMs > 0
        ? Math.trunc(timestampMs)
        : null;
  }

  requestPanel(panel: ModuleType): void {
    this.nonce += 1;
    this.panelRequest.value = { panel, nonce: this.nonce };
  }
}
