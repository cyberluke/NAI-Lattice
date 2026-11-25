//! Verified, embedded dashboard bundle (`packages/lattice-dashboard/dist`).
//!
//! `build.rs` verifies the Vite output against `bundle-manifest.json` at compile
//! time and emits a table of `include_bytes!` entries. This module is the single
//! read path for those bytes: no filesystem lookup, no directory traversal, no
//! MIME guessing at request time, no Node dependency at runtime.
//!
//! A missing bundle is not an error: the host serves the legacy dashboard at
//! `/lattice` (and keeps `/lattice/legacy` as the explicit rollback path).

use std::io::Write;

include!(concat!(env!("OUT_DIR"), "/dashboard_bundle.rs"));

// The build script's pure helpers (path containment + manifest parsing) are
// compiled into the test harness so `cargo test -p nai-fabric-host` exercises
// the rules that guard the embedded bundle.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

pub(crate) struct Asset {
    pub bytes: &'static [u8],
    pub content_type: &'static str,
    /// Vite hashes everything under `assets/`, so those files are immutable.
    pub immutable: bool,
}

pub(crate) fn bundle_present() -> bool {
    BUNDLE_PRESENT
}

pub(crate) fn bundle_version() -> &'static str {
    BUNDLE_VERSION
}

/// Map a request path (`/lattice/...`) to a verified embedded asset.
///
/// Only exact manifest paths are reachable. Anything containing `..`, `%`, `\\`,
/// a drive/stream separator `:` or an absolute segment is rejected outright
/// rather than normalized: the bundle contains no such names, so a request for
/// one is hostile or broken. This mirrors the build-time containment rule.
pub(crate) fn lookup(request_path: &str) -> Option<Asset> {
    if !bundle_present() {
        return None;
    }
    let rest = request_path
        .strip_prefix("/lattice/next")
        .or_else(|| request_path.strip_prefix("/lattice"))
        .unwrap_or(request_path);
    // Strip the query/fragment BEFORE normalizing: `?embed=1` is a supported
    // shell mode and must still resolve to index.html.
    let path_only = rest.split(['?', '#']).next().unwrap_or(rest);
    let rel = path_only.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    if rel.contains("..")
        || rel.contains('%')
        || rel.contains('\\')
        || rel.contains(':')
        || rel.starts_with('/')
    {
        return None;
    }
    BUNDLE
        .iter()
        .find(|(path, _, _)| *path == rel)
        .map(|(path, bytes, content_type)| Asset {
            bytes,
            content_type,
            immutable: path.starts_with("assets/"),
        })
}

/// Serve the embedded shell and its assets. Returns after writing the response.
pub(crate) fn serve<W: Write>(w: &mut W, request_path: &str) {
    if !bundle_present() {
        // Honest rollback: the compiled shell is not in this binary.
        let _ = w.write_all(
            b"HTTP/1.1 302 Found\r\nLocation: /lattice\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    }
    let Some(asset) = lookup(request_path) else {
        let body = br#"{"error":"not_found","detail":"no such dashboard bundle asset"}"#;
        let head = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = w.write_all(head.as_bytes());
        let _ = w.write_all(body);
        return;
    };
    let cache = if asset.immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache, must-revalidate"
    };
    // The shell is same-origin only; Dockview injects inline styles, hence
    // style-src 'unsafe-inline'. No eval, no external origins, no frames.
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {len}\r\nCache-Control: {cache}\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; font-src 'self'; frame-ancestors 'self'; base-uri 'none'; form-action 'none'\r\nConnection: close\r\n\r\n",
        ct = asset.content_type,
        len = asset.bytes.len(),
    );
    let _ = w.write_all(head.as_bytes());
    let _ = w.write_all(asset.bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_and_encoded_paths_are_rejected() {
        assert!(lookup("/lattice/../../etc/passwd").is_none());
        assert!(lookup("/lattice/%2e%2e/index.html").is_none());
        assert!(lookup("/lattice/assets\\..\\x.js").is_none());
    }

    #[test]
    fn absolute_windows_manifest_paths_are_rejected() {
        // Regression: `C:/Windows/win.ini` escaped `dist` at build time and a
        // drive/stream separator must never resolve through the asset lookup.
        assert!(lookup("/lattice/C:/Windows/win.ini").is_none());
        assert!(lookup("/lattice/C:\\Windows\\win.ini").is_none());
        assert!(lookup("C:/Windows/win.ini").is_none());
        assert!(lookup("/lattice/assets/a.js:stream").is_none());
    }

    #[test]
    fn empty_path_maps_to_index_when_bundle_exists() {
        let hit = lookup("/lattice/");
        if bundle_present() {
            assert!(hit.is_some(), "built bundle must expose index.html");
        } else {
            assert!(hit.is_none());
        }
    }

    #[test]
    fn embed_query_still_resolves_to_the_shell() {
        if !bundle_present() {
            return;
        }
        for path in ["/lattice?embed=1", "/lattice/?embed=1", "/lattice"] {
            let hit = lookup(path).unwrap_or_else(|| panic!("{path} must resolve"));
            assert_eq!(hit.content_type, "text/html; charset=utf-8", "{path}");
        }
        // The former preview URL stays a compatibility alias.
        let alias = lookup("/lattice/next/").expect("old /lattice/next alias");
        assert_eq!(alias.content_type, "text/html; charset=utf-8");
        // A hashed asset resolves with its query string too.
        let asset = BUNDLE
            .iter()
            .find(|(p, _, _)| p.starts_with("assets/"))
            .map(|(p, _, _)| p.to_string())
            .expect("built bundle must contain at least one asset");
        let hit = lookup(&format!("/lattice/{asset}?v=1"))
            .unwrap_or_else(|| panic!("asset with query string must resolve: {asset}"));
        assert!(hit.immutable, "assets/ entries are immutable");
    }

    fn serve_to_string(path: &str) -> String {
        let mut buf: Vec<u8> = Vec::new();
        serve(&mut buf, path);
        String::from_utf8_lossy(&buf).to_string()
    }

    #[test]
    fn missing_bundle_redirects_to_legacy_dashboard() {
        if bundle_present() {
            return;
        }
        let out = serve_to_string("/lattice/");
        assert!(out.starts_with("HTTP/1.1 302 Found"), "{out}");
        assert!(out.contains("Location: /lattice"), "{out}");
    }

    #[test]
    fn built_bundle_is_served_with_locked_down_headers() {
        if !bundle_present() {
            return;
        }
        let out = serve_to_string("/lattice/");
        assert!(out.starts_with("HTTP/1.1 200 OK"), "{out}");
        assert!(out.contains("Content-Type: text/html; charset=utf-8"), "{out}");
        assert!(out.contains("Cache-Control: no-cache, must-revalidate"), "{out}");
        assert!(out.contains("Content-Security-Policy: default-src 'self'"), "{out}");
        assert!(out.contains("X-Content-Type-Options: nosniff"), "{out}");
        assert!(!out.contains("Location:"), "index must not redirect");
    }
}
