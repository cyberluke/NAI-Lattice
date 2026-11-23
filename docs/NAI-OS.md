# NAI OS — first production integration

<img src="../assets/nai-os-logo.png" alt="NAI OS" width="420" />

**NAI OS** is the first production-ready integration of NAI Lattice — an
AI-native Windows desktop with a dock, launcher, window manager, widgets,
themes, a curated store for AI and agentic apps (MCP + `SKILL.md`), and
N-Pilot, its local agent.

## What NAI OS brings

- AI-native shell: dock, launcher, window manager, settings, widgets, themes.
- Curated store for AI apps and agentic apps, onboarded through the Enhancer
  wizard.
- N-Pilot: local agent with on-device vision/OCR, STT/TTS and permissioned
  tools.

## How it uses NAI Lattice

| NAI OS surface | NAI Lattice |
|---|---|
| N-Pilot | task/thread/relationship state in the canonical ledger |
| Dashboard | live dashboard + SSE on `127.0.0.1:8787`, same ledger |
| AI/agentic app store | durable app state; CAS/AAF for reusable assets |
| Host lifecycle | versioned runtime, `active.json`, monitor/worker, live swap, crash recovery |

## What it proves

NAI OS exercises the full production path — install, activation, live upgrade,
crash recovery and long-running local state — in a shipping desktop product.

## Verification

The installer checks the host manifest before launch. Signing and trust are
covered by the NAI OS security policy.

## References

- NAI OS: <https://github.com/cyberluke/NAI-OS>
- Host install/activation: [PRODUCT_BUNDLING.md](PRODUCT_BUNDLING.md)
- Signing and trust: see the NAI OS repository security policy.
