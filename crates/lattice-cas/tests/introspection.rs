//! CAS end-to-end tests.

use nai_lattice_core::{StabilityClass, VerificationState};

use nai_lattice_cas::object::ObjectStore;
use nai_lattice_cas::manifest::ManifestStore;

#[test]
fn put_verify_open_range_and_external_location() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("cas");
    let store = ObjectStore::new(root.clone()).unwrap();

    let data = b"hello lattice artifact";
    let hash = store.put(data, "text/plain", "test-blob", "unit-test").unwrap();
    let hash_hex = hash.as_hex().to_string();

    // Whole-object hash is the canonical identity.
    assert!(hash_hex.len() == 64);
    assert!(store.verify(&hash_hex).unwrap());

    // Full read matches.
    let opened = store.open(&hash_hex).unwrap();
    assert_eq!(opened.size, data.len() as u64);
    let full = std::fs::read(&opened.owned_path).unwrap();
    assert_eq!(full.as_slice(), data);

    // Range read.
    let r = store.open_range(&hash_hex, 6, 13).unwrap();
    assert_eq!(&r.data[..], &data[6..13]);

    // External AAF location appends without mutating owned bytes.
    let mstore = store.manifest_store();
    mstore.add_external_location(
        &hash_hex,
        "naibrowser-opfs",
        "profile-0/blob.bin",
        data.len() as u64,
        StabilityClass::StableLocal,
        false,
        3,
    )
    .unwrap();
    mstore.mark_verified(&hash_hex, VerificationState::Verified).unwrap();

    let manifest = ManifestStore::new(root.join("manifests")).read(&hash_hex).unwrap().unwrap();
    assert_eq!(manifest.external_locations.len(), 1);
    assert_eq!(manifest.external_locations[0].provider_id, "naibrowser-opfs");
    assert_eq!(manifest.verification_state, VerificationState::Verified);
}