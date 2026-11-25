/**
 * Popout (U04) is opt-in and must never lose a panel. Dockview's
 * `addPopoutGroup` resolves `false` when the window module is unavailable and
 * rejects on a blocked/refused window; both outcomes leave the group docked.
 * Keeping the decision here makes the success/failure semantics testable with
 * a mocked API (no real window is opened in jsdom).
 */

/** Same-origin page shipped in the bundle (`public/popout.html`). */
export const POPOUT_URL = '/lattice/popout.html';

export interface PopoutPanel {
  readonly id: string;
  readonly title?: string;
}

/** The narrow Dockview surface the popout action needs. */
export interface PopoutCapableApi {
  readonly activePanel?: PopoutPanel | undefined;
  addPopoutGroup(panel: PopoutPanel, options?: { popoutUrl?: string }): Promise<boolean>;
}

export type PopoutOutcome =
  | { status: 'detached'; panelId: string; title: string | null }
  | { status: 'no-active-panel' }
  | { status: 'blocked'; panelId: string; title: string | null }
  | { status: 'error'; panelId: string | null; title: string | null; message: string };

function titleOf(panel: PopoutPanel): string | null {
  return typeof panel.title === 'string' && panel.title.length > 0 ? panel.title : null;
}

/**
 * Detaches the active panel into a same-origin popout window. Never throws:
 * every failure is reported as an outcome so the caller can show a notice and
 * leave the panel in place.
 */
export async function popoutActivePanel(api: PopoutCapableApi): Promise<PopoutOutcome> {
  const panel = api.activePanel;
  if (panel === undefined || panel.id.length === 0) {
    return { status: 'no-active-panel' };
  }
  const title = titleOf(panel);
  try {
    const opened = await api.addPopoutGroup(panel, { popoutUrl: POPOUT_URL });
    if (opened === false) return { status: 'blocked', panelId: panel.id, title };
    return { status: 'detached', panelId: panel.id, title };
  } catch (error) {
    return {
      status: 'error',
      panelId: panel.id,
      title,
      message: error instanceof Error ? error.message : 'popout failed',
    };
  }
}
