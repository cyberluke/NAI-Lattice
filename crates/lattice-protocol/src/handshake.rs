//! NAF/1 handshake negotiation (Lattice spec §8, §naf-versioning).
//!
//! Products negotiate the mutually supported NAF version and the runtime
//! advertises its capabilities. Retries with the same idempotency key must
//! never produce duplicate durable events — enforced at the ledger layer.

use crate::pb;

/// Build the client's `HandshakeRequest`.
pub fn client_request(
    app_id: &str,
    version: &str,
    pid: u32,
    naf_min: u32,
    naf_max: u32,
    requested_capabilities: &[&str],
    package_identity: Option<&str>,
    build_sha: &str,
    nonce: &[u8],
) -> pb::HandshakeRequest {
    pb::HandshakeRequest {
        client_app_id: app_id.to_string(),
        client_version: version.to_string(),
        client_pid: pid as i64,
        naf_min,
        naf_max,
        requested_capabilities: requested_capabilities.iter().map(|s| s.to_string()).collect(),
        package_identity: package_identity.map(str::to_string).unwrap_or_default(),
        build_sha: build_sha.to_string(),
        nonce: nonce.to_vec(),
    }
}

/// Build the runtime's `HandshakeResponse` from the request + advertised caps.
pub fn server_response(
    request: &pb::HandshakeRequest,
    capabilities: &[&str],
    runtime_instance_id: &str,
    storage_schema_version: u32,
) -> std::result::Result<pb::HandshakeResponse, crate::ProtocolError> {
    let selected = crate::select_version(
        request.naf_min,
        request.naf_max,
        crate::NAF_VERSION_1,
        crate::NAF_VERSION_1,
    )
    .ok_or_else(|| {
        crate::ProtocolError::Negotiation(format!(
            "incompatible NAF range: client [{},{}], runtime [1,1]",
            request.naf_min, request.naf_max
        ))
    })?;

    Ok(pb::HandshakeResponse {
        runtime_version: handshake_runtime_version().to_string(),
        runtime_build_sha: "dev".to_string(),
        selected_naf_version: selected,
        capabilities: capabilities
            .iter()
            .map(|c| pb::Capability {
                name: c.to_string(),
                version: "1".to_string(),
                detail: String::new(),
            })
            .collect(),
        runtime_instance_id: runtime_instance_id.to_string(),
        storage_schema_version,
        health: Some(pb::Health {
            state: "healthy".to_string(),
            detail: String::new(),
            lanes: Vec::new(),
        }),
    })
}

/// The runtime's NAF version (compiled-in, used by the server side).
fn handshake_runtime_version() -> &'static str {
    option_env!("CARGO_PKG_VERSION").unwrap_or("0.1.0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_compatible_version() {
        let req = client_request(
            "nai.os",
            "1.0.0",
            1234,
            1,
            1,
            &["event.append", "query.federated"],
            None,
            "abc",
            b"nonce",
        );
        let resp = server_response(&req, &["event.append", "query.federated"], "inst-1", 1).unwrap();
        assert_eq!(resp.selected_naf_version, 1);
        assert_eq!(resp.capabilities.len(), 2);
        assert_eq!(resp.capabilities[0].name, "event.append");
    }

    #[test]
    fn rejects_incompatible_range() {
        let req = client_request("nai.os", "1.0.0", 1, 5, 9, &[], None, "abc", b"nonce");
        assert!(server_response(&req, &[], "i", 1).is_err());
    }
}