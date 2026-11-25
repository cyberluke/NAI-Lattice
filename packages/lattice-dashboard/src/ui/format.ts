const pad2 = (value: number): string => String(value).padStart(2, '0');

/** Local wall clock, HH:MM:SS. */
export function formatClock(timestampMs: number): string {
  const date = new Date(timestampMs);
  return `${pad2(date.getHours())}:${pad2(date.getMinutes())}:${pad2(date.getSeconds())}`;
}

/** Local wall clock with millisecond precision for log rows. */
export function formatTimestamp(timestampMs: number | null): string {
  if (timestampMs === null || timestampMs <= 0) return '—';
  const date = new Date(timestampMs);
  return `${pad2(date.getHours())}:${pad2(date.getMinutes())}:${pad2(date.getSeconds())}.${String(
    date.getMilliseconds(),
  ).padStart(3, '0')}`;
}

/** Local date and time for historical anchors (since filters, host uptime). */
export function formatDateTime(timestampMs: number | null): string {
  if (timestampMs === null || timestampMs <= 0) return '—';
  const date = new Date(timestampMs);
  return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())} ${pad2(
    date.getHours(),
  )}:${pad2(date.getMinutes())}:${pad2(date.getSeconds())}`;
}

/** Compact relative age; honest "—" when there is no measurement. */
export function formatAge(timestampMs: number | null, nowMs: number): string {
  if (timestampMs === null || timestampMs <= 0) return '—';
  const delta = Math.max(0, nowMs - timestampMs);
  if (delta < 1000) return 'now';
  if (delta < 60_000) return `${Math.floor(delta / 1000)}s ago`;
  if (delta < 3_600_000) return `${Math.floor(delta / 60_000)}m ago`;
  if (delta < 86_400_000) return `${Math.floor(delta / 3_600_000)}h ago`;
  return `${Math.floor(delta / 86_400_000)}d ago`;
}

export function formatDuration(durationMs: number | null): string {
  if (durationMs === null || !Number.isFinite(durationMs)) return '—';
  if (durationMs < 1000) return `${Math.round(durationMs)}ms`;
  if (durationMs < 60_000) return `${(durationMs / 1000).toFixed(durationMs < 10_000 ? 2 : 1)}s`;
  return `${(durationMs / 60_000).toFixed(1)}m`;
}

export function formatUptime(uptimeMs: number | null): string {
  if (uptimeMs === null || uptimeMs < 0) return '—';
  const seconds = Math.floor(uptimeMs / 1000);
  const days = Math.floor(seconds / 86_400);
  const hours = Math.floor((seconds % 86_400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m ${seconds % 60}s`;
  return `${seconds}s`;
}

export function formatCount(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return '—';
  return new Intl.NumberFormat('en-US').format(value);
}

const BYTE_UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'] as const;

/** Binary byte size with an honest dash when the host did not measure it. */
export function formatBytes(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined || !Number.isFinite(bytes) || bytes < 0) return '—';
  if (bytes < 1024) return `${formatCount(bytes)} B`;
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < BYTE_UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = value >= 100 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(digits)} ${BYTE_UNITS[unit]}`;
}

/** Percentage with one decimal below 10%, whole percent above; null stays unknown. */
export function formatPercent(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return '—';
  const clamped = Math.max(0, value);
  return `${clamped < 10 ? clamped.toFixed(1) : Math.round(clamped)}%`;
}

export function shortId(value: string | null, length = 8): string {
  if (value === null || value.length === 0) return '—';
  return value.length <= length ? value : value.slice(0, length);
}

export function displayLevel(level: string): string {
  switch (level.toLowerCase()) {
    case 'warning':
      return 'WARN';
    case 'trace':
    case 'debug':
    case 'info':
    case 'warn':
    case 'error':
    case 'fatal':
      return level.toUpperCase();
    default:
      return level.toUpperCase().slice(0, 5);
  }
}

export function levelCssClass(level: string): string {
  switch (level.toLowerCase()) {
    case 'trace':
      return 'level--trace';
    case 'debug':
      return 'level--debug';
    case 'warn':
    case 'warning':
      return 'level--warn';
    case 'error':
    case 'fatal':
      return 'level--error';
    default:
      return 'level--info';
  }
}
