import type { LatticeAppContext } from '../app-context';

/**
 * Same-origin module SDK v1 (`window.naiLattice`, frozen contract §10).
 *
 * A module never receives the internal stores and never fabricates a Desktop
 * Agent. It can focus a real trace and raise a selection context; both actions
 * go through the same investigation store / FDC3 adapter as the shell UI.
 */
export const MODULE_SDK_VERSION = 1;

export interface NaiLatticeSdkV1 {
  readonly version: 1;
  /**
   * `nai.lattice` once the app channel is joined. `null` means the Desktop
   * Agent lacked channel support (default-channel fallback) or no agent is
   * connected; the SDK never pretends a channel exists.
   */
  readonly channel: string | null;
  /** Focus an existing trace in the shell and raise it on the app channel. */
  focusTrace(id: string): void;
  /** Validate and raise a `nai.lattice.logSelection` v1 context. */
  raiseSelection(context: unknown): Promise<boolean>;
}

export interface ModuleSdkHost {
  investigation: LatticeAppContext['investigation'];
  interop: LatticeAppContext['interop'];
}

export function createModuleSdk(host: ModuleSdkHost): NaiLatticeSdkV1 {
  return {
    version: MODULE_SDK_VERSION,
    get channel(): string | null {
      return host.interop.channel.value;
    },
    focusTrace(id: string): void {
      const traceId = typeof id === 'string' ? id.trim() : '';
      if (traceId.length === 0) return;
      host.investigation.setTrace(traceId);
      host.investigation.requestPanel('traces');
      // Fire-and-forget: a missing agent is a normal state, never an exception
      // thrown into a same-origin module.
      void host.interop.raiseTrace(traceId);
    },
    raiseSelection(context: unknown): Promise<boolean> {
      return host.interop.raiseSelection(context);
    },
  };
}

function resolveWindow(target?: Window): Window | null {
  if (target !== undefined) return target;
  return typeof window === 'undefined' ? null : window;
}

/** Installs `window.naiLattice` v1 for same-origin modules. */
export function installModuleSdk(host: ModuleSdkHost, target?: Window): NaiLatticeSdkV1 {
  const win = resolveWindow(target);
  if (win === null) throw new Error('module SDK requires a browser window');
  const sdk = createModuleSdk(host);
  win.naiLattice = sdk;
  return sdk;
}

/**
 * Removes the SDK only when it still points at the instance this shell
 * installed, so a host with its own SDK is never clobbered.
 */
export function uninstallModuleSdk(target?: Window, sdk?: NaiLatticeSdkV1): void {
  const win = resolveWindow(target);
  if (win === null) return;
  if (sdk !== undefined && win.naiLattice !== sdk) return;
  delete win.naiLattice;
}
