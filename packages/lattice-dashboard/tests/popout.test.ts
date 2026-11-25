import { describe, expect, it, vi } from 'vitest';
import popoutHtml from '../public/popout.html?raw';
import { POPOUT_URL, popoutActivePanel, type PopoutCapableApi } from '../src/shell/popout';

function api(overrides: Partial<PopoutCapableApi> = {}): PopoutCapableApi {
  return {
    activePanel: { id: 'log-tail@1', title: 'Log Tail' },
    addPopoutGroup: vi.fn(async () => true),
    ...overrides,
  };
}

describe('popout action', () => {
  it('detaches the active panel with the bundled same-origin page', async () => {
    const addPopoutGroup = vi.fn(async () => true);
    const outcome = await popoutActivePanel(api({ addPopoutGroup }));
    expect(outcome).toEqual({ status: 'detached', panelId: 'log-tail@1', title: 'Log Tail' });
    expect(addPopoutGroup).toHaveBeenCalledWith(
      { id: 'log-tail@1', title: 'Log Tail' },
      { popoutUrl: POPOUT_URL },
    );
    expect(POPOUT_URL).toBe('/lattice/popout.html');
  });

  it('keeps the panel docked when the popout is blocked', async () => {
    const outcome = await popoutActivePanel(api({ addPopoutGroup: async () => false }));
    expect(outcome).toEqual({ status: 'blocked', panelId: 'log-tail@1', title: 'Log Tail' });
  });

  it('keeps the panel docked and reports a rejected window', async () => {
    const outcome = await popoutActivePanel(
      api({
        addPopoutGroup: async () => {
          throw new Error('window.open returned null');
        },
      }),
    );
    expect(outcome).toEqual({
      status: 'error',
      panelId: 'log-tail@1',
      title: 'Log Tail',
      message: 'window.open returned null',
    });
  });

  it('reports no active module without calling the API', async () => {
    const addPopoutGroup = vi.fn(async () => true);
    const outcome = await popoutActivePanel(api({ activePanel: undefined, addPopoutGroup }));
    expect(outcome).toEqual({ status: 'no-active-panel' });
    expect(addPopoutGroup).not.toHaveBeenCalled();
  });
});

describe('popout document chrome', () => {
  function popoutCss(): string {
    const match = popoutHtml.match(/<style>([\s\S]*?)<\/style>/);
    if (match === null) throw new Error('popout.html has no inline stylesheet');
    return match[1];
  }

  it('hides the dead tab strip and fills the window with the detached group', () => {
    expect(popoutHtml).toContain('#dv-popout-window .dv-tabs-and-actions-container');
    expect(popoutCss()).toMatch(
      /#dv-popout-window \.dv-tabs-and-actions-container\s*\{[^}]*display:\s*none\s*!important/,
    );
    expect(popoutCss()).toMatch(/#dv-popout-window \.dv-groupview[^{]*\{[^}]*height:\s*100%\s*!important/);
    expect(popoutCss()).toMatch(
      /#dv-popout-window \.dv-content-container[^{]*\{[^}]*flex:\s*1 1 auto/,
    );
  });

  it('computes no tab strip and a full-height content area in a popout-shaped DOM', () => {
    const style = document.createElement('style');
    style.textContent = popoutCss();
    document.head.appendChild(style);
    document.body.innerHTML = `
      <div id="dv-popout-window">
        <div class="dv-groupview dv-groupview-popout">
          <div class="dv-tabs-and-actions-container"><span class="dv-tab">Host &amp; GPU</span></div>
          <div class="dv-content-container"><div class="dv-vue-part">panel content</div></div>
        </div>
      </div>
    `;

    const tabs = document.querySelector('.dv-tabs-and-actions-container') as HTMLElement;
    const group = document.querySelector('.dv-groupview') as HTMLElement;
    const content = document.querySelector('.dv-content-container') as HTMLElement;
    const part = document.querySelector('.dv-vue-part') as HTMLElement;
    expect(getComputedStyle(tabs).display).toBe('none');
    expect(getComputedStyle(group).height).toBe('100%');
    expect(getComputedStyle(content).height).toBe('100%');
    expect(getComputedStyle(part).height).toBe('100%');

    style.remove();
    document.body.innerHTML = '';
  });
});
