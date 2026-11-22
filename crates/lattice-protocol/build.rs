//! Compile the NAF/1 protobuf schemas into Rust via prost-build.
//!
//! Uses protoc-bin-vendored (prebuilt protoc shipped for Windows/Linux/macOS)
//! so a system protoc or C toolchain is not required on the build machine.
//! Generated bindings land in the default `OUT_DIR` (never committed).

fn main() -> std::io::Result<()> {
    std::env::set_var(
        "PROTOC",
        protoc_bin_vendored::protoc_bin_path()
            .expect("vendored protoc binary")
            .as_os_str(),
    );

    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // The workspace keeps the canonical schema at /protos/naf/protocol.proto.
    // A crates.io package only contains files under the crate directory, so the
    // same schema is vendored at crates/lattice-protocol/protos/naf/protocol.proto
    // and used as the fallback when the workspace root is absent.
    let workspace_proto = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("protos").join("naf"));
    let packaged_proto = manifest.join("protos").join("naf");
    let proto_root = match workspace_proto {
        Some(root) if root.join("protocol.proto").exists() => root,
        _ => packaged_proto,
    };
    let proto_file = proto_root.join("protocol.proto");
    if !proto_file.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("proto not found: {}", proto_file.display()),
        ));
    }

    println!("cargo:rerun-if-changed={}", proto_file.display());

    prost_build::Config::new()
        .btree_map(["."])
        .compile_protos(&[proto_file], &[proto_root])?;

    Ok(())
}