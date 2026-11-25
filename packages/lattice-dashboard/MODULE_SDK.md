# Lattice dashboard module SDK v1

The shell installs one same-origin global for embedded modules:

```ts
window.naiLattice: {
  version: 1;
  channel: string | null;
  focusTrace(id: string): void;
  raiseSelection(context: unknown): Promise<boolean>;
}
```

Source: `src/sdk/module-sdk.ts`; declaration: `src/types/nai-lattice.d.ts`.

## Rules

- The SDK is same-origin only. It does not create or impersonate a Desktop
  Agent; with no agent, `channel` is `null` and raises resolve `false`.
- `focusTrace(id)` trims the id, sets the shell's trace filter, requests the
  Traces panel, and raises `nai.lattice.trace` on the app channel when one is
  available.
- `raiseSelection(context)` validates a `nai.lattice.logSelection` v1 context
  (`schema_version: 1`, string `id.runtime_sequence`, numeric `sequence`) before
  raising it. Invalid contexts are rejected locally and never broadcast.
- `channel` is `nai.lattice` only after the shell joined the app channel. When
  the Desktop Agent lacks channel support the shell uses the agent's default
  channel; `channel` is then `null` so a module does not claim the wrong scope.
- No message body, prompt, token or credential is added by the SDK. The
  selection context carries only sequence, level, component and timestamp.

## Example

```ts
const sdk = window.naiLattice;
if (sdk?.version === 1) {
  sdk.focusTrace('trace-abc');
  await sdk.raiseSelection({
    type: 'nai.lattice.logSelection',
    schema_version: 1,
    id: { runtime_sequence: '42', trace_id: 'trace-abc' },
    sequence: 42,
    level: 'warn',
    component: 'nai-os.agent',
    timestamp_utc_ms: Date.now(),
  });
}
```

The shell removes `window.naiLattice` on teardown only when it still points to
the instance the shell installed.
