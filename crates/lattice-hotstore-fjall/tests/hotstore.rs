//! Fjall HotStore roundtrip test (parity with SurrealKV adapter).

use nai_lattice_hotstore::{HotKey, HotStore, HotValue, Keyspace};

#[test]
fn fjall_hotstore_roundtrip_scan_and_counters() {
    let dir = tempfile::tempdir().unwrap();
    let store = nai_lattice_hotstore_fjall::FjallHotStore::open(
        nai_lattice_hotstore_fjall::FjallOptions::new(dir.path().join("db")),
    )
    .unwrap();

    store.put(&HotKey(b"entity/person/1".to_vec()), &HotValue(b"a".to_vec())).unwrap();
    store.put(&HotKey(b"entity/person/2".to_vec()), &HotValue(b"b".to_vec())).unwrap();
    store.put(&HotKey(b"email/message/1".to_vec()), &HotValue(b"c".to_vec())).unwrap();

    assert_eq!(
        store.get(&HotKey(b"entity/person/1".to_vec())).unwrap().unwrap().0,
        b"a".to_vec()
    );
    assert!(store.exists(&HotKey(b"entity/person/2".to_vec())).unwrap());
    assert!(!store.exists(&HotKey(b"missing".to_vec())).unwrap());

    let entity = store.scan_prefix(b"entity/").unwrap();
    assert_eq!(entity.len(), 2);

    let ks = Keyspace::new(&store);
    ks.put_json(["projection", "idx", "checkpoint"], &7u64).unwrap();
    assert_eq!(ks.get_json::<_, _, u64>(["projection", "idx", "checkpoint"]).unwrap(), Some(7));

    let ctr = HotKey(b"runtime/counter".to_vec());
    assert_eq!(store.increment(&ctr).unwrap(), 1);
    assert_eq!(store.increment(&ctr).unwrap(), 2);

    assert!(store.delete(&HotKey(b"entity/person/2".to_vec())).unwrap());
    assert!(!store.exists(&HotKey(b"entity/person/2".to_vec())).unwrap());
}