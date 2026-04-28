//! publish-list-lint — cross-check the publishable crate set against
//! every place it's enumerated.
//!
//! Sources:
//!   - Workspace `Cargo.toml` members, filtered by `package.publish != false`
//!     (also treats the empty-array form `publish = []` as unpublishable)
//!     AND `package.metadata.release.release != false`. This is the ground
//!     truth derived from the source tree.
//!   - `release.toml` `# LINT-PUBLISH-LIST-CRATE:` marker block
//!     (structured, in publish/topological order).
//!   - `.github/workflows/release.yml` — `cargo publish -p <name>` steps
//!     (in publish/topological order).
//!   - `docs/RELEASING.md` — numbered publish list.
//!   - `.github/workflows/matrix.yml` — `feature-propagation` matrix
//!     `crate:` list (order not asserted; this is a parallel check).
//!
//! Equality is asserted as a set across all five sources. In addition,
//! the two publish-ordered sources (`release.toml` + `release.yml`) must
//! agree on *sequence*, because the release workflow publishes in the
//! `release.yml` order and `release.toml` documents that order for
//! humans — drift between them means the doc lies about what the
//! workflow does. `docs/RELEASING.md` is human-authored narrative and
//! the feature-propagation matrix is parallel-sharded, so their order
//! is not asserted.
//!
//! Fails with exit 1 if any source disagrees.
//!
//! See `feedback_release_publish_list_drift.md` — v0.3.0/v0.3.1 failed
//! twice from publish-list drift.
//!
//! Follow-up (v0.13 target): replace the hardcoded feature-propagation
//! matrix `crate:` list in `.github/workflows/matrix.yml` with a setup
//! step that generates it from the `# LINT-PUBLISH-LIST-CRATE:` markers
//! at runtime, eliminating the fifth copy entirely. The v0.12 lint
//! extension below is the faster-to-ship interim guardrail.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct WorkspaceRoot {
    workspace: WorkspaceSection,
}

#[derive(Debug, Deserialize)]
struct WorkspaceSection {
    members: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CrateManifest {
    package: Option<CratePackage>,
}

#[derive(Debug, Deserialize)]
struct CratePackage {
    name: String,
    #[serde(default)]
    publish: Option<toml::Value>,
    #[serde(default)]
    metadata: Option<toml::Value>,
}

fn main() -> ExitCode {
    let repo = repo_root();
    match run(&repo) {
        Ok(()) => ExitCode::SUCCESS,
        Err(errs) => {
            eprintln!("publish-list-lint: FAIL");
            for e in &errs {
                eprintln!("  - {e}");
            }
            eprintln!();
            eprintln!(
                "Sources of truth must agree: workspace Cargo.toml (publish=true + metadata.release.release!=false),"
            );
            eprintln!(
                "  release.toml LINT-PUBLISH-LIST block, .github/workflows/release.yml, docs/RELEASING.md,"
            );
            eprintln!("  .github/workflows/matrix.yml feature-propagation matrix.");
            eprintln!("See feedback_release_publish_list_drift.md");
            ExitCode::FAILURE
        }
    }
}

fn repo_root() -> PathBuf {
    // The binary is run as `cargo run -p publish-list-lint` from the
    // workspace root, so CWD is the repo root. CI and the
    // pre-publish rehearsal both invoke it that way.
    std::env::current_dir().expect("cwd")
}

fn run(repo: &Path) -> Result<(), Vec<String>> {
    let publishable_from_workspace = discover_publishable(repo).map_err(|e| vec![e])?;
    let (from_release_toml_set, from_release_toml_vec) =
        parse_release_toml(repo).map_err(|e| vec![e])?;
    let (from_release_yml_set, from_release_yml_vec) =
        parse_release_yml(repo).map_err(|e| vec![e])?;
    let from_releasing_md = parse_releasing_md(repo).map_err(|e| vec![e])?;
    let from_feature_propagation =
        parse_feature_propagation_matrix(repo).map_err(|e| vec![e])?;

    let mut errs = Vec::new();

    diff(
        "workspace Cargo.toml",
        "release.toml LINT-PUBLISH-LIST",
        &publishable_from_workspace,
        &from_release_toml_set,
        &mut errs,
    );
    diff(
        "workspace Cargo.toml",
        ".github/workflows/release.yml",
        &publishable_from_workspace,
        &from_release_yml_set,
        &mut errs,
    );
    diff(
        "workspace Cargo.toml",
        "docs/RELEASING.md",
        &publishable_from_workspace,
        &from_releasing_md,
        &mut errs,
    );
    diff(
        "workspace Cargo.toml",
        ".github/workflows/matrix.yml feature-propagation",
        &publishable_from_workspace,
        &from_feature_propagation,
        &mut errs,
    );

    // Order check: release.toml and release.yml publish order must agree.
    // (docs/RELEASING.md + feature-propagation are parallel/narrative and
    // intentionally order-free.)
    if from_release_toml_vec != from_release_yml_vec {
        errs.push(format!(
            "release.toml LINT-PUBLISH-LIST order disagrees with release.yml publish step order:\n     release.toml: {:?}\n     release.yml : {:?}",
            from_release_toml_vec, from_release_yml_vec
        ));
    }

    if errs.is_empty() {
        let mut names: Vec<_> = publishable_from_workspace.iter().cloned().collect();
        names.sort();
        println!(
            "publish-list-lint: OK — {} publishable crates agree across all sources:",
            names.len()
        );
        for n in names {
            println!("  {n}");
        }
        Ok(())
    } else {
        Err(errs)
    }
}

fn diff(
    a_name: &str,
    b_name: &str,
    a: &BTreeSet<String>,
    b: &BTreeSet<String>,
    errs: &mut Vec<String>,
) {
    let only_a: Vec<_> = a.difference(b).cloned().collect();
    let only_b: Vec<_> = b.difference(a).cloned().collect();
    if !only_a.is_empty() {
        errs.push(format!(
            "{a_name} lists crates missing from {b_name}: {only_a:?}"
        ));
    }
    if !only_b.is_empty() {
        errs.push(format!(
            "{b_name} lists crates missing from {a_name}: {only_b:?}"
        ));
    }
}

fn discover_publishable(repo: &Path) -> Result<BTreeSet<String>, String> {
    let root: WorkspaceRoot = toml::from_str(
        &fs::read_to_string(repo.join("Cargo.toml"))
            .map_err(|e| format!("reading Cargo.toml: {e}"))?,
    )
    .map_err(|e| format!("parsing Cargo.toml: {e}"))?;

    let mut publishable = BTreeSet::new();
    for member in &root.workspace.members {
        // All workspace members are parsed; `tools/*` crates declare
        // `publish = false` in their own manifest, so they're filtered
        // out below by the publish / metadata.release.release gates
        // rather than by a path-prefix skip.
        let manifest_path = repo.join(member).join("Cargo.toml");
        let raw = fs::read_to_string(&manifest_path)
            .map_err(|e| format!("reading {}: {e}", manifest_path.display()))?;
        let manifest: CrateManifest = toml::from_str(&raw)
            .map_err(|e| format!("parsing {}: {e}", manifest_path.display()))?;
        let Some(pkg) = manifest.package else {
            continue;
        };

        // Cargo treats both `publish = false` AND `publish = []` (an
        // empty array of registries) as "do not publish". Handle both.
        if let Some(p) = &pkg.publish {
            if p.as_bool() == Some(false) {
                continue;
            }
            if let Some(arr) = p.as_array()
                && arr.is_empty()
            {
                continue;
            }
        }

        // [package.metadata.release] release = false => excluded.
        if let Some(meta) = &pkg.metadata
            && let Some(release) = meta.get("release")
            && release.get("release").and_then(|v| v.as_bool()) == Some(false)
        {
            continue;
        }

        publishable.insert(pkg.name);
    }
    Ok(publishable)
}

fn parse_release_toml(repo: &Path) -> Result<(BTreeSet<String>, Vec<String>), String> {
    let raw = fs::read_to_string(repo.join("release.toml"))
        .map_err(|e| format!("reading release.toml: {e}"))?;
    let mut in_block = false;
    let mut set = BTreeSet::new();
    let mut order = Vec::new();
    for line in raw.lines() {
        let t = line.trim();
        if t == "# LINT-PUBLISH-LIST:BEGIN" {
            in_block = true;
            continue;
        }
        if t == "# LINT-PUBLISH-LIST:END" {
            in_block = false;
            continue;
        }
        if !in_block {
            continue;
        }
        if let Some(rest) = t.strip_prefix("# LINT-PUBLISH-LIST-CRATE:") {
            let name = rest.trim().to_string();
            if !name.is_empty() {
                set.insert(name.clone());
                order.push(name);
            }
        }
    }
    if set.is_empty() {
        return Err("release.toml missing LINT-PUBLISH-LIST block or it's empty".into());
    }
    Ok((set, order))
}

fn parse_release_yml(repo: &Path) -> Result<(BTreeSet<String>, Vec<String>), String> {
    // Plain-text scan: each publish step has a run line containing
    // `cargo publish -p <name>`. Avoids a full YAML dep for a single
    // grep-style extraction.
    let raw = fs::read_to_string(repo.join(".github/workflows/release.yml"))
        .map_err(|e| format!("reading release.yml: {e}"))?;
    let mut set = BTreeSet::new();
    let mut order = Vec::new();
    for line in raw.lines() {
        let t = line.trim();
        if let Some(idx) = t.find("cargo publish -p ") {
            let rest = &t[idx + "cargo publish -p ".len()..];
            // take until first whitespace
            let name: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
            if !name.is_empty() {
                set.insert(name.clone());
                order.push(name);
            }
        }
    }
    if set.is_empty() {
        return Err("release.yml contains no `cargo publish -p` steps".into());
    }
    Ok((set, order))
}

fn parse_releasing_md(repo: &Path) -> Result<BTreeSet<String>, String> {
    let raw = fs::read_to_string(repo.join("docs/RELEASING.md"))
        .map_err(|e| format!("reading docs/RELEASING.md: {e}"))?;
    // The "What gets published" section enumerates crates as a
    // numbered list `<N>. \`<crate>\`` lines. Anchor on the section
    // header to avoid false positives elsewhere in the doc.
    let mut set = BTreeSet::new();
    let mut in_section = false;
    for line in raw.lines() {
        if line.starts_with("## ") {
            in_section = line.contains("What gets published");
            continue;
        }
        if !in_section {
            continue;
        }
        // Numbered-list entry: "1. `ferriskey`", allowing indentation.
        // We peel off any ASCII-digit prefix, then require ". `name`".
        let t = line.trim_start();
        let first = t.chars().next();
        if !matches!(first, Some(c) if c.is_ascii_digit()) {
            // Stop once we hit the "Excluded" subheader.
            if line.starts_with("Excluded from publish") {
                break;
            }
            continue;
        }
        let after_num = t.trim_start_matches(|c: char| c.is_ascii_digit());
        if let Some(after_dot) = after_num.strip_prefix(". ")
            && let Some(rest) = after_dot.strip_prefix('`')
            && let Some(end) = rest.find('`')
        {
            set.insert(rest[..end].to_string());
        }
    }
    if set.is_empty() {
        return Err(
            "docs/RELEASING.md 'What gets published' section yielded no crate names".into(),
        );
    }
    Ok(set)
}

fn parse_feature_propagation_matrix(repo: &Path) -> Result<BTreeSet<String>, String> {
    // Plain-text scan of `.github/workflows/matrix.yml` for the
    // `feature-propagation:` job's `matrix.crate:` list. We locate the
    // `feature-propagation:` job header, then find the `crate:` key
    // and collect the subsequent `- <name>` entries until indentation
    // drops out of the list. A full YAML parse is avoided for the same
    // reason as parse_release_yml — single-key extraction, no security
    // caveats, stable input shape.
    let path = repo.join(".github/workflows/matrix.yml");
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut set = BTreeSet::new();
    let mut in_job = false;
    let mut in_crate_list = false;
    let mut list_indent: Option<usize> = None;
    for line in raw.lines() {
        if line.starts_with("  feature-propagation:") {
            in_job = true;
            continue;
        }
        if !in_job {
            continue;
        }
        // Next top-level (2-space indent) job header ends our scan.
        if line.starts_with("  ")
            && !line.starts_with("   ")
            && line.trim_end().ends_with(':')
            && !line.trim_start().starts_with('-')
            && !line.starts_with("  feature-propagation:")
        {
            break;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("crate:") {
            in_crate_list = true;
            list_indent = None;
            continue;
        }
        if in_crate_list {
            if !trimmed.starts_with("- ") {
                // Either a sibling key or end-of-list.
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    in_crate_list = false;
                }
                continue;
            }
            let indent = line.len() - trimmed.len();
            match list_indent {
                None => list_indent = Some(indent),
                Some(expected) if expected != indent => {
                    in_crate_list = false;
                    continue;
                }
                _ => {}
            }
            let name = trimmed.trim_start_matches("- ").trim().to_string();
            if !name.is_empty() {
                set.insert(name);
            }
        }
    }
    if set.is_empty() {
        return Err(
            ".github/workflows/matrix.yml feature-propagation matrix yielded no crate names"
                .into(),
        );
    }
    Ok(set)
}
