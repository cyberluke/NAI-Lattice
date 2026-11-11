//! Golden Rust↔TS frame interop test (NAF-001/NAF-010).
//!
//! The golden frame below was emitted by the NAI-Lattice TypeScript client
//! (`packages/naf-client-ts`): `encodeWireFrame` on a `gastown.admin.health`
//! request envelope. The Rust canonical decoder must accept it byte-for-byte
//! and recover the exact envelope fields, proving the two implementations use
//! ONE canonical wire format sourced only from `protos/naf/protocol.proto`.
//!
//! Generated: `GOLDEN_HEX=...` from the TS client test `frame.test.mjs`.

use bytes::BytesMut;
use lattice_protocol::frame;
use lattice_protocol::pb;
use prost::Message;

const TS_GOLDEN_HEX: &str = concat!(
    "8e00000008b18c85f2041001185a325a122461616161616161612d626262622d636363632d646464642d6565656565656565656565651a086964656d2d303031",
    "220c676173746f776e2e686f73745a1a0a14676173746f776e2e61646d696e2e6865616c746812027b7d38e2fd92f90c42209e62444407e10c80e2238a6f2842c0a76d96172c6dc03723d9475af730fb42c4"
);

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn ts_client_golden_frame_decodes_on_rust_canonical_decoder() {
    let mut buf = BytesMut::from(&unhex(TS_GOLDEN_HEX)[..]);
    let mut frames = Vec::new();
    frame::decode_frames(&mut buf, &mut frames).unwrap();
    assert_eq!(frames.len(), 1, "exactly one complete golden frame");
    assert!(buf.is_empty(), "no trailing bytes after golden frame");

    let f = &frames[0];
    // The payload is the protobuf `Envelope`.
    let env = pb::Envelope::decode(&f.payload[..]).unwrap();
    assert_eq!(env.producer_id, "gastown.host");
    assert_eq!(env.idempotency_key, "idem-001");
    assert!(!env.request_id_raw.is_empty(), "request_id_raw preserved");

    let req = match env.kind {
        Some(pb::envelope::Kind::Request(r)) => r,
        _ => panic!("expected a Request oneof"),
    };
    assert_eq!(req.method, "gastown.admin.health");
    assert_eq!(req.args, b"{}");

    // Frame integrity already verified by decode_frames; also confirm the
    // canonical encoder re-emits the same bytes (round-trip stability).
    let reencoded = frame::encode_frame(1, 0, "", &f.payload[..]).unwrap();
    // length prefix is 4 bytes; compare bodies (payload identical).
    assert_eq!(&reencoded[4..], &unhex(TS_GOLDEN_HEX)[4..]);
}