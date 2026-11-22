//! Live-pipe integration probe: connects to the running `nai-fabric-host.exe`
//! singleton via the Rust `nai_lattice_client::win_pipe` + `NafClient` and performs
//! a NAF handshake. This isolates whether the host is serving the pipe (vs a
//! TS-only client issue). Requires the singleton to be running.
//! Run: `cargo test -p nai-fabric-host --test live_pipe -- --ignored --nocapture`

use nai_lattice_client::NafClient;
use nai_lattice_client::win_pipe::NamedPipeStream;
use nai_lattice_protocol::pb;

#[tokio::test]
#[ignore = "requires the running singleton"]
async fn rust_client_reaches_live_singleton() {
    // Match the host's pipe resolution.
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
    let h = blake3::hash(user.as_bytes()).to_hex().to_string();
    let stream = NamedPipeStream::connect(&format!(r"\\.\pipe\NAI.AmbientFabric.NAF1.{}", &h[..6]))
        .expect("connect to running singleton pipe");

    let req = pb::HandshakeRequest {
        client_app_id: "rust-probe".into(),
        client_version: "0.1.0".into(),
        client_pid: std::process::id() as i64,
        naf_min: 1,
        naf_max: 1,
        requested_capabilities: vec!["event.append".into(), "gastown.admin.health".into()],
        package_identity: "rust-probe".into(),
        build_sha: "live".into(),
        nonce: vec![1],
    };
    let client = NafClient::connect(stream, req).await;
    match client {
        Ok(c) => {
            eprintln!("RUST_CLIENT_CONNECTED instance={} naf={}", c.runtime_instance_id, c.naf_version);
        }
        Err(e) => {
            eprintln!("RUST_CLIENT_FAILED: {e}");
            panic!("host not reachable via Rust client: {e}");
        }
    }
    let _ = &std::process::id();
}