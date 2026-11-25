import type { Context } from '@finos/fdc3';
import type { LogRecord } from '../telemetry/types';

export const LOG_SELECTION_CONTEXT = 'nai.lattice.logSelection';
export const TRACE_CONTEXT = 'nai.lattice.trace';
export const INSPECT_TRACE_INTENT = 'InspectTrace';

/**
 * Minimal FDC3 selection context. No message body, prompt, token or credential
 * ever leaves the dashboard.
 *
 * FDC3 identifier values SHOULD be strings, so the numeric sequence is carried
 * both as a string id and as the typed `sequence` field.
 */
export interface LogSelectionContext extends Context {
  type: typeof LOG_SELECTION_CONTEXT;
  schema_version: 1;
  id: {
    runtime_sequence: string;
    trace_id?: string;
  };
  sequence: number;
  level: string;
  component: string;
  timestamp_utc_ms: number;
}

export interface TraceContext extends Context {
  type: typeof TRACE_CONTEXT;
  schema_version: 1;
  id: {
    trace_id: string;
  };
}

export function buildLogSelectionContext(record: LogRecord): LogSelectionContext {
  const id: LogSelectionContext['id'] = {
    runtime_sequence: String(record.runtime_sequence),
  };
  if (record.trace_id !== null && record.trace_id.length > 0) {
    id.trace_id = record.trace_id;
  }
  return {
    type: LOG_SELECTION_CONTEXT,
    schema_version: 1,
    id,
    sequence: record.runtime_sequence,
    level: record.level,
    component: record.component,
    timestamp_utc_ms: record.timestamp_utc_ms,
  };
}

export function buildTraceContext(traceId: string): TraceContext {
  return {
    type: TRACE_CONTEXT,
    schema_version: 1,
    id: { trace_id: traceId },
  };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

/** Reads a trace id from a `nai.lattice.trace` context; rejects everything else. */
export function readTraceId(context: unknown): string | null {
  if (!isRecord(context)) return null;
  if (context['type'] !== TRACE_CONTEXT) return null;
  const schema = context['schema_version'];
  if (schema !== undefined && schema !== 1) return null;
  const id = context['id'];
  if (!isRecord(id)) return null;
  const traceId = id['trace_id'];
  return typeof traceId === 'string' && traceId.length > 0 ? traceId : null;
}

export function isLogSelectionContext(context: unknown): context is LogSelectionContext {
  if (!isRecord(context) || context['type'] !== LOG_SELECTION_CONTEXT) return false;
  if (context['schema_version'] !== 1) return false;
  const id = context['id'];
  if (!isRecord(id)) return false;
  return typeof id['runtime_sequence'] === 'string' && typeof context['sequence'] === 'number';
}
