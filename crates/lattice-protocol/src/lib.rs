//! NAF/1 — NAI Ambient Fabric Protocol v1.
//!
//! Language-neutral protobuf messages + Rust framing/helpers (Lattice spec §8).
//! Products depend on these capability contracts, never engine implementations.

pub mod protocol {
    //! Generated protobuf bindings (from `/protos/naf/protocol.proto`).
    include!(concat!(env!("OUT_DIR"), "/naf.v1.rs"));
}
pub use protocol as pb;

pub mod frame;
pub mod handshake;

use thiserror::Error;

/// NAF major ABI version negotiated on handshake (spec §8, §naf-versioning).
pub const NAF_VERSION_1: u32 = 1;
/// Frame magic "NAF1" as u32 little-endian.
pub const FRAME_MAGIC: u32 = 0x4e41_4631; // b"NAF1"

/// Minimum set of capabilities a NAF/1 runtime must advertise (spec §8).
pub const MINIMUM_CAPABILITIES: &[&str] = &[
    "runtime.health",
    "runtime.describe",
    "runtime.shutdown_if_idle",
    "capabilities.list",
    "event.append",
    "event.append_batch",
    "event.subscribe",
    "event.read_range",
    "artifact.register_external",
    "artifact.put",
    "artifact.open",
    "artifact.open_range",
    "artifact.resolve",
    "artifact.locations",
    "artifact.verify",
    "artifact.release",
    "entity.get",
    "entity.resolve",
    "entity.upsert_alias",
    "query.sql",
    "query.federated",
    "query.search",
    "query.vector",
    "query.graph",
    "projection.list",
    "projection.status",
    "projection.rebuild",
    "projection.verify",
    "provider.list",
    "provider.rescan",
    "provider.health",
];

/// A frame/parse error.
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("bad frame magic: got {got:#010x}, expected {expected:#010x}")]
    BadMagic { got: u32, expected: u32 },
    #[error("unsupported format version {0}")]
    UnsupportedFormatVersion(u32),
    #[error("frame length {actual} exceeds maximum {max}")]
    FrameTooLarge { actual: u64, max: u64 },
    #[error("crc32c mismatch")]
    CrcMismatch,
    #[error("blake3 mismatch on frame")]
    HashMismatch,
    #[error("incomplete frame: {0}")]
    Incomplete(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("prost decode: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("prost encode: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("protocol negotiation failed: {0}")]
    Negotiation(String),
}

/// Return true when `version` is within the `[min, max]` negotiated range.
pub fn version_in_range(version: u32, min: u32, max: u32) -> bool {
    version >= min && version <= max
}

/// Select the highest mutually supported NAF version given client and runtime ranges.
///
/// Returns `None` when no compatible version exists.
pub fn select_version(client_min: u32, client_max: u32, runtime_min: u32, runtime_max: u32) -> Option<u32> {
    let lo = client_min.max(runtime_min);
    let hi = client_max.min(runtime_max);
    (lo <= hi).then_some(hi)
}