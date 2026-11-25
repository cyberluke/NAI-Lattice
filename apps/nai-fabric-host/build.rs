//! Embeds the compiled `packages/lattice-dashboard` bundle into the host binary.
//!
//! The dashboard is a Vue 3 + Dockview + FDC3 app built by Vite. Its `dist/`
//! directory ships a `bundle-manifest.json` listing every file with a BLAKE3
//! digest. This script:
//!
//! 1. reads that manifest (a missing/malformed manifest simply means "no
//!    bundle" — the host then keeps serving the legacy dashboard);
//! 2. verifies that every listed path stays INSIDE `dist` (no absolute path,
//!    no drive/stream separator, no UNC/backslash form, no `..`, and the
//!    canonicalized file must start with the canonicalized `dist`);
//! 3. verifies each listed file's size and BLAKE3 digest against the manifest,
//!    so a stale or partially written `dist/` can never be embedded;
//! 4. emits a Rust table of `include_bytes!` entries (one binary, no runtime
//!    filesystem dependency, no Node at runtime).
//!
//! A missing, unreadable, malformed or hostile manifest is not a build failure:
//! it degrades to "no bundle" with a `cargo:warning` (documented rollback path).
//! Failures of the size/digest integrity checks ARE hard build errors: silently
//! serving a corrupted dashboard is worse than no dashboard.
//!
//! The pure helpers below are also compiled into the host crate's test harness
//! (see `lattice_assets.rs`), so `cargo test -p nai-fabric-host` exercises the
//! containment and malformed-manifest rules directly.

use std::path::{Component, Path, PathBuf};

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn rust_str(s: &str) -> String {
    format!("{s:?}")
}

/// A verified manifest entry ready for code generation.
#[derive(Debug)]
struct BundleEntry {
    path: String,
    full: PathBuf,
    content_type: &'static str,
}

/// A fully verified bundle.
#[derive(Debug)]
struct BundleManifest {
    version: String,
    entries: Vec<BundleEntry>,
}

/// Outcome of reading `dist/bundle-manifest.json`.
#[derive(Debug)]
enum BundleLoad {
    /// The manifest and every listed file verified.
    Bundle(BundleManifest),
    /// No usable bundle. The reason is emitted as a `cargo:warning`.
    Absent(String),
}

/// Why a manifest path was not accepted.
#[derive(Debug)]
enum PathError {
    /// The manifest itself is invalid/hostile: fall back to "no bundle".
    Rejected(String),
    /// A contained path could not be read/canonicalized: real broken `dist/`.
    Unreadable(String),
}

/// Reject any manifest path that could escape `dist`.
///
/// Rejected outright: absolute paths, `:` (drive or NTFS stream separator),
/// `\\` (UNC / Windows separator), root/prefix/`..` components, and anything
/// whose canonicalized file does not start with the canonicalized `dist`.
fn validate_manifest_path(
    dist: &Path,
    dist_canonical: &Path,
    path: &str,
) -> Result<PathBuf, PathError> {
    if path.is_empty() {
        return Err(PathError::Rejected("empty file path".to_string()));
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(PathError::Rejected(format!(
            "absolute path is not allowed: {path}"
        )));
    }
    if path.contains(':') {
        return Err(PathError::Rejected(format!(
            "drive or stream separator ':' is not allowed: {path}"
        )));
    }
    if path.contains('\\') {
        return Err(PathError::Rejected(format!(
            "backslash / UNC separator is not allowed: {path}"
        )));
    }
    if candidate.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(PathError::Rejected(format!(
            "path must not contain '..', root or prefix components: {path}"
        )));
    }
    let full = dist.join(candidate);
    let canonical = std::fs::canonicalize(&full).map_err(|error| {
        PathError::Unreadable(format!(
            "bundle file {path} listed in manifest is unreadable: {error}"
        ))
    })?;
    if !canonical.starts_with(dist_canonical) {
        return Err(PathError::Rejected(format!(
            "bundle manifest path escapes dist: {path}"
        )));
    }
    Ok(full)
}

/// Read and verify `dist/bundle-manifest.json`.
///
/// Returns `Err` only for a genuine integrity failure (size/digest mismatch or
/// an unreadable contained file). Everything that makes the manifest unusable
/// (missing, unreadable, malformed JSON, unsupported schema, missing fields,
/// unsafe paths) degrades to [`BundleLoad::Absent`].
fn load_bundle(dist: &Path) -> Result<BundleLoad, String> {
    let manifest_path = dist.join("bundle-manifest.json");
    let index_path = dist.join("index.html");
    if !manifest_path.is_file() || !index_path.is_file() {
        return Ok(BundleLoad::Absent(format!(
            "dashboard bundle not found at {} — host keeps the legacy /lattice dashboard",
            dist.display()
        )));
    }
    let raw = match std::fs::read_to_string(&manifest_path) {
        Ok(raw) => raw,
        Err(error) => {
            return Ok(BundleLoad::Absent(format!(
                "dashboard bundle manifest at {} is unreadable ({error}) — host keeps the legacy /lattice dashboard",
                manifest_path.display()
            )))
        }
    };
    let manifest: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            return Ok(BundleLoad::Absent(format!(
                "dashboard bundle manifest at {} is malformed ({error}) — host keeps the legacy /lattice dashboard",
                manifest_path.display()
            )))
        }
    };
    let schema = manifest
        .get("schema")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    if schema != 1 {
        return Ok(BundleLoad::Absent(format!(
            "dashboard bundle manifest at {} has unsupported schema {schema} (expected 1) — host keeps the legacy /lattice dashboard",
            manifest_path.display()
        )));
    }
    let Some(files) = manifest.get("files").and_then(|value| value.as_array()) else {
        return Ok(BundleLoad::Absent(format!(
            "dashboard bundle manifest at {} must list a files array — host keeps the legacy /lattice dashboard",
            manifest_path.display()
        )));
    };
    let version = manifest
        .get("version")
        .and_then(|value| value.as_str())
        .unwrap_or("0.0.0")
        .to_string();
    let dist_canonical = std::fs::canonicalize(dist).map_err(|error| {
        format!(
            "dashboard dist {} could not be canonicalized: {error}",
            dist.display()
        )
    })?;

    let mut entries = Vec::with_capacity(files.len());
    for file in files {
        let Some(path) = file.get("path").and_then(|value| value.as_str()) else {
            return Ok(BundleLoad::Absent(format!(
                "dashboard bundle manifest at {} has a file entry without a path — host keeps the legacy /lattice dashboard",
                manifest_path.display()
            )));
        };
        let full = match validate_manifest_path(dist, &dist_canonical, path) {
            Ok(full) => full,
            Err(PathError::Rejected(reason)) => {
                return Ok(BundleLoad::Absent(format!(
                    "dashboard bundle manifest at {} is malformed ({reason}) — host keeps the legacy /lattice dashboard",
                    manifest_path.display()
                )))
            }
            Err(PathError::Unreadable(reason)) => return Err(reason),
        };
        let bytes = std::fs::read(&full).map_err(|error| {
            format!("bundle file {path} listed in manifest is unreadable: {error}")
        })?;
        let Some(want_bytes) = file.get("bytes").and_then(|value| value.as_u64()) else {
            return Ok(BundleLoad::Absent(format!(
                "dashboard bundle manifest at {} has no bytes for {path} — host keeps the legacy /lattice dashboard",
                manifest_path.display()
            )));
        };
        if want_bytes != bytes.len() as u64 {
            return Err(format!(
                "bundle file {path} size does not match the manifest (stale dist?)"
            ));
        }
        let Some(want_hash) = file.get("blake3").and_then(|value| value.as_str()) else {
            return Ok(BundleLoad::Absent(format!(
                "dashboard bundle manifest at {} has no blake3 digest for {path} — host keeps the legacy /lattice dashboard",
                manifest_path.display()
            )));
        };
        let actual = blake3::hash(&bytes).to_hex().to_string();
        if want_hash != actual {
            return Err(format!(
                "bundle file {path} digest mismatch (stale or modified dist)"
            ));
        }
        entries.push(BundleEntry {
            path: path.to_string(),
            full,
            content_type: content_type(path),
        });
    }
    if !entries.iter().any(|entry| entry.path == "index.html") {
        return Ok(BundleLoad::Absent(format!(
            "dashboard bundle manifest at {} must list index.html — host keeps the legacy /lattice dashboard",
            manifest_path.display()
        )));
    }
    Ok(BundleLoad::Bundle(BundleManifest { version, entries }))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let dist = crate_dir
        .join("..")
        .join("..")
        .join("packages")
        .join("lattice-dashboard")
        .join("dist");
    println!("cargo:rerun-if-changed={}", dist.display());

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("out dir"));
    let out_file = out_dir.join("dashboard_bundle.rs");

    let mut present = false;
    let mut version = String::from("0.0.0");
    let mut entries: Vec<BundleEntry> = Vec::new();

    match load_bundle(&dist) {
        Ok(BundleLoad::Bundle(bundle)) => {
            present = true;
            version = bundle.version;
            entries = bundle.entries;
        }
        Ok(BundleLoad::Absent(reason)) => println!("cargo:warning={reason}"),
        Err(reason) => panic!("{reason}"),
    }

    let mut src = String::new();
    src.push_str("// @generated by apps/nai-fabric-host/build.rs — do not edit.\n");
    src.push_str(&format!("pub const BUNDLE_PRESENT: bool = {present};\n"));
    src.push_str(&format!(
        "pub const BUNDLE_VERSION: &str = {};\n",
        rust_str(&version)
    ));
    src.push_str("pub static BUNDLE: &[(&str, &[u8], &str)] = &[\n");
    for entry in &entries {
        src.push_str(&format!(
            "    ({}, include_bytes!({}), {}),\n",
            rust_str(&entry.path),
            rust_str(&entry.full.to_string_lossy()),
            rust_str(entry.content_type)
        ));
    }
    src.push_str("];\n");
    std::fs::write(&out_file, src).expect("write generated dashboard bundle table");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dist(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nai_bundle_manifest_{tag}_{}_{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dist");
        dir
    }

    fn write_file(dist: &Path, path: &str, bytes: &[u8]) {
        let full = dist.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&full, bytes).expect("write fixture file");
    }

    fn entry_json(path: &str, bytes: &[u8]) -> serde_json::Value {
        serde_json::json!({
            "path": path,
            "bytes": bytes.len() as u64,
            "blake3": blake3::hash(bytes).to_hex().to_string(),
        })
    }

    fn write_manifest(dist: &Path, manifest: &serde_json::Value) {
        std::fs::write(
            dist.join("bundle-manifest.json"),
            serde_json::to_vec(manifest).expect("encode manifest"),
        )
        .expect("write manifest");
    }

    #[test]
    fn contained_paths_are_accepted_and_escaping_paths_are_rejected() {
        let dist = temp_dist("containment");
        write_file(&dist, "index.html", b"<html></html>");
        write_file(&dist, "assets/a.js", b"console.log(1)");
        let canonical = std::fs::canonicalize(&dist).expect("canonical dist");

        for path in ["index.html", "assets/a.js"] {
            assert!(
                validate_manifest_path(&dist, &canonical, path).is_ok(),
                "{path} must be accepted"
            );
        }
        for path in [
            "C:/Windows/win.ini",
            "C:\\Windows\\win.ini",
            "\\\\server\\share\\x.js",
            "/etc/passwd",
            "../outside.js",
            "assets/../../outside.js",
            "assets/stream:name.js",
        ] {
            match validate_manifest_path(&dist, &canonical, path) {
                Err(PathError::Rejected(_)) => {}
                Ok(_) => panic!("{path} must be rejected"),
                Err(PathError::Unreadable(reason)) => {
                    panic!("{path} must be rejected as malformed, got unreadable: {reason}")
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dist);
    }

    #[test]
    fn missing_or_malformed_manifest_yields_no_bundle() {
        let dist = temp_dist("malformed");
        // Missing manifest + index.
        match load_bundle(&dist).expect("missing bundle is not an integrity failure") {
            BundleLoad::Absent(reason) => assert!(reason.contains("not found"), "{reason}"),
            BundleLoad::Bundle(_) => panic!("missing manifest must not produce a bundle"),
        }

        write_file(&dist, "index.html", b"<html></html>");
        // Unparseable JSON.
        std::fs::write(dist.join("bundle-manifest.json"), b"{ not json").expect("write bad json");
        assert!(matches!(
            load_bundle(&dist).expect("malformed json falls back"),
            BundleLoad::Absent(_)
        ));

        // Unsupported schema.
        write_manifest(&dist, &serde_json::json!({"schema": 2, "files": []}));
        assert!(matches!(
            load_bundle(&dist).expect("unsupported schema falls back"),
            BundleLoad::Absent(_)
        ));

        // Missing files array.
        write_manifest(&dist, &serde_json::json!({"schema": 1}));
        assert!(matches!(
            load_bundle(&dist).expect("missing files falls back"),
            BundleLoad::Absent(_)
        ));

        // File entry without a path.
        write_manifest(&dist, &serde_json::json!({"schema": 1, "files": [{"bytes": 1}]}));
        assert!(matches!(
            load_bundle(&dist).expect("pathless entry falls back"),
            BundleLoad::Absent(_)
        ));

        // Escaping manifest path (the reported security defect).
        write_manifest(
            &dist,
            &serde_json::json!({
                "schema": 1,
                "version": "9.9.9",
                "files": [{"path": "C:/Windows/win.ini", "bytes": 1, "blake3": "00"}],
            }),
        );
        match load_bundle(&dist).expect("escaping path falls back") {
            BundleLoad::Absent(reason) => assert!(reason.contains("malformed"), "{reason}"),
            BundleLoad::Bundle(_) => panic!("escaping manifest path must not produce a bundle"),
        }

        // index.html missing from the file list.
        let html = b"<html></html>";
        write_manifest(
            &dist,
            &serde_json::json!({"schema": 1, "files": [entry_json("other.html", html)]}),
        );
        write_file(&dist, "other.html", html);
        assert!(matches!(
            load_bundle(&dist).expect("missing index entry falls back"),
            BundleLoad::Absent(_)
        ));

        let _ = std::fs::remove_dir_all(&dist);
    }

    #[test]
    fn size_and_digest_mismatches_remain_hard_failures() {
        let dist = temp_dist("integrity");
        let html = b"<html></html>";
        write_file(&dist, "index.html", html);

        let mut wrong_size = entry_json("index.html", html);
        wrong_size["bytes"] = serde_json::json!(html.len() as u64 + 1);
        write_manifest(
            &dist,
            &serde_json::json!({"schema": 1, "files": [wrong_size]}),
        );
        let error = load_bundle(&dist).expect_err("size mismatch must be a hard failure");
        assert!(error.contains("size does not match"), "{error}");

        let mut wrong_digest = entry_json("index.html", html);
        wrong_digest["blake3"] = serde_json::json!("00");
        write_manifest(
            &dist,
            &serde_json::json!({"schema": 1, "files": [wrong_digest]}),
        );
        let error = load_bundle(&dist).expect_err("digest mismatch must be a hard failure");
        assert!(error.contains("digest mismatch"), "{error}");

        let _ = std::fs::remove_dir_all(&dist);
    }

    #[test]
    fn valid_manifest_is_loaded_with_schema_and_version() {
        let dist = temp_dist("valid");
        let html = b"<html></html>";
        let js = b"console.log(1)";
        write_file(&dist, "index.html", html);
        write_file(&dist, "assets/a.js", js);
        write_manifest(
            &dist,
            &serde_json::json!({
                "schema": 1,
                "version": "0.1.0",
                "files": [entry_json("index.html", html), entry_json("assets/a.js", js)],
            }),
        );
        let bundle = match load_bundle(&dist).expect("valid bundle") {
            BundleLoad::Bundle(bundle) => bundle,
            BundleLoad::Absent(reason) => panic!("expected a bundle, got: {reason}"),
        };
        assert_eq!(bundle.version, "0.1.0");
        assert_eq!(bundle.entries.len(), 2);
        assert_eq!(bundle.entries[0].content_type, "text/html; charset=utf-8");
        assert_eq!(bundle.entries[1].content_type, "text/javascript; charset=utf-8");
        let _ = std::fs::remove_dir_all(&dist);
    }
}
