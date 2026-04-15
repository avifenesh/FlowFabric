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

    // Tell Cargo to re-run if any Lua file changes
    for filename in lua_files {
        println!("cargo:rerun-if-changed=../../lua/{}", filename);
    }
}
