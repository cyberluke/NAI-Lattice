//! NAI Ambient Fabric host — per-user NAF/1 server over a Windows named pipe
//! (Lattice spec §7).
//!
//! Lifecycle:
//!  1. open the runtime (ledger + hotstore + CAS + AAF);
//!  2. create/own the per-user pipe `\\.\pipe\NAI.AmbientFabric.NAF1.<sid-hash>`;
//!  3. serve connections: handshake, then dispatch NAF methods via the runtime.
//!
//! The runtime dispatch is synchronous, so each connection is handled on its
//! own thread with no Tokio coordinator — deterministic append order and the
//! internal LSM/hot-store pipeline provide the required concurrency.

use std::io::{Read, Write};
use std::sync::Arc;

use bytes::BytesMut;
use prost::Message;

use lattice_protocol::pb;
use lattice_runtime::{LatticeRuntime, RuntimeConfig};

mod naf_server;

fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let config = RuntimeConfig::default();
    let rt = match LatticeRuntime::open(config) {
        Ok(rt) => Arc::new(rt),
        Err(e) => {
            eprintln!("fatal: failed to open Lattice runtime: {e}");
            std::process::exit(2);
        }
    };
    tracing::info!(instance = %rt.instance_id, "NAI fabric host ready");

    let pipe = naf_server::ServerPipe::pipe_name();
    naf_server::serve(rt, &pipe);
}

/// Shared NAF/1 handling used by every connection.
pub(crate) fn handle_connection<S: Read + Write>(rt: &Arc<LatticeRuntime>, stream: &mut S) {
    let mut buf = BytesMut::new();
    // 1. Handshake.
    match naf_server::read_naf_frame(stream, &mut buf) {
        Ok(frame) => {
            let env: pb::Envelope = match prost::Message::decode(&frame.payload[..]) {
                Ok(e) => e,
                Err(_) => return,
            };
            let hs_req = match env.kind {
                Some(pb::envelope::Kind::Request(r)) if r.method == "__handshake" => {
                    match pb::HandshakeRequest::decode(r.args.as_slice()) {
                        Ok(h) => h,
                        Err(_) => return,
                    }
                }
                _ => return,
            };
            let resp = build_handshake_response(rt, &hs_req);
            let resp_env = pb::Envelope {
                kind: Some(pb::envelope::Kind::Response(pb::Response {
                    status: 0,
                    error: String::new(),
                    body: resp.encode_to_vec(),
                })),
                ..Default::default()
            };
            if let Ok(bytes) = naf_server::encode_naf(&resp_env) {
                let _ = stream.write_all(&bytes);
                let _ = stream.flush();
            }
        }
        Err(_) => return,
    }

    // 2. Dispatch loop.
    loop {
        let frame = match naf_server::read_naf_frame(stream, &mut buf) {
            Ok(f) => f,
            Err(_) => break,
        };
        let env: pb::Envelope = match prost::Message::decode(&frame.payload[..]) {
            Ok(e) => e,
            Err(_) => break,
        };
        let (method, args, producer, idem) = match env.kind {
            Some(pb::envelope::Kind::Request(r)) => {
                (r.method, r.args, env.producer_id, Option::<String>::None)
            }
            _ => break,
        };
        let _ = producer;
        let result = rt.dispatch(&method, &args, "naf", idem.as_deref());
        let (status, body, error) = match result {
            Ok(body) => (0, body, String::new()),
            Err(e) => (500, Vec::new(), e.to_string()),
        };
        let resp_env = pb::Envelope {
            kind: Some(pb::envelope::Kind::Response(pb::Response {
                status,
                error,
                body,
            })),
            ..Default::default()
        };
        if let Ok(bytes) = naf_server::encode_naf(&resp_env) {
            if stream.write_all(&bytes).is_err() || stream.flush().is_err() {
                break;
            }
        }
    }
}

fn build_handshake_response(rt: &Arc<LatticeRuntime>, req: &pb::HandshakeRequest) -> pb::HandshakeResponse {
    let selected = lattice_protocol::select_version(
        req.naf_min,
        req.naf_max,
        lattice_protocol::NAF_VERSION_1,
        lattice_protocol::NAF_VERSION_1,
    )
    .unwrap_or(lattice_protocol::NAF_VERSION_1);
    pb::HandshakeResponse {
        runtime_version: "0.1.0".to_string(),
        runtime_build_sha: "dev".to_string(),
        selected_naf_version: selected,
        capabilities: rt
            .capabilities()
            .iter()
            .map(|c| pb::Capability {
                name: c.to_string(),
                version: "1".to_string(),
                detail: String::new(),
            })
            .collect(),
        runtime_instance_id: rt.instance_id.clone(),
        storage_schema_version: 1,
        health: Some(pb::Health {
            state: "healthy".to_string(),
            detail: String::new(),
            lanes: Vec::new(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use prost::Message;

    /// Minimal synchronous duplex: reads from `incoming`, writes to `outgoing`.
    struct TestDuplex {
        incoming: VecDeque<u8>,
        outgoing: Vec<u8>,
    }
    impl Read for TestDuplex {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.incoming.len().min(buf.len());
            for b in buf.iter_mut().take(n) {
                *b = self.incoming.pop_front().unwrap();
            }
            Ok(n)
        }
    }
    impl Write for TestDuplex {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.outgoing.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn host_dispatch_handshake_and_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Arc::new(
            LatticeRuntime::open(RuntimeConfig {
                root: dir.path().to_path_buf(),
                ledger_segment_bytes: 64 * 1024 * 1024,
                ..Default::default()
            })
            .unwrap(),
        );

        // Handshake frame.
        let hs = pb::HandshakeRequest {
            client_app_id: "test".to_string(),
            client_version: "1".to_string(),
            client_pid: 99,
            naf_min: 1,
            naf_max: 1,
            requested_capabilities: vec!["event.append".to_string()],
            package_identity: String::new(),
            build_sha: "abc".to_string(),
            nonce: vec![1],
        };
        let hs_payload = hs.encode_to_vec();
        let hs_env = pb::Envelope {
            producer_id: "test".to_string(),
            payload_type: "naf.handshake.request".to_string(),
            kind: Some(pb::envelope::Kind::Request(pb::Request {
                method: "__handshake".to_string(),
                args: hs_payload,
            })),
            ..Default::default()
        };
        let mut stream = TestDuplex {
            incoming: naf_server::encode_naf(&hs_env).unwrap().into(),
            outgoing: Vec::new(),
        };

        handle_connection(&rt, &mut stream);

        assert!(stream.outgoing.len() > 0, "no handshake response");
        let resp = naf_server::read_naf_frame(
            &mut std::io::Cursor::new(stream.outgoing.clone()),
            &mut bytes::BytesMut::new(),
        )
        .unwrap();
        let env: pb::Envelope = pb::Envelope::decode(resp.payload.as_ref()).unwrap();
        let hs_resp = match env.kind {
            Some(pb::envelope::Kind::Response(r)) => {
                pb::HandshakeResponse::decode(r.body.as_slice()).unwrap()
            }
            _ => panic!("expected handshake response"),
        };
        assert_eq!(hs_resp.selected_naf_version, 1);
        assert!(hs_resp.capabilities.iter().any(|c| c.name == "event.append"));
    }
}