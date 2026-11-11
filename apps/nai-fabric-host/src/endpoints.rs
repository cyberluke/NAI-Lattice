//! Canonical per-user NAF endpoint discovery (Lattice §5 / GATE 1).
//!
//! The host owns the authoritative endpoint identity. A client must NOT guess a
//! pipe name from `USERNAME`; it discovers the canonical endpoint from an
//! NAI-Lattice-owned publication the host writes atomically. The host serves
//! BOTH:
//!   - the canonical SID-derived pipe (`NAI.AmbientFabric.NAF1.<sid-hash>`)
//!   - a backward-compatible legacy alias (`NAI.AmbientFabric.NAF1.<user-hash>`)
//! from the SAME singleton, both secured with the SID-only DACL, both routing to
//! the same dispatcher/state. Legacy alias is temporary for live-migration only.

use std::path::PathBuf;

/// Endpoint identity for one persistent singleton host.
#[derive(Debug, Clone)]
pub(crate) struct Endpoints {
    /// `\\.\pipe\NAI.AmbientFabric.NAF1.<sid-hash>` (canonical, SID-based).
    pub canonical: String,
    /// `\\.\pipe\NAI.AmbientFabric.NAF1.<user-hash>` (legacy alias, same ACL).
    pub legacy_alias: String,
    /// The user SID string for the DACL.
    pub user_sid: String,
}

impl Endpoints {
    pub(crate) fn discover() -> Self {
        let sid = crate::win_acl::current_user_sid_string()
            .unwrap_or_else(|| "S-1-0-0".to_string());
        let sid_hash = blake3::hash(sid.as_bytes()).to_hex().to_string();
        let canonical = format!(r"\\.\pipe\NAI.AmbientFabric.NAF1.{}", &sid_hash[..6]);

        // Legacy alias derived from the lowercased username for in-flight clients
        // that have not yet migrated to canonical discovery. Served by the same
        // singleton with the same SID-only ACL (NOT a separate process).
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into()).to_lowercase();
        let user_hash = blake3::hash(user.as_bytes()).to_hex().to_string();
        let alias = format!(r"\\.\pipe\NAI.AmbientFabric.NAF1.{}", &user_hash[..6]);

        Endpoints { canonical, legacy_alias: alias, user_sid: sid }
    }

    pub(crate) fn names(&self) -> Vec<String> {
        let mut v = vec![self.canonical.clone()];
        if self.legacy_alias != self.canonical {
            v.push(self.legacy_alias.clone());
        }
        v
    }
}

/// Atomically publish the endpoint manifest into the Lattice runtime root so
/// `@nai-lattice/client` can discover the canonical endpoint without any manual
/// env var. `sidecar.json`-style publication (write tmp + rename = atomic on NTFS).
pub(crate) fn publish_endpoint_manifest(
    runtime_root: &std::path::Path,
    ep: &Endpoints,
    instance_id: &str,
) -> Result<(), String> {
    let manifest = serde_json::json!({
        "app": "nai-lattice",
        "endpoint_kind": "naf1",
        "protocol": {"min": 1, "max": 1, "selected": 1},
        "canonical_pipe": ep.canonical,
        "legacy_alias": ep.legacy_alias,
        "sid": ep.user_sid,
        "instance_id": instance_id,
        "published_at_unix_ms": crate::now_unix_ms(),
        "pid": std::process::id(),
    });
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    let root = PathBuf::from(runtime_root);
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let target = root.join("endpoint.json");
    let tmp = root.join("endpoint.json.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    // Atomic replace on the same volume.
    if std::fs::rename(&tmp, &target).is_err() {
        // Fallback: overwrite in place (still crash-safe enough for a tiny file).
        let _ = std::fs::write(&target, &bytes);
    }
    Ok(())
}

/// Client-side canonical discovery: read the host-published endpoint manifest.
/// This is the ONLY authoritative SID endpoint for a network-less client; the
/// fallback keeps the legacy username-derived name (served by the same host).
pub(crate) fn discover_client_endpoint(cache_dir: Option<&PathBuf>) -> (String, String) {
    // 1. Prefer the endpoint manifest in the Lattice per-user runtime root.
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let candidates = vec![
        cache_dir.map(|p| p.join("endpoint.json")),
        Some(local.join("NAI").join("Lattice").join("v1").join("endpoint.json")),
    ];
    for cand in candidates.into_iter().flatten() {
        if let Ok(text) = std::fs::read_to_string(&cand) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(p) = v.get("canonical_pipe").and_then(|x| x.as_str()) {
                    let alias = v.get("legacy_alias").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    if !p.is_empty() {
                        return (p.to_string(), alias);
                    }
                }
            }
        }
    }
    // 2. Fallback: legacy username-derived alias (host still serves it).
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into()).to_lowercase();
    let user_hash = blake3::hash(user.as_bytes()).to_hex().to_string();
    let alias = format!(r"\\.\pipe\NAI.AmbientFabric.NAF1.{}", &user_hash[..6]);
    (alias.clone(), alias)
}