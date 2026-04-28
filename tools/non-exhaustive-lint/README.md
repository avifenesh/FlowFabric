# non-exhaustive-lint

Flags `pub` **structs** marked `#[non_exhaustive]` that have no
constructor (`fn new`, `fn builder`, `impl From<...>`, or `impl TryFrom<...>`)
in the same file.

Enums are intentionally skipped: `#[non_exhaustive]` on an enum only
restricts `match` exhaustiveness, it does not block variant
construction (`Enum::Variant` still works downstream). Only structs
become unbuildable when `#[non_exhaustive]` lands without a
constructor.

Motivation: `feedback_non_exhaustive_needs_constructor.md`. Such a type is
unbuildable by downstream consumers and is effectively dead API. v0.3.2's
smoke caught one example after the fact; this lint catches them before
merge.

## Scope

- Scan root: `crates/`. Excludes `tools/`, `examples/`, `benches/`,
  in-crate `tests/` + `benches/` subdirs, and `build.rs`.
- Only top-level `pub` items (not `pub(crate)` etc.).
- Variant-level `#[non_exhaustive]` is out of scope.
- Constructors are only detected in the SAME file as the type
  definition; re-exports are not followed. This is the deliberate
  "sufficient" bar per the plan decision: a type with its constructor
  far away is rare in this codebase and still catches a human reviewer's
  eye.

## Run locally

```sh
cargo run -p non-exhaustive-lint
```

## Reproduce a failure

Add a throwaway violation:

```rust
// crates/ff-core/src/lib.rs (TEMP — do not commit)
#[non_exhaustive]
pub struct DeadApi {
    pub name: String,
}
```

Then:

```sh
cargo run -p non-exhaustive-lint
# Expected: exit 1 + "crates/ff-core/src/lib.rs:0 — DeadApi"
```

Remove the marker (add `impl DeadApi { pub fn new() -> Self { ... } }`
or `impl From<String> for DeadApi`) and the lint passes.
