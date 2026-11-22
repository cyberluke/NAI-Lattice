//! SurrealKV HotStore end-to-end tests.

use nai_lattice_hotstore::{HotStore, HotKey, HotValue, Keyspace};

#[test]
fn hotstore_roundtrip_scan_and_idempotency() {
    let dir = tempfile::tempdir().unwrap();
    let store = nai_lattice_hotstore_surrealkv::SurrealKvHotStore::open(
        nai_lattice_hotstore_surrealkv::SurrealKvOptions::new(dir.path().join("db")),
    )
    .unwrap();

    // Roundtrip.
    store
        .put(&HotKey(b"entity/person/1".to_vec()), &HotValue(b"{}\0".to_vec()))
        .unwrap();
    let got = store.get(&HotKey(b"entity/person/1".to_vec())).unwrap().unwrap();
    assert_eq!(got.0.as_slice(), b"{}\0");
    assert!(store.exists(&HotKey(b"entity/person/1".to_vec())).unwrap());
    assert!(!store.exists(&HotKey(b"missing".to_vec())).unwrap());

    // Scan prefix: put more under entity/, ensure prefix scan isolates it.
    store.put(&HotKey(b"entity/person/2".to_vec()), &HotValue(b"b".to_vec())).unwrap();
    store.put(&HotKey(b"email/message/1".to_vec()), &HotValue(b"c".to_vec())).unwrap();
    let entity = store.scan_prefix(b"entity/").unwrap();
    assert_eq!(entity.len(), 2);

    // Keyspace typed JSON helpers + counter increment.
    let ks = Keyspace::new(&store);
    ks.put_json(["projection", "idx", "checkpoint"], &42u64).unwrap();
    let v: Option<u64> = ks.get_json(["projection", "idx", "checkpoint"]).unwrap();
    assert_eq!(v, Some(42));

    let ctr = HotKey(b"runtime/counter".to_vec());
    assert_eq!(store.increment(&ctr).unwrap(), 1);
    assert_eq!(store.increment(&ctr).unwrap(), 2);

    // Delete.
    assert!(store.delete(&HotKey(b"entity/person/2".to_vec())).unwrap());
    assert!(!store.exists(&HotKey(b"entity/person/2".to_vec())).unwrap());
}