import type { SerializedDockview } from 'dockview-core';

export const WORKSPACE_STORAGE_KEY = 'nai.lattice.workspace.v1';
export const WORKSPACE_CORRUPT_KEY = `${WORKSPACE_STORAGE_KEY}.corrupt`;
/**
 * v4 adds Host & GPU (right group) and Topology (bottom group). A schema bump
 * resets once with a visible notice: applying an older layout would silently
 * hide both new modules.
 *
 * Execution Chain is additive inside v4: it is validated as a known module and
 * seeded into fresh/reset layouts, while an existing v4 layout stays untouched.
 * Adding it must not invalidate a saved workspace (§ standalone module contract).
 */
export const WORKSPACE_VERSION = 4;

export const MODULE_TYPES = [
  'log-tail',
  'traces',
  'components',
  'health',
  'storage',
  'overview',
  'errors',
  'discover',
  'host',
  'topology',
  'execution-chain',
] as const;
export type ModuleType = (typeof MODULE_TYPES)[number];

export const MODULE_TITLES: Record<ModuleType, string> = {
  'log-tail': 'Log Tail',
  traces: 'Request → Response Traces',
  components: 'Components',
  health: 'Health & Stream',
  storage: 'Storage',
  overview: 'Overview',
  errors: 'Error Intelligence',
  discover: 'Discover',
  host: 'Host & GPU',
  topology: 'Topology',
  'execution-chain': 'Execution Chain',
};

export interface ModuleParams {
  /** Stable module type; survives persistence and versioning. */
  moduleType: ModuleType;
  /** Unique panel instance; the Dockview panel id. */
  instanceId: string;
}

export function isModuleType(value: unknown): value is ModuleType {
  return typeof value === 'string' && (MODULE_TYPES as readonly string[]).includes(value);
}

export function seedInstanceId(type: ModuleType): string {
  return `${type}@1`;
}

export function nextInstanceId(type: ModuleType, existing: Iterable<string>): string {
  const known = new Set(existing);
  let index = 2;
  while (known.has(`${type}@${index}`)) index += 1;
  return `${type}@${index}`;
}

export function isModuleParams(value: unknown): value is ModuleParams {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return false;
  const raw = value as Record<string, unknown>;
  return isModuleType(raw['moduleType']) && typeof raw['instanceId'] === 'string' && raw['instanceId'].length > 0;
}

export interface PersistedWorkspace {
  version: number;
  savedAt: string;
  layout: SerializedDockview;
}

export type WorkspaceResetReason =
  | 'malformed-json'
  | 'invalid-shape'
  | 'version-mismatch'
  | 'unknown-module';

export interface WorkspaceLoadResult {
  layout: SerializedDockview | null;
  reason: WorkspaceResetReason | 'missing' | 'ok';
  notice: string | null;
  /** Raw value to archive for diagnosis before the key is cleared. */
  corruptRaw: string | null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

/**
 * Validates a persisted workspace before Dockview ever sees it. A layout that
 * references an unknown module type or a missing instance id is rejected as a
 * whole, so a stale module cannot crash the shell on restore.
 */
export function validateLayout(layout: unknown): WorkspaceResetReason | null {
  if (!isRecord(layout)) return 'invalid-shape';
  if (!isRecord(layout['grid'])) return 'invalid-shape';
  if (!isRecord((layout['grid'] as Record<string, unknown>)['root'])) return 'invalid-shape';
  const panels = layout['panels'];
  if (!isRecord(panels)) return 'invalid-shape';
  const entries = Object.entries(panels);
  if (entries.length === 0) return 'invalid-shape';
  for (const [panelId, panel] of entries) {
    if (!isRecord(panel)) return 'invalid-shape';
    // Dockview's serialized panel state names the content renderer
    // `contentComponent`; anything else cannot be rehydrated by fromJSON.
    if (panel['contentComponent'] !== 'module') return 'invalid-shape';
    const params = panel['params'];
    if (!isModuleParams(params)) return 'unknown-module';
    if (params.instanceId !== panelId) return 'invalid-shape';
  }
  return null;
}

export function decodeWorkspace(raw: string | null): WorkspaceLoadResult {
  if (raw === null) {
    return { layout: null, reason: 'missing', notice: null, corruptRaw: null };
  }
  if (raw.length > 4_000_000) {
    return {
      layout: null,
      reason: 'invalid-shape',
      notice: 'Saved workspace was too large and has been reset to the canonical layout.',
      corruptRaw: raw,
    };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw) as unknown;
  } catch {
    return {
      layout: null,
      reason: 'malformed-json',
      notice: 'Saved workspace was malformed and has been reset to the canonical layout.',
      corruptRaw: raw,
    };
  }
  if (!isRecord(parsed)) {
    return {
      layout: null,
      reason: 'invalid-shape',
      notice: 'Saved workspace had an invalid shape and has been reset to the canonical layout.',
      corruptRaw: raw,
    };
  }
  const version = parsed['version'];
  if (version !== WORKSPACE_VERSION) {
    return {
      layout: null,
      reason: 'version-mismatch',
      notice: `Saved workspace schema version ${String(
        version,
      )} was migrated: the canonical layout for version ${WORKSPACE_VERSION} was restored. Your previous layout is archived and was not applied.`,
      corruptRaw: raw,
    };
  }
  const layout = parsed['layout'];
  const invalid = validateLayout(layout);
  if (invalid !== null) {
    return {
      layout: null,
      reason: invalid,
      notice:
        invalid === 'unknown-module'
          ? 'Saved workspace referenced an unknown module; the canonical layout was restored.'
          : 'Saved workspace could not be validated and has been reset to the canonical layout.',
      corruptRaw: raw,
    };
  }
  return {
    layout: layout as SerializedDockview,
    reason: 'ok',
    notice: null,
    corruptRaw: null,
  };
}

export function encodeWorkspace(layout: SerializedDockview, savedAt: string): string {
  const payload: PersistedWorkspace = { version: WORKSPACE_VERSION, savedAt, layout };
  return JSON.stringify(payload);
}
