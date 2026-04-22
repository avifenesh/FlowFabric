//! Per-test JSON evidence writer. Each readiness test writes a
//! single JSON document at `target/readiness-evidence/<test>.json`
//! summarising what the test observed. PR-D4 wires this into CI
//! artifact upload.

use std::path::PathBuf;

/// Locate `target/readiness-evidence/` relative to the workspace root,
/// creating it if needed. Returns the file path for `<test>.json`.
pub fn path_for(test_name: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR points at crates/ff-readiness-tests; step up
    // twice to reach the workspace root.
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|_| ".".to_owned());
    let mut root = PathBuf::from(manifest);
    root.pop(); // crates/
    root.pop(); // workspace root
    let dir = root.join("target").join("readiness-evidence");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{test_name}.json"))
}

/// Write a JSON evidence document for `test_name`. Overwrites.
pub fn write(test_name: &str, value: &serde_json::Value) {
    let path = path_for(test_name);
    let body = serde_json::to_string_pretty(value).expect("evidence serialise");
    if let Err(e) = std::fs::write(&path, body) {
        tracing::warn!(?path, error = %e, "failed to write readiness evidence");
    }
}
