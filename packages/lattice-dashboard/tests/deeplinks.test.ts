import { describe, expect, it } from 'vitest';
import { parseDeepLinks } from '../src/shell/deeplinks';

describe('deep links', () => {
  it('parses embed, panel, trace, component and time', () => {
    const links = parseDeepLinks(
      '?embed=1&panel=traces&trace=trace-abc&component=nai-os.observability&time=1764000000000',
    );
    expect(links).toEqual({
      embed: true,
      panel: 'traces',
      trace: 'trace-abc',
      component: 'nai-os.observability',
      timeMs: 1_764_000_000_000,
    });
  });

  it('rejects an unknown panel and non-numeric time instead of guessing', () => {
    const links = parseDeepLinks('?panel=made-up&time=not-a-number&trace=%20%20');
    expect(links.panel).toBeNull();
    expect(links.timeMs).toBeNull();
    expect(links.trace).toBeNull();
    expect(links.embed).toBe(false);
  });

  it('accepts a hash-free query string without the leading question mark', () => {
    const links = parseDeepLinks('panel=components&component=nai-pilot');
    expect(links.panel).toBe('components');
    expect(links.component).toBe('nai-pilot');
  });
});
