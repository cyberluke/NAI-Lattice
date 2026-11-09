//! Runtime integration test: full NAF dispatch through ledger + CAS + hotstore.

use lattice_runtime::{LatticeRuntime, RuntimeConfig};

#[test]
fn runtime_appends_event_and_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = RuntimeConfig {
        root: dir.path().to_path_buf(),
        ..Default::default()
    };
    let rt = LatticeRuntime::open(cfg.clone()).unwrap();

    // capabilities.list
    let caps: Vec<String> = serde_json::from_slice(
        &rt.dispatch("capabilities.list", b"{}", "test", None).unwrap(),
    )
    .unwrap();
    assert!(caps.contains(&"event.append".to_string()));

    // event.append
    let event_args = serde_json::json!({
        "event_type": "email.message.ingested",
        "namespace": "nai.files",
        "payload": {"subject": "hi"},
        "entity_id": "entity://email/m1",
    });
    let resp: serde_json::Value = serde_json::from_slice(
        &rt.dispatch("event.append", event_args.to_string().as_bytes(), "test", Some("idem-1"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(resp["duplicated"], false);
    assert!(resp["event_id"].as_str().unwrap().len() > 0);

    // Same idempotency key must not duplicate.
    let resp2: serde_json::Value = serde_json::from_slice(
        &rt.dispatch("event.append", event_args.to_string().as_bytes(), "test", Some("idem-1"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(resp2["duplicated"], true);
    assert_eq!(resp["event_id"], resp2["event_id"]);

    // artifact.put then open (roundtrip).
    use base64::Engine;
    let payload = b"my-large-artifact".to_vec();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&payload);
    let put_args = serde_json::json!({ "data": b64, "media_type": "application/octet-stream", "logical_type": "onnx-model" });
    let put: serde_json::Value = serde_json::from_slice(
        &rt.dispatch("artifact.put", put_args.to_string().as_bytes(), "test", None).unwrap(),
    )
    .unwrap();
    let uri = put["artifact_uri"].as_str().unwrap().to_string();
    assert!(uri.starts_with("artifact://blake3/"));

    let open_args = serde_json::json!({ "artifact_uri": uri });
    let open: serde_json::Value = serde_json::from_slice(
        &rt.dispatch("artifact.open", open_args.to_string().as_bytes(), "test", None).unwrap(),
    )
    .unwrap();
    let roundtrip =
        base64::engine::general_purpose::STANDARD.decode(open["data"].as_str().unwrap()).unwrap();
    assert_eq!(roundtrip, payload);

    // runtime.health reflects ledger high-water mark.
    let h: serde_json::Value = serde_json::from_slice(
        &rt.dispatch("runtime.health", b"{}", "test", None).unwrap(),
    )
    .unwrap();
    assert_eq!(h["state"], "healthy");
    assert!(h["ledger_high_water_mark"].as_u64().unwrap() >= 1);

    // BatchedDurable: append then flush, reopen -> frame persisted.
    let batched: serde_json::Value = serde_json::from_slice(
        &rt.dispatch(
            "event.append",
            serde_json::json!({
                "event_type": "test.batched",
                "durability": "batched",
                "payload": {"x": 1},
            })
            .to_string()
            .as_bytes(),
            "test",
            Some("batched-idem"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(batched["duplicated"], false);
    rt.flush_hotstore();
    // Release the SurrealKV file lock before reopening.
    drop(rt);

    // Reopen the runtime on the same root -> crash recovery replays all frames.
    let rt2 = LatticeRuntime::open(cfg.clone()).unwrap();
    let h2: serde_json::Value = serde_json::from_slice(
        &rt2.dispatch("runtime.health", b"{}", "test", None).unwrap(),
    )
    .unwrap();
    // Two committed events persist: the ProjectionOnly append (seq 1) and the
    // BatchedDurable append (seq 2). Reopen replays both -> high-water >= 2.
    assert!(h2["ledger_high_water_mark"].as_u64().unwrap() >= 2);

    // Semantic + fuzzy engines: index two docs (vector + text), then query.
    // Note: the runtime opened `vector` at dimension 1024; we use a fresh
    // in-memory dimension here to avoid dim coupling in this unit test.
    let vpred = vec![0.0f32; 1024];
    let mut a = vpred.clone();
    a[0] = 1.0;
    let mut b = vpred.clone();
    b[1] = 1.0;
    rt2.dispatch(
        "semantic.index",
        serde_json::json!({
            "id": "app/calc", "vector": a.to_vec(), "title": "Calculator", "body": "arithmetic and math", "namespace": "apps"
        }).to_string().as_bytes(),
        "test", None,
    ).unwrap();
    rt2.dispatch(
        "semantic.index",
        serde_json::json!({
            "id": "app/editor", "vector": b.to_vec(), "title": "Code Editor", "body": "edit source", "namespace": "apps"
        }).to_string().as_bytes(),
        "test", None,
    ).unwrap();

    // query.vector near 'a' returns app/calc first.
    let mut q = vec![0.0f32; 1024];
    q[0] = 0.99; q[1] = 0.01;
    let vq: serde_json::Value = serde_json::from_slice(
        &rt2.dispatch("query.vector", serde_json::json!({"vector": q, "limit": 2}).to_string().as_bytes(), "test", None).unwrap(),
    ).unwrap();
    assert_eq!(vq["hits"][0]["id"], "app/calc");

    // query.search fuzzy finds the editor via a misspelled term.
    let sq: serde_json::Value = serde_json::from_slice(
        &rt2.dispatch("query.search", serde_json::json!({"query": "edtor", "fuzzy": true, "limit": 2}).to_string().as_bytes(), "test", None).unwrap(),
    ).unwrap();
    assert_eq!(sq["hits"][0]["id"], "app/editor");

    // query.federated fuses text + fuzzy.
    let fq: serde_json::Value = serde_json::from_slice(
        &rt2.dispatch("query.federated", serde_json::json!({"query": "arithmetic", "limit": 3}).to_string().as_bytes(), "test", None).unwrap(),
    ).unwrap();
    assert_eq!(fq["fused"][0]["id"], "app/calc");
}