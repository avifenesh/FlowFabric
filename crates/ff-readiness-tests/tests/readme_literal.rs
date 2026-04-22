//! PR-D1d — README literal parity.
//!
//! Two tests that parse the top-level `README.md` at runtime and
//! assert its quickstart instructions still match the shipping code:
//!
//! 1. `readme_literal_quickstart_boot` — regex-extracts env var name +
//!    byte count from the `openssl rand -hex N` snippet in step 2 of
//!    the quickstart. Guards two things the quickstart promises:
//!    (a) the env var name matches `ServerConfig::from_env`'s
//!    canonical `FF_WAITPOINT_HMAC_SECRET` (direct string equality —
//!    drift here is a README/impl bug); and (b) a secret shaped per
//!    the snippet (2 × N hex chars) boots an in-process server and
//!    serves `/healthz` with 200. The test does not reroute through
//!    `ServerConfig::from_env` — it passes the minted secret directly
//!    via `InProcessServer::start_with_secret`. That narrows what the
//!    test proves: the *shape contract*, plus the env-var name
//!    drift-guard, not the full env-parsing path.
//!
//! 2. `readme_literal_sdk_feature_gate` — regex-extracts the feature name from
//!    the README's `## SDK claim_next() feature gate` toml fence and
//!    asserts `crates/ff-sdk/Cargo.toml` actually declares it in its
//!    `[features]` section. Pure manifest parsing, no server — so the
//!    multi-test-per-file risk is bounded (see issue #112 for the
//!    follow-up to give `InProcessServer` a proper async shutdown).
//!
//! Snippet 3 (coding-agent example) is out of scope — it needs
//! `OPENROUTER_API_KEY` and is not part of the quickstart contract.
//!
//! Run with:
//!   cargo test -p ff-readiness-tests --features readiness \
//!       -- --ignored readme_literal --test-threads=1
//!
//! RED-proofs:
//! * Mutating the env var name in README to `FF_WRONG_SECRET` makes
//!   `readme_literal_quickstart_boot` fail on assertion (a) — the
//!   drift guard detects that README no longer names the canonical
//!   `FF_WAITPOINT_HMAC_SECRET` that `ServerConfig::from_env` reads.
//!   Captured: `tests/red-proofs/readme_literal_quickstart.log`.
//! * Mutating the feature name in the README toml fence to a string
//!   that `crates/ff-sdk/Cargo.toml` doesn't declare makes
//!   `readme_literal_sdk_feature_gate` fail with the expected assertion.
//!   Captured: `tests/red-proofs/readme_literal_sdk_feature.log`.
#![cfg(feature = "readiness")]

use std::path::PathBuf;

use ff_readiness_tests::server::InProcessServer;
use regex::Regex;

const LANE: &str = "readiness-readme";

/// Locate `README.md` at the workspace root, relative to this crate's
/// manifest dir — same pattern as `evidence::path_for`.
fn readme_path() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let mut root = PathBuf::from(manifest);
    root.pop(); // crates/
    root.pop(); // workspace root
    root.join("README.md")
}

fn sdk_manifest_path() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let mut root = PathBuf::from(manifest);
    root.pop(); // crates/
    root.pop(); // workspace root
    root.join("crates").join("ff-sdk").join("Cargo.toml")
}

/// Extract (env_var_name, hex_byte_count) from the quickstart snippet.
/// The README line under step 2 looks like:
///   FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32) cargo run -p ff-server
/// Zero matches => the test fails loudly. That is a feature: if the
/// quickstart syntax drifts, someone has to update this test.
fn parse_quickstart(readme: &str) -> (String, usize) {
    let re = Regex::new(r"(FF_[A-Z0-9_]+)=\$\(openssl rand -hex (\d+)\)")
        .expect("compile quickstart regex");
    let caps = re
        .captures(readme)
        .expect("README quickstart must contain a `FF_…=$(openssl rand -hex N)` snippet");
    let var = caps.get(1).expect("env var capture").as_str().to_owned();
    let bytes: usize = caps
        .get(2)
        .expect("hex byte count capture")
        .as_str()
        .parse()
        .expect("hex byte count must parse as usize");
    assert!(bytes > 0, "hex byte count must be positive");
    (var, bytes)
}

/// Extract the feature name ff-sdk is gated behind from the README's
/// `## SDK claim_next() feature gate` toml fence. Example target line:
///   ff-sdk = { path = "crates/ff-sdk", features = ["direct-valkey-claim"] }
fn parse_sdk_feature(readme: &str) -> String {
    let re = Regex::new(r#"ff-sdk\s*=\s*\{[^}]*features\s*=\s*\[\s*"([a-z0-9-]+)"\s*\]"#)
        .expect("compile sdk feature regex");
    let caps = re
        .captures(readme)
        .expect("README must contain an `ff-sdk = { ... features = [\"…\"] }` snippet");
    caps.get(1)
        .expect("feature name capture")
        .as_str()
        .to_owned()
}

#[tokio::test]
#[ignore]
#[serial_test::serial]
async fn readme_literal_quickstart_boot() {
    let readme = std::fs::read_to_string(readme_path()).expect("read README.md");
    let (var_name, byte_count) = parse_quickstart(&readme);

    // Guard against silent drift from the canonical ServerConfig env
    // var. If README ever uses a different var name, stop — that's an
    // impl/doc drift that needs its own decision, not a test nudge.
    assert_eq!(
        var_name, "FF_WAITPOINT_HMAC_SECRET",
        "README quickstart env var must match ServerConfig::from_env; \
         investigate README or config.rs drift before editing this assertion"
    );

    // `openssl rand -hex N` emits a 2N-char lowercase hex string.
    let hex_len = byte_count * 2;
    let secret: String = std::iter::repeat_n('a', hex_len).collect();
    assert_eq!(secret.len(), hex_len);
    assert!(
        secret.chars().all(|c| c.is_ascii_hexdigit()),
        "synthesised secret must satisfy even-length ASCII hex constraint"
    );

    // Boot the in-process server with exactly that env var value, hit
    // /healthz. `InProcessServer::start_with_secret` itself polls
    // /healthz to 200 before returning, so a successful `.await` is
    // the assertion. An explicit probe is added as a belt for the
    // README contract.
    let server = InProcessServer::start_with_secret(LANE, &secret).await;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .expect("build healthz client");
    let url = format!("{}/healthz", server.base_url);
    let resp = client.get(&url).send().await.expect("GET /healthz");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "GET /healthz must return 200 after a README-literal boot"
    );

    // Explicit drop + yield so any pending axum task has a chance to
    // observe the abort before the next serial test starts.
    drop(server);
    tokio::task::yield_now().await;
}

#[tokio::test]
#[ignore]
#[serial_test::serial]
async fn readme_literal_sdk_feature_gate() {
    let readme = std::fs::read_to_string(readme_path()).expect("read README.md");
    let feature = parse_sdk_feature(&readme);

    let manifest =
        std::fs::read_to_string(sdk_manifest_path()).expect("read crates/ff-sdk/Cargo.toml");

    // Find the `[features]` section and assert the README-declared
    // feature is declared there. A plain `contains` over the whole
    // file would pass on any stray mention, including a comment; scope
    // to the features block between `[features]` and the next `[` top
    // level header.
    let features_section = {
        let start = manifest
            .find("[features]")
            .expect("ff-sdk Cargo.toml must have a [features] section");
        let rest = &manifest[start + "[features]".len()..];
        // Next top-level section header begins with `\n[`.
        let end = rest.find("\n[").unwrap_or(rest.len());
        &rest[..end]
    };

    let needle = format!("{feature} =");
    assert!(
        features_section.contains(&needle),
        "README declares feature `{feature}` on ff-sdk but \
         crates/ff-sdk/Cargo.toml [features] does not declare it; \
         section was:\n{features_section}"
    );
}
