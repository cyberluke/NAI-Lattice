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
//!
//! # Isolated QA instances (`NAI_LATTICE_INSTANCE`, DASHBOARD_API §6.1)
//!
//! When `NAI_LATTICE_INSTANCE` is set to a valid name, the canonical pipe and
//! the singleton mutex gain the same `.<name>` suffix, the legacy username alias
//! is disabled (it can only point at a production host), and `endpoint.json`
//! records `"instance": "<name>"`. When the variable is unset, behavior is
//! byte-identical to the production endpoint identity. An invalid name is a
//! startup failure — never silently normalized or dropped.

use std::path::PathBuf;

/// Environment variable selecting a fully isolated QA host instance.
pub(crate) const INSTANCE_ENV: &str = "NAI_LATTICE_INSTANCE";

/// Maximum accepted instance-name length (`[A-Za-z0-9_-]{1,24}`).
pub(crate) const INSTANCE_MAX_LEN: usize = 24;

/// Endpoint identity for one persistent singleton host.
#[derive(Debug, Clone)]
pub(crate) struct Endpoints {
    /// `\\.\pipe\NAI.AmbientFabric.NAF1.<sid-hash>` (canonical, SID-based).
    /// Isolated: `\\.\pipe\NAI.AmbientFabric.NAF1.<sid-hash>.<instance>`.
    pub canonical: String,
    /// `\\.\pipe\NAI.AmbientFabric.NAF1.<user-hash>` (legacy alias, same ACL).
    /// Equals `canonical` when an isolated instance disables the alias.
    pub legacy_alias: String,
    /// The user SID string for the DACL.
    pub user_sid: String,
    /// `NAI_LATTICE_INSTANCE` name when this host is an isolated QA instance.
    pub instance: Option<String>,
}

impl Endpoints {
    /// Discover the endpoint identity for this process.
    ///
    /// Fails only on an invalid `NAI_LATTICE_INSTANCE`; the caller must then
    /// refuse to start rather than fall back to the production identity (that
    /// fallback would collide with a real host's singleton/pipe).
    pub(crate) fn discover() -> Result<Self, String> {
        let instance = instance_from_env()?;
        let sid = crate::win_acl::current_user_sid_string()
            .unwrap_or_else(|| "S-1-0-0".to_string());
        let user = std::env::var("USERNAME").ok();
        Ok(Self::from_identity(&sid, user.as_deref(), instance))
    }

    /// Pure identity construction (no environment/filesystem reads).
    fn from_identity(sid: &str, username: Option<&str>, instance: Option<String>) -> Self {
        let sid_hash = blake3::hash(sid.as_bytes()).to_hex().to_string();
        let sid6 = &sid_hash[..6];

        let (canonical, legacy_alias) = match instance.as_deref() {
            Some(name) => {
                // Isolated: one suffixed canonical pipe, no username alias (the
                // alias would point at a production host and leak discovery).
                let pipe = format!(r"\\.\pipe\NAI.AmbientFabric.NAF1.{sid6}.{name}");
                (pipe.clone(), pipe)
            }
            None => {
                let pipe = format!(r"\\.\pipe\NAI.AmbientFabric.NAF1.{sid6}");
                // Legacy alias derived from the lowercased username for in-flight
                // clients that have not yet migrated to canonical discovery.
                // Served by the same singleton with the same SID-only ACL (NOT a
                // separate process).
                let user = username.unwrap_or("unknown").to_lowercase();
                let user_hash = blake3::hash(user.as_bytes()).to_hex().to_string();
                let alias = format!(r"\\.\pipe\NAI.AmbientFabric.NAF1.{}", &user_hash[..6]);
                (pipe, alias)
            }
        };

        Endpoints {
            canonical,
            legacy_alias,
            user_sid: sid.to_string(),
            instance,
        }
    }

    /// Every pipe this host actually serves (canonical first).
    pub(crate) fn names(&self) -> Vec<String> {
        let mut v = vec![self.canonical.clone()];
        if self.legacy_alias != self.canonical {
            v.push(self.legacy_alias.clone());
        }
        v
    }
}

/// Read and validate `NAI_LATTICE_INSTANCE` from the process environment.
pub(crate) fn instance_from_env() -> Result<Option<String>, String> {
    match std::env::var(INSTANCE_ENV) {
        Ok(raw) => parse_instance_name(Some(&raw)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!(
            "{INSTANCE_ENV} is not valid UTF-8; refusing to start"
        )),
    }
}

/// Validate a raw `NAI_LATTICE_INSTANCE` value.
///
/// The value is trimmed first. A variable that is present but blank is invalid
/// (not "unset"): silently treating it as unset would join the production
/// singleton under an operator's nose. Valid names match `[A-Za-z0-9_-]{1,24}`.
pub(crate) fn parse_instance_name(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = raw else { return Ok(None) };
    let name = raw.trim();
    if name.is_empty() {
        return Err(format!(
            "{INSTANCE_ENV} is set but empty; unset it or use [A-Za-z0-9_-]{{1,{INSTANCE_MAX_LEN}}}"
        ));
    }
    let valid_chars = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if name.len() > INSTANCE_MAX_LEN || !valid_chars {
        return Err(format!(
            "invalid {INSTANCE_ENV} value {name:?}: must match [A-Za-z0-9_-]{{1,{INSTANCE_MAX_LEN}}}"
        ));
    }
    Ok(Some(name.to_string()))
}

/// Atomically publish the endpoint manifest into the Lattice runtime root so
/// `@nai-lattice/client` can discover the canonical endpoint without any manual
/// env var. `sidecar.json`-style publication (write tmp + rename = atomic on NTFS).
pub(crate) fn publish_endpoint_manifest(
    runtime_root: &std::path::Path,
    ep: &Endpoints,
    instance_id: &str,
) -> Result<(), String> {
    // The manifest must name the pipe this host actually serves.
    if !ep.names().iter().any(|name| name == &ep.canonical) {
        return Err(format!(
            "refusing to publish a canonical pipe that is not served: {}",
            ep.canonical
        ));
    }
    let control_port = std::env::var("HELLDESK_CTRL_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .filter(|p| *p != 0)
        .unwrap_or(8787);
    let mut manifest = serde_json::json!({
        "app": "nai-lattice",
        // The shell discovers the running host from THIS canonical manifest,
        // rather than assuming its own unrelated environment has the port.
        "control_http_port": control_port,
        "endpoint_kind": "naf1",
        "protocol": {"min": 1, "max": 1, "selected": 1},
        "canonical_pipe": ep.canonical,
        "legacy_alias": ep.legacy_alias,
        "sid": ep.user_sid,
        "instance_id": instance_id,
        "published_at_unix_ms": crate::now_unix_ms(),
        "pid": std::process::id(),
    });
    // Only an isolated host advertises its instance; production manifests keep
    // their exact previous key set.
    if let (Some(instance), Some(obj)) = (ep.instance.as_deref(), manifest.as_object_mut()) {
        obj.insert("instance".to_string(), serde_json::json!(instance));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "S-1-5-21-1111-2222-3333-1001";

    fn sid_pipe_suffix() -> String {
        let hash = blake3::hash(SID.as_bytes()).to_hex().to_string();
        hash[..6].to_string()
    }

    #[test]
    fn instance_name_is_trimmed_and_validated() {
        assert_eq!(parse_instance_name(None).expect("unset is valid"), None);
        assert_eq!(
            parse_instance_name(Some("  qa-dashboard  ")).expect("trimmed"),
            Some("qa-dashboard".to_string())
        );
        assert_eq!(
            parse_instance_name(Some("QA_1-x")).expect("allowed charset"),
            Some("QA_1-x".to_string())
        );
        for bad in ["", "   ", "has space", "slash/yes", "dot.dot", "qä"] {
            assert!(
                parse_instance_name(Some(bad)).is_err(),
                "{bad:?} must be rejected, never normalized"
            );
        }
        let too_long = "a".repeat(INSTANCE_MAX_LEN + 1);
        assert!(parse_instance_name(Some(&too_long)).is_err());
        let max = "a".repeat(INSTANCE_MAX_LEN);
        assert_eq!(
            parse_instance_name(Some(&max)).expect("24 chars allowed"),
            Some(max)
        );
    }

    #[test]
    fn default_endpoint_names_are_unchanged() {
        let ep = Endpoints::from_identity(SID, Some("Alice"), None);
        let sid6 = sid_pipe_suffix();
        let user_hash = blake3::hash(b"alice").to_hex().to_string();
        assert_eq!(
            ep.canonical,
            format!(r"\\.\pipe\NAI.AmbientFabric.NAF1.{sid6}")
        );
        assert_eq!(
            ep.legacy_alias,
            format!(r"\\.\pipe\NAI.AmbientFabric.NAF1.{}", &user_hash[..6])
        );
        assert_ne!(ep.legacy_alias, ep.canonical);
        assert_eq!(ep.instance, None);
        assert_eq!(ep.names(), vec![ep.canonical.clone(), ep.legacy_alias.clone()]);
        assert_eq!(ep.user_sid, SID);
    }

    #[test]
    fn isolated_endpoint_name_has_suffix_and_no_legacy_alias() {
        let ep = Endpoints::from_identity(SID, Some("Alice"), Some("qa-dashboard".into()));
        let expected = format!(
            r"\\.\pipe\NAI.AmbientFabric.NAF1.{}.qa-dashboard",
            sid_pipe_suffix()
        );
        assert_eq!(ep.canonical, expected);
        assert_eq!(ep.legacy_alias, expected, "isolated hosts serve no alias");
        assert_eq!(ep.instance.as_deref(), Some("qa-dashboard"));
        assert_eq!(
            ep.names(),
            vec![expected],
            "exactly one pipe name (canonical) is served"
        );
    }

    #[test]
    fn manifest_carries_instance_only_when_isolated() {
        let dir = tempfile::tempdir().expect("tempdir");

        let default_ep = Endpoints::from_identity(SID, Some("Alice"), None);
        publish_endpoint_manifest(dir.path(), &default_ep, "runtime-prod").expect("publish");
        let text = std::fs::read_to_string(dir.path().join("endpoint.json")).expect("read");
        let v: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(v["canonical_pipe"], default_ep.canonical);
        assert!(
            v.get("instance").is_none(),
            "production manifest must not gain an instance field"
        );
        for key in [
            "canonical_pipe",
            "control_http_port",
            "pid",
            "instance_id",
            "published_at_unix_ms",
        ] {
            assert!(v.get(key).is_some(), "manifest lost required key {key}");
        }

        let isolated_ep =
            Endpoints::from_identity(SID, Some("Alice"), Some("qa-dashboard".into()));
        publish_endpoint_manifest(dir.path(), &isolated_ep, "runtime-qa").expect("publish qa");
        let text = std::fs::read_to_string(dir.path().join("endpoint.json")).expect("read");
        let v: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(v["instance"], "qa-dashboard");
        assert_eq!(v["canonical_pipe"], isolated_ep.canonical);
        assert_eq!(
            v["canonical_pipe"], v["legacy_alias"],
            "isolated isolation: the advertised alias is the canonical pipe"
        );
        assert_eq!(v["instance_id"], "runtime-qa");
        assert_eq!(
            v["pid"].as_u64(),
            Some(u64::from(std::process::id())),
            "manifest must advertise the serving pid"
        );
    }
}
