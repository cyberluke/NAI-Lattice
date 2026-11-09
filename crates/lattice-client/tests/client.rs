//! Client codec + dispatch tests over an in-memory duplex transport.

use bytes::BytesMut;
use prost::Message;

use lattice_client::codec::{encode_frame, read_frame, WireFrame};
use lattice_client::NafClient;
use lattice_protocol::pb;

#[tokio::test]
async fn client_handshakes_and_dispatches_over_duplex() {
    // A single duplex pair: `client_end` (used by the client) and `server_end`.
    let (client_end, server_end) = lattice_client::transport::duplex();
    let mut server = server_end;

    // Fake server: read handshake, reply; read dispatch, reply.
    let srv = tokio::spawn(async move {
        let mut buf = BytesMut::new();
        // 1. Handshake request.
        let fr: WireFrame = read_frame(&mut server, &mut buf).await.unwrap();
        let env: pb::Envelope = prost::Message::decode(fr.payload.as_ref()).unwrap();
        let hs_req = match env.kind {
            Some(pb::envelope::Kind::Request(r)) => {
                pb::HandshakeRequest::decode(r.args.as_slice()).unwrap()
            }
            _ => panic!("expected handshake request"),
        };
        assert_eq!(hs_req.client_app_id, "test-client");
        // Reply with a handshake response.
        let resp = pb::HandshakeResponse {
            runtime_version: "1.0.0".into(),
            runtime_build_sha: "abc".into(),
            selected_naf_version: 1,
            capabilities: vec![pb::Capability {
                name: "event.append".into(),
                version: "1".into(),
                detail: String::new(),
            }],
            runtime_instance_id: "inst-42".into(),
            storage_schema_version: 1,
            health: Some(pb::Health {
                state: "healthy".into(),
                detail: String::new(),
                lanes: Vec::new(),
            }),
        };
        let resp_env = pb::Envelope {
            kind: Some(pb::envelope::Kind::Response(pb::Response {
                status: 0,
                error: String::new(),
                body: resp.encode_to_vec(),
            })),
            ..Default::default()
        };
        tokio::io::AsyncWriteExt::write_all(&mut server, &encode_frame(&resp_env).unwrap())
            .await
            .unwrap();

        // 2. Dispatch request, echo the body back.
        let fr: WireFrame = read_frame(&mut server, &mut buf).await.unwrap();
        let env: pb::Envelope = prost::Message::decode(fr.payload.as_ref()).unwrap();
        let req = match env.kind {
            Some(pb::envelope::Kind::Request(r)) => r,
            _ => panic!("expected request"),
        };
        assert_eq!(req.method, "event.append");
        let echo_env = pb::Envelope {
            kind: Some(pb::envelope::Kind::Response(pb::Response {
                status: 0,
                error: String::new(),
                body: b"{\"ok\":true}".to_vec(),
            })),
            ..Default::default()
        };
        tokio::io::AsyncWriteExt::write_all(&mut server, &encode_frame(&echo_env).unwrap())
            .await
            .unwrap();
    });

    // Client side over `client_end`.
    let hs_req = pb::HandshakeRequest {
        client_app_id: "test-client".into(),
        client_version: "1".into(),
        client_pid: 1234,
        naf_min: 1,
        naf_max: 1,
        requested_capabilities: vec!["event.append".into()],
        package_identity: String::new(),
        build_sha: "abc".into(),
        nonce: vec![1, 2, 3],
    };
    let mut client = NafClient::connect(client_end, hs_req).await.unwrap();
    assert_eq!(client.runtime_instance_id, "inst-42");
    assert_eq!(client.naf_version, 1);

    let body = client.dispatch("event.append", b"{}", Some("idem-1")).await.unwrap();
    assert_eq!(body.as_slice(), b"{\"ok\":true}");

    srv.await.unwrap();
}