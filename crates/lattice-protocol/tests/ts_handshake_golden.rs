//! Rust↔TS handshake envelope interop (NAF-002/NAF-010).
//!
//! This golden handshake wire was emitted by the TS client
//! (`packages/naf-client-ts` `hsgen`). The Rust canonical decoder + prost must
//! parse it into a valid request envelope + `HandshakeRequest`, proving the TS
//! client's handshake is byte-compatible with the native host.

use bytes::BytesMut;
use lattice_protocol::frame;
use lattice_protocol::pb;
use prost::Message;

const TS_HS_HEX: &str =
    "e300000008b18c85f204100118ad0132ad01122461616161616161612d626262622d636363632d646464642d656565656565656565656565220a736d6f6b652e746573744a0d6e61662e68616e6473\
     68616b655a6a0a0b5f5f68616e647368616b65125b0a046b696c6f1205302e312e3018d20920012801320c6576656e742e617070656e643214676173746f776e2e61646d696e2e6865616c74683a\
     19406e61692d6c6174746963652f636c69656e7440302e312e304201784a0301020338bd9896e90242201e7470e7f7fc63dec48a6234107839b6beea2e0837162065d2800c9e814b11f7";

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn ts_handshake_envelope_decodes_via_canonical_decoder() {
    let mut buf = BytesMut::from(&unhex(TS_HS_HEX)[..]);
    let mut frames = Vec::new();
    frame::decode_frames(&mut buf, &mut frames).unwrap();
    assert_eq!(frames.len(), 1);
    assert!(buf.is_empty());

    let env = pb::Envelope::decode(&frames[0].payload[..]).unwrap();
    assert_eq!(env.producer_id, "smoke.test");
    assert_eq!(env.payload_type, "naf.handshake");

    let req = match env.kind {
        Some(pb::envelope::Kind::Request(r)) => r,
        _ => panic!("expected Request oneof"),
    };
    assert_eq!(req.method, "__handshake");

    let hs = pb::HandshakeRequest::decode(req.args.as_slice())
        .expect("HandshakeRequest must decode from TS args");
    assert_eq!(hs.client_app_id, "kilo");
    assert_eq!(hs.naf_min, 1);
    assert_eq!(hs.naf_max, 1);
    assert_eq!(hs.requested_capabilities, vec!["event.append", "gastown.admin.health"]);
    assert_eq!(hs.package_identity, "@nai-lattice/client@0.1.0");
    assert_eq!(hs.nonce, [1, 2, 3]);
}