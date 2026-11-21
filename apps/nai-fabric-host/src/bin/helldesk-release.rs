//! `helldesk-release` — console maintenance CLI for versioned Helldesk release
//! install / activation / rollback / launch / status.
//!
//! A CONSOLE-subsystem binary on purpose: the operator-facing packaging tool
//! prints its JSON result to stdout. The actual Gas Town host
//! (`nai-fabric-host`) is a no-console windows-subsystem daemon by design
//! (PROC-005: no console window for the background host); this CLI talks to the
//! same release store under `%LOCALAPPDATA%\NAI\Lattice\releases` so operators
//! can establish, audit, and roll back the active release from a terminal.

// Console subsystem (opposite of the host's no-console default).
#[path = "../release.rs"]
mod release;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    release::release_main(args);
}