use std::fs;
use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = std::env::var("OUT_DIR").unwrap();

    // lua/ lives at the workspace root (two levels up from crates/ff-script/)
    let lua_dir = Path::new(&manifest_dir).join("../../lua");
    let output_path = Path::new(&out_dir).join("flowfabric.lua");

    // Ordered list of Lua source files. Helpers MUST come first since all
    // registered functions depend on the library-local helpers.
    let lua_files: &[&str] = &[
        "helpers.lua",
        "version.lua",
        "lease.lua",
        "execution.lua",
        "scheduling.lua",
        "suspension.lua",
        "signal.lua",
        "stream.lua",
        "budget.lua",
        "quota.lua",
        "flow.lua",
    ];

    let mut output = String::new();

    // Preamble: library declaration (required by Valkey Functions)
    output.push_str("#!lua name=flowfabric\n");

    for filename in lua_files {
        let path = lua_dir.join(filename);
        let source = fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("Failed to read {}: {}", path.display(), e);
        });
        output.push_str(&format!("\n-- source: lua/{}\n", filename));
        output.push_str(&source);
        output.push('\n');
    }

    fs::write(&output_path, &output).unwrap_or_else(|e| {
        panic!("Failed to write {}: {}", output_path.display(), e);
    });

    // Single source of truth for LIBRARY_VERSION: extract the string
    // literal from `lua/version.lua` and expose it to Rust via
    // `cargo:rustc-env=FLOWFABRIC_LUA_VERSION=...`. `crates/ff-script/src/lib.rs`
    // reads it through `env!("FLOWFABRIC_LUA_VERSION")`. Eliminates the
    // two-place drift class (one bump updates both the Lua function and
    // the Rust constant the loader compares against).
    //
    // Parse contract: `lua/version.lua` must contain exactly one
    // `return 'X'` literal (the body of `ff_version`). String find+slice,
    // no regex or Lua parser dependency. Break the contract → panic with
    // a message that points at the file to fix.
    let version_path = lua_dir.join("version.lua");
    let version_source = fs::read_to_string(&version_path).unwrap_or_else(|e| {
        panic!("Failed to read {}: {}", version_path.display(), e);
    });
    // Walk lines, skipping Lua comments ("--" at start after trimming).
    // The version.lua docstring contains "return 'X'" as explanatory text;
    // a naive `version_source.find("return '")` picks that up instead of
    // the real function body. Line-based skip-comments parse keeps the
    // extract string-based (no regex dep) while avoiding that trap.
    let version = version_source
        .lines()
        .filter_map(|raw| {
            let line = raw.trim_start();
            if line.starts_with("--") {
                return None;
            }
            let i = line.find("return '")?;
            let rest = &line[i + "return '".len()..];
            rest.find('\'').map(|j| rest[..j].to_owned())
        })
        .next()
        .unwrap_or_else(|| {
            panic!(
                "Failed to extract LIBRARY_VERSION from {}: expected exactly one \
                 non-commented `return 'X'` literal (the body of ff_version). Do NOT \
                 maintain a separate copy in crates/ff-script/src/lib.rs — this \
                 extract is the single source of truth.",
                version_path.display()
            )
        });
    // Sanity bounds: non-empty, no embedded quotes/newlines that would
    // imply a broken parse, short enough to be a version string.
    assert!(
        !version.is_empty()
            && version.len() < 64
            && !version.contains('\n')
            && !version.contains('\''),
        "Invalid LIBRARY_VERSION extracted from lua/version.lua: {version:?}"
    );
    println!("cargo:rustc-env=FLOWFABRIC_LUA_VERSION={version}");

    // Tell Cargo to re-run if any Lua file changes. `version.lua` is
    // already in the list above, so changes to it trigger re-extract.
    for filename in lua_files {
        println!("cargo:rerun-if-changed=../../lua/{}", filename);
    }
}
