//! The crate ships a vendored copy of the canonical NAF/1 schema so a
//! crates.io build is self-contained. In the workspace, assert the copy has
//! not drifted from `/protos/naf/protocol.proto`.
use std::path::Path;

#[test]
fn vendored_proto_matches_workspace_canonical() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let canonical = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("protos").join("naf").join("protocol.proto"));
    let Some(canonical) = canonical else { return };
    if !canonical.exists() {
        return; // packaged crate: the canonical workspace copy is absent
    }
    let vendored = manifest.join("protos").join("naf").join("protocol.proto");
    let a = std::fs::read(&canonical).expect("read canonical proto");
    let b = std::fs::read(&vendored).expect("read vendored proto");
    assert_eq!(a, b, "vendored proto drifted from {}", canonical.display());
}
