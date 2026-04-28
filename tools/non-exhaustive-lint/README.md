# non-exhaustive-lint

Flags `pub` **structs** marked `#[non_exhaustive]` that have no public
constructor in the same file.

Accepted constructor shapes:

- An inherent `impl Ty { pub fn <name>(...) -> Self }` or
  `impl Ty { pub fn <name>(...) -> Ty }` — any associated function
  that is `pub`, takes no `self` receiver, and returns the type by
  value. Captures `new`, `none`, `normal`, `with_ttl`, `empty`,
  `from_parts`, etc.
- A `pub fn builder() -> BuilderTy` whose return type is a **separate
  builder struct** is NOT counted here — the builder must itself have
  a reachable `.build()`/`.finish()` path to `Ty`. In practice, pair
  the `#[non_exhaustive]` target with either a direct-field
  constructor (`pub fn new(...) -> Self`) or an `impl From<Builder>
  for Ty`; the latter satisfies the lint via the `From` rule.
- `impl From<...> for Ty`
- `impl TryFrom<...> for Ty`
- `impl Default for Ty`, or `#[derive(Default)]` on the struct.

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
- Any `pub` struct, anywhere in the file including nested modules.
  `syn::visit` recurses into `mod` blocks; that matches the intent
  (a `src/` tree routinely nests public types inside modules).
- Only `pub` items (not `pub(crate)` / `pub(super)`).
- Variant-level `#[non_exhaustive]` is out of scope.
- Constructors are only detected in the SAME file as the type
  definition; re-exports are not followed. This is the deliberate
  "sufficient" bar per the plan decision: a type with its constructor
  far away is rare in this codebase and still catches a human reviewer's
  eye.

## Parse failures are fatal

If a `.rs` file under `crates/` cannot be read or parsed, the lint
exits non-zero. Silently skipping on parse errors would let a genuine
violation hide behind a syntax glitch.

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
# Expected: exit 1 + "crates/ff-core/src/lib.rs — DeadApi"
```

Remove the marker (add `impl DeadApi { pub fn new() -> Self { ... } }`
or `impl From<String> for DeadApi`, or `#[derive(Default)]`) and the
lint passes.
