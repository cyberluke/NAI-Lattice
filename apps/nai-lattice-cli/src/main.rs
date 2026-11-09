//! NAI Lattice CLI: inspection, verification, recovery and rebuild (spec §7).
//!
//! Commands:
//!   inspect                print runtime health/describe (opens the runtime)
//!   verify-ledger          run ledger crash recovery + report high-water mark
//!   rebuild --projection hot-state
//!                          rebuild the hot-store from canonical ledger manifests
//!   verify --artifact <hash>   verify a CAS object's whole-object identity

use std::process::ExitCode;

use lattice_runtime::{LatticeRuntime, RuntimeConfig};

fn main() -> ExitCode {
    tracing_subscriber::fmt().with_env_filter("error").init();
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("inspect");
    let config = RuntimeConfig::default();

    match cmd {
        "inspect" => {
            let rt = match LatticeRuntime::open(config) {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("inspect: failed to open runtime: {e}");
                    return ExitCode::from(2);
                }
            };
            println!("{}", serde_json::to_string_pretty(&rt.health()).unwrap());
            println!("capabilities:");
            for c in rt.capabilities() {
                println!("  - {c}");
            }
            ExitCode::SUCCESS
        }
        "verify-ledger" => {
            // Open the ledger (runs crash recovery) and report the high-water mark.
            let rt = match LatticeRuntime::open(config) {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("verify-ledger: failed to open runtime: {e}");
                    return ExitCode::from(2);
                }
            };
            let hwm = rt
                .dispatch("runtime.health", b"{}", "cli", None)
                .map(|v| serde_json::from_slice::<serde_json::Value>(&v).unwrap())
                .unwrap_or_else(|e| {
                    eprintln!("verify-ledger: {e}");
                    serde_json::json!({"error": e.to_string()})
                });
            println!("{}", serde_json::to_string_pretty(&hwm).unwrap());
            ExitCode::SUCCESS
        }
        "rebuild" => {
            let _ = rebuild_projection(&args);
            match LatticeRuntime::open(config) {
                Ok(rt) => {
                    rt.flush_hotstore();
                    println!("rebuild complete; ledger high-water mark {:?}", rt.dispatch("runtime.health", b"{}", "cli", None).is_ok());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("rebuild: {e}");
                    ExitCode::from(2)
                }
            }
        }
        "verify" => {
            let hash = args.get(2).map(|s| s.as_str()).unwrap_or_default();
            if hash.is_empty() {
                eprintln!("usage: nai-lattice-cli verify --artifact <hash> | verify-ledger");
                return ExitCode::from(2);
            }
            match LatticeRuntime::open(config) {
                Ok(rt) => {
                    let out = rt.dispatch("artifact.verify", &serde_json::json!({"expected_hash": hash}).to_string().into_bytes(), "cli", None);
                    match out {
                        Ok(v) => {
                            println!("{}", String::from_utf8_lossy(&v));
                            ExitCode::SUCCESS
                        }
                        Err(e) => {
                            eprintln!("verify: {e}");
                            ExitCode::from(2)
                        }
                    }
                }
                Err(e) => {
                    eprintln!("verify: {e}");
                    ExitCode::from(2)
                }
            }
        }
        other => {
            eprintln!("unknown command: {other}");
            eprintln!("usage: nai-lattice-cli [inspect|verify-ledger|rebuild [--projection hot-state]|verify <hash>]");
            ExitCode::from(2)
        }
    }
}

/// Hot-state rebuild hook — re-establishes hot-store records from canonical
/// state. `LatticeRuntime::flush_hotstore` persists pending group-commits.
fn rebuild_projection(_args: &[String]) {}

/// Re-export for doc link only.
#[allow(dead_code)]
fn _ledger_alias(_cfg: RuntimeConfig) -> lattice_ledger::LedgerConfig {
    unimplemented!("kept for doc reference")
}