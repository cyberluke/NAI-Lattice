//! AAF resolver tests: preference ordering + read-only external reuse.

use std::sync::Arc;

use nai_lattice_aaf::source::MemorySource;
use nai_lattice_aaf::{AafResolver, DiscoverFilter, Locator, Resolution};
use nai_lattice_cas::object::ObjectStore;

#[test]
fn resolves_owned_cas_before_external() {
    let dir = tempfile::tempdir().unwrap();
    let cas = Arc::new(ObjectStore::new(dir.path().join("cas")).unwrap());
    let resolver = AafResolver::new(cas.clone());

    let payload = b"same-model-bytes";
    let hash = cas.put(payload, "application/octet-stream", "onnx-model", "test").unwrap();
    let hash_hex = hash.as_hex().to_string();

    // Even with an external provider, owned CAS wins the preference.
    let mem = Arc::new(MemorySource {
        id: "external".into(),
        objects: std::collections::HashMap::new(),
    });
    resolver.register(mem).unwrap();

    let res = resolver.resolve(&hash_hex, &DiscoverFilter::default()).unwrap();
    assert!(matches!(res, Resolution::Owned { .. }));
}

#[test]
fn reuses_external_fingerprint_matching_source() {
    let dir = tempfile::tempdir().unwrap();
    let cas = Arc::new(ObjectStore::new(dir.path().join("cas")).unwrap());
    let resolver = AafResolver::new(cas.clone());

    let payload = b"model-weights";
    let expected = blake3::hash(payload).to_hex().to_string();
    resolver.register(Arc::new(MemorySource {
        id: "browser-cache".into(),
        objects: std::collections::HashMap::from([
            ("model.bin".to_string(), payload.to_vec()),
        ]),
    }))
    .unwrap();

    // Not in owned CAS -> resolve from external provider by hash match.
    let res = resolver.resolve(&expected, &DiscoverFilter::default()).unwrap();
    match res {
        Resolution::External { provider_id, locator } => {
            assert_eq!(provider_id, "browser-cache");
            assert_eq!(locator, Locator("model.bin".into()));
            // Read range back out read-only.
            let mut stream = resolver
                .open_range("browser-cache", &locator, 0..payload.len() as u64)
                .unwrap();
            use std::io::Read;
            let mut got = Vec::new();
            stream.read_to_end(&mut got).unwrap();
            assert_eq!(got, payload);
        }
        other => panic!("expected external resolution, got {other:?}"),
    }
}

#[test]
fn no_location_when_unmatched() {
    let dir = tempfile::tempdir().unwrap();
    let cas = Arc::new(ObjectStore::new(dir.path().join("cas")).unwrap());
    let resolver = AafResolver::new(cas.clone());
    let res = resolver.resolve("deadbeef".repeat(8).as_str(), &DiscoverFilter::default());
    assert!(matches!(res, Err(nai_lattice_aaf::AafError::NoLocation(_))));
}