import { isModuleType, type ModuleType } from '../workspace/schema';

export interface DeepLinks {
  embed: boolean;
  panel: ModuleType | null;
  trace: string | null;
  component: string | null;
  timeMs: number | null;
}

function nonEmpty(value: string | null): string | null {
  if (value === null) return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
}

/** Parses `?embed=1&panel=…&trace=…&component=…&time=<epoch_ms>`. */
export function parseDeepLinks(search: string): DeepLinks {
  const params = new URLSearchParams(search.startsWith('?') ? search.slice(1) : search);
  const rawPanel = params.get('panel');
  const panel = isModuleType(rawPanel) ? rawPanel : null;
  const embedRaw = params.get('embed');
  const embed = embedRaw === '1' || embedRaw === 'true';
  const rawTime = params.get('time');
  let timeMs: number | null = null;
  if (rawTime !== null && rawTime.trim() !== '') {
    const parsed = Number(rawTime);
    if (Number.isFinite(parsed) && parsed > 0) timeMs = Math.trunc(parsed);
  }
  return {
    embed,
    panel,
    trace: nonEmpty(params.get('trace')),
    component: nonEmpty(params.get('component')),
    timeMs,
  };
}
