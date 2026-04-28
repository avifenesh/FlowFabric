//! publish-list-lint — cross-check the publishable crate set against
//! every place it's enumerated.
//!
//! Sources:
//!   - Workspace `Cargo.toml` members, filtered by `package.publish != false`
//!     AND `package.metadata.release.release != false`. This is the ground
//!     truth derived from the source tree.
//!   - `release.toml` `# LINT-PUBLISH-LIST-CRATE:` marker block
//!     (structured, in publish order).
//!   - `.github/workflows/release.yml` — `cargo publish -p <name>` steps.
//!   - `docs/RELEASING.md` — numbered publish list.
//!
//! Fails with exit 1 if any of the four sources disagree.
//!
//! See `feedback_release_publish_list_drift.md` — v0.3.0/v0.3.1 failed
//! twice from publish-list drift.

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
                "  release.toml LINT-PUBLISH-LIST-CRATE block, .github/workflows/release.yml, docs/RELEASING.md."
            );
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
    let from_release_toml = parse_release_toml(repo).map_err(|e| vec![e])?;
    let from_release_yml = parse_release_yml(repo).map_err(|e| vec![e])?;
    let from_releasing_md = parse_releasing_md(repo).map_err(|e| vec![e])?;

    let mut errs = Vec::new();

    diff(
        "workspace Cargo.toml",
        "release.toml LINT-PUBLISH-LIST",
        &publishable_from_workspace,
        &from_release_toml,
        &mut errs,
    );
    diff(
        "workspace Cargo.toml",
        ".github/workflows/release.yml",
        &publishable_from_workspace,
        &from_release_yml,
        &mut errs,
    );
    diff(
        "workspace Cargo.toml",
        "docs/RELEASING.md",
        &publishable_from_workspace,
        &from_releasing_md,
        &mut errs,
    );

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
        // Skip `tools/*` — those are self-declared publish = false.
        // We still parse them below, but they won't make the cut.
        let manifest_path = repo.join(member).join("Cargo.toml");
        let raw = fs::read_to_string(&manifest_path)
            .map_err(|e| format!("reading {}: {e}", manifest_path.display()))?;
        let manifest: CrateManifest = toml::from_str(&raw)
            .map_err(|e| format!("parsing {}: {e}", manifest_path.display()))?;
        let Some(pkg) = manifest.package else {
            continue;
        };

        // `publish = false` => not publishable.
        if let Some(p) = &pkg.publish
            && p.as_bool() == Some(false)
        {
            continue;
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

fn parse_release_toml(repo: &Path) -> Result<BTreeSet<String>, String> {
    let raw = fs::read_to_string(repo.join("release.toml"))
        .map_err(|e| format!("reading release.toml: {e}"))?;
    let mut in_block = false;
    let mut set = BTreeSet::new();
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
                set.insert(name);
            }
        }
    }
    if set.is_empty() {
        return Err("release.toml missing LINT-PUBLISH-LIST block or it's empty".into());
    }
    Ok(set)
}

fn parse_release_yml(repo: &Path) -> Result<BTreeSet<String>, String> {
    // Plain-text scan: each publish step has a run line containing
    // `cargo publish -p <name>`. Avoids a full YAML dep for a single
    // grep-style extraction.
    let raw = fs::read_to_string(repo.join(".github/workflows/release.yml"))
        .map_err(|e| format!("reading release.yml: {e}"))?;
    let mut set = BTreeSet::new();
    for line in raw.lines() {
        let t = line.trim();
        if let Some(idx) = t.find("cargo publish -p ") {
            let rest = &t[idx + "cargo publish -p ".len()..];
            // take until first whitespace
            let name: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
            if !name.is_empty() {
                set.insert(name);
            }
        }
    }
    if set.is_empty() {
        return Err("release.yml contains no `cargo publish -p` steps".into());
    }
    Ok(set)
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
        let t = line.trim_start();
        if let Some(after_num) = t.strip_prefix(|c: char| c.is_ascii_digit()) {
            // strip further digits in case of "10."
            let after_num = after_num.trim_start_matches(|c: char| c.is_ascii_digit());
            if let Some(after_dot) = after_num.strip_prefix(". ")
                && let Some(rest) = after_dot.strip_prefix('`')
                && let Some(end) = rest.find('`')
            {
                set.insert(rest[..end].to_string());
            }
        }
        // Stop once we hit the "Excluded" subheader.
        if line.starts_with("Excluded from publish") {
            break;
        }
    }
    if set.is_empty() {
        return Err(
            "docs/RELEASING.md 'What gets published' section yielded no crate names".into(),
        );
    }
    Ok(set)
}
