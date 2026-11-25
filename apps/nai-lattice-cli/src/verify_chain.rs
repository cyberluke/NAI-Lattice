//! `verify-chain`: a strictly read-only audit of the canonical ledger hash
//! chain.
//!
//! Unlike `verify-ledger` (which opens the runtime and therefore runs crash
//! recovery, truncating a torn tail), this command never opens the ledger. It
//! uses the ledger crate's read-only readers and reports:
//!
//! * every sealed and active segment with its frame count and sequence range;
//! * `truncate_offset` where a torn/corrupt/chain-broken tail starts;
//! * hidden bytes that are not part of a verified chain;
//! * `OK` / `BROKEN` for the whole chain.
//!
//! Usage:
//!   nai-lattice-cli verify-chain [--json]
//!
//! Exit codes:
//!   0  chain intact
//!   4  broken/integrity error (reported, never repaired)
//!   5  I/O failure (the audit could not read the ledger)
//!   2  usage error (unknown option)
//!
//! The canonical root honors `NAI_LATTICE_ROOT` (via `RuntimeConfig`), so a QA
//! drill audits the same tree the CLI/host use for that root and never the
//! production tree by accident.

use nai_lattice_ledger::audit::{audit_chain, ChainAudit};
use nai_lattice_runtime::RuntimeConfig;

/// Exit code for a detected integrity failure.
pub const EXIT_BROKEN: u8 = 4;
/// Exit code for an I/O failure while auditing.
pub const EXIT_IO: u8 = 5;
/// Exit code for an invalid invocation (kept consistent with the other commands).
pub const EXIT_USAGE: u8 = 2;

pub fn run(raw: &[String]) -> u8 {
    let mut json = false;
    for arg in raw {
        match arg.as_str() {
            "--json" => json = true,
            other => {
                eprintln!("verify-chain: unknown option `{other}`");
                print_usage();
                return EXIT_USAGE;
            }
        }
    }

    let root = RuntimeConfig::default().root;
    match audit_chain(&root) {
        Ok(audit) => {
            if json {
                match serde_json::to_string_pretty(&audit) {
                    Ok(payload) => println!("{payload}"),
                    Err(e) => {
                        eprintln!("verify-chain: serialize audit: {e}");
                        return EXIT_IO;
                    }
                }
            } else {
                print_human(&audit);
            }
            if audit.ok {
                0
            } else {
                EXIT_BROKEN
            }
        }
        Err(e) => {
            eprintln!("verify-chain: {e}");
            EXIT_IO
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage: nai-lattice-cli verify-chain [--json]\n\
         \n\
         Strictly read-only ledger chain audit (never opens/repairs the ledger).\n\
         exit codes: 0 ok, 4 broken/integrity error, 5 I/O, 2 usage"
    );
}

fn print_human(audit: &ChainAudit) {
    println!("chain audit: {}", audit.root);
    if audit.segments.is_empty() {
        println!("  (no segments: the ledger does not exist under this root)");
    }
    let width = audit
        .segments
        .iter()
        .map(|segment| segment.path.len())
        .max()
        .unwrap_or(0);
    for segment in &audit.segments {
        let seq = match (segment.first_seq, segment.last_seq) {
            (Some(first), Some(last)) => format!("seq {first}..{last}"),
            _ => "seq -".to_string(),
        };
        let tail = match segment.truncate_offset {
            Some(offset) => format!("broken at offset {offset}"),
            None => format!("{} B", segment.bytes),
        };
        println!(
            "  {:<width$}  {:>8} frames  {:<14} {:>12}  {}",
            segment.path,
            segment.frames,
            seq,
            tail,
            if segment.broken { "BROKEN" } else { "OK" },
            width = width
        );
    }
    if audit.ok {
        println!(
            "OK ({} segment(s), {} hidden bytes)",
            audit.segments.len(),
            audit.hidden_bytes
        );
    } else {
        let broken = audit
            .segments
            .iter()
            .filter(|segment| segment.broken)
            .count();
        println!(
            "BROKEN ({} of {} segment(s), {} hidden bytes; nothing was repaired)",
            broken,
            audit.segments.len(),
            audit.hidden_bytes
        );
    }
}
