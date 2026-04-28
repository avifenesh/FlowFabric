//! non-exhaustive-lint — flag `pub` items marked `#[non_exhaustive]`
//! that have no public constructor in the same file.
//!
//! Motivation: `feedback_non_exhaustive_needs_constructor.md`. A public
//! #[non_exhaustive] type without a constructor is unbuildable outside
//! the defining crate (downstream users cannot use struct literal
//! syntax, and without a `pub fn` returning `Self` / `Type` or a
//! `From`/`TryFrom` impl they have no way to obtain a value). v0.3.2
//! shipped such a dead API.
//!
//! Scope (per plan decision):
//!   - A constructor in the SAME FILE as the type definition is
//!     sufficient — re-exports are not followed, cross-file
//!     constructors are not tolerated.
//!   - Only `pub` structs (anywhere in the file, including nested
//!     modules — `syn::visit` recurses, which matches the intent:
//!     `src/` trees routinely nest public types inside `mod`s).
//!     Enums are intentionally skipped — `#[non_exhaustive]` on an
//!     enum restricts `match` exhaustiveness but does not block
//!     variant construction (`Enum::Variant` still works downstream).
//!     Variant-level `#[non_exhaustive]` is also out of scope.
//!   - `pub(crate)` / `pub(super)` are ignored — only `pub` items.
//!   - Accepted constructor shapes within the same file:
//!       * an `impl Ty { pub fn <name>(...) -> Self }` — any public
//!         associated function that returns `Self` (by value, not a
//!         reference) and takes no `self` receiver. This captures
//!         `new`, `builder`, `none`, `normal`, `with_ttl`, `empty`,
//!         etc.
//!       * an `impl Ty { pub fn <name>(...) -> Ty }` — same but with
//!         the type named explicitly.
//!       * `impl From<_> for Ty`
//!       * `impl TryFrom<_> for Ty`
//!       * `impl Default for Ty` (derive counted via the struct's own
//!         `#[derive(Default)]` attr, handled below).
//!
//! Scan root: `crates/` — the published surface. `tools/`, `examples/`,
//! and `benches/` are excluded.
//!
//! Parse failures are fatal: a `.rs` file under `crates/` that cannot
//! be read or parsed is treated as a lint error, not silently skipped.
//! Skipping on parse error would let a genuine violation hide behind a
//! syntax glitch in the same file.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use syn::{
    Attribute, Item, ItemImpl, ItemStruct, ReturnType, Type, Visibility, visit::Visit,
};
use walkdir::WalkDir;

fn main() -> ExitCode {
    let repo = std::env::current_dir().expect("cwd");
    let scan_root = repo.join("crates");
    let mut findings: Vec<Finding> = Vec::new();
    let mut parse_errors: Vec<String> = Vec::new();
    for entry in WalkDir::new(&scan_root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("rs"))
    {
        let path = entry.path();
        // Skip tests/benches/examples/build.rs — consumer surface only.
        if path_contains_component(path, "tests")
            || path_contains_component(path, "benches")
            || path_contains_component(path, "examples")
            || path.file_name().and_then(|n| n.to_str()) == Some("build.rs")
        {
            continue;
        }
        let rel = path.strip_prefix(&repo).unwrap_or(path).to_path_buf();
        let src = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                parse_errors.push(format!("read {}: {e}", rel.display()));
                continue;
            }
        };
        let file = match syn::parse_file(&src) {
            Ok(f) => f,
            Err(e) => {
                parse_errors.push(format!("parse {}: {e}", rel.display()));
                continue;
            }
        };
        let mut v = FileVisitor::default();
        v.visit_file(&file);
        for (ty_name, span_line) in v.non_exhaustive_pub_types {
            if !v.constructors.get(&ty_name).copied().unwrap_or(false) {
                findings.push(Finding {
                    path: rel.clone(),
                    ty: ty_name,
                    line: span_line,
                });
            }
        }
    }

    if !parse_errors.is_empty() {
        eprintln!(
            "non-exhaustive-lint: {} file(s) under crates/ failed to read/parse",
            parse_errors.len()
        );
        eprintln!();
        eprintln!(
            "Skipping on parse errors would let genuine violations hide behind syntax glitches."
        );
        eprintln!("Fix the file or exclude it explicitly if intentional.");
        eprintln!();
        for e in &parse_errors {
            eprintln!("  {e}");
        }
        return ExitCode::FAILURE;
    }

    if findings.is_empty() {
        println!("non-exhaustive-lint: OK — no violations found in crates/");
        return ExitCode::SUCCESS;
    }

    eprintln!("non-exhaustive-lint: {} violation(s)", findings.len());
    eprintln!();
    eprintln!("Each of the following `pub` items is marked `#[non_exhaustive]`");
    eprintln!("but has no sibling public constructor (pub fn returning Self / Ty,");
    eprintln!("#[derive(Default)], impl Default, impl From, impl TryFrom) in the");
    eprintln!("same file. Downstream consumers cannot build a value — the type is");
    eprintln!("effectively dead API.");
    eprintln!();
    eprintln!("See feedback_non_exhaustive_needs_constructor.md.");
    eprintln!();
    for f in &findings {
        eprintln!("  {}:{} — {}", f.path.display(), f.line, f.ty);
    }
    ExitCode::FAILURE
}

#[derive(Debug)]
struct Finding {
    path: PathBuf,
    ty: String,
    line: usize,
}

fn path_contains_component(path: &Path, comp: &str) -> bool {
    path.components()
        .any(|c| c.as_os_str().to_str() == Some(comp))
}

#[derive(Default)]
struct FileVisitor {
    /// (type name, approximate line number) for each `pub` struct
    /// carrying `#[non_exhaustive]`.
    non_exhaustive_pub_types: Vec<(String, usize)>,
    /// Type name -> true if any accepted constructor shape found in-file.
    constructors: BTreeMap<String, bool>,
}

impl<'ast> Visit<'ast> for FileVisitor {
    fn visit_item(&mut self, item: &'ast Item) {
        match item {
            Item::Struct(s) => self.check_struct(s),
            Item::Impl(i) => self.check_impl(i),
            _ => {}
        }
        syn::visit::visit_item(self, item);
    }
}

impl FileVisitor {
    fn check_struct(&mut self, s: &ItemStruct) {
        if !is_pub(&s.vis) {
            return;
        }
        if !has_non_exhaustive(&s.attrs) {
            return;
        }
        // Unit / tuple / named-field structs all require an
        // explicit constructor once #[non_exhaustive] is on — struct
        // literal syntax is blocked downstream regardless of shape.
        let name = s.ident.to_string();
        // `#[derive(Default)]` on the struct itself is an acceptable
        // constructor: downstream can call `Ty::default()`.
        if has_derive_default(&s.attrs) {
            self.constructors.insert(name.clone(), true);
        }
        self.non_exhaustive_pub_types.push((name, 0));
    }

    fn check_impl(&mut self, i: &ItemImpl) {
        // Target type name (only if it's a plain path like `Foo` or `Foo<T>`).
        let Some(target) = impl_target_name(i) else {
            return;
        };

        // `impl From<...> for Target`, `impl TryFrom<...> for Target`,
        // `impl Default for Target`.
        if let Some((_not, trait_, _for)) = &i.trait_ {
            if let Some(last) = trait_.segments.last() {
                let name = last.ident.to_string();
                if name == "From" || name == "TryFrom" || name == "Default" {
                    self.constructors.insert(target, true);
                    return;
                }
            }
            // Other trait impls don't count as constructors.
            return;
        }

        // Inherent impl — look for any `pub fn` that takes no `self`
        // receiver and returns `Self` or the target type by value.
        // This captures `new`, `builder`, `none`, `normal`, `empty`,
        // `with_ttl`, `from_parts`, etc.
        for item in &i.items {
            if let syn::ImplItem::Fn(m) = item {
                if !matches!(m.vis, Visibility::Public(_)) {
                    continue;
                }
                // Skip methods — a method (has a `self` receiver) is
                // not a consumer-side constructor.
                if m.sig.inputs.iter().any(|arg| matches!(arg, syn::FnArg::Receiver(_))) {
                    continue;
                }
                if fn_returns_target_by_value(&m.sig.output, &target) {
                    self.constructors.insert(target.clone(), true);
                }
            }
        }
    }
}

fn is_pub(v: &Visibility) -> bool {
    matches!(v, Visibility::Public(_))
}

fn has_non_exhaustive(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| a.path().is_ident("non_exhaustive"))
}

fn has_derive_default(attrs: &[Attribute]) -> bool {
    for a in attrs {
        if !a.path().is_ident("derive") {
            continue;
        }
        let mut found = false;
        let _ = a.parse_nested_meta(|meta| {
            if meta.path.is_ident("Default") {
                found = true;
            }
            Ok(())
        });
        if found {
            return true;
        }
    }
    false
}

fn impl_target_name(i: &ItemImpl) -> Option<String> {
    let ty = &*i.self_ty;
    if let Type::Path(tp) = ty {
        return tp.path.segments.last().map(|s| s.ident.to_string());
    }
    None
}

/// `true` iff the function signature returns `Self` or `<target>`
/// by value — not a reference, not `Option<Self>`, not
/// `Result<Self, _>`. We deliberately stay strict: a consumer-side
/// constructor must yield a value the caller can bind directly.
///
/// Bare-value `Self`/`Ty` is the common shape (`fn none() -> Self`,
/// `fn new() -> Ty`); `Result<Self, E>` fallible constructors are
/// handled via `impl TryFrom` or by pairing with an infallible
/// constructor. If that proves too strict in practice we'll widen;
/// today every live constructor in crates/ fits this shape.
fn fn_returns_target_by_value(ret: &ReturnType, target: &str) -> bool {
    let ReturnType::Type(_, ty) = ret else {
        return false;
    };
    let Type::Path(tp) = &**ty else {
        return false;
    };
    let Some(last) = tp.path.segments.last() else {
        return false;
    };
    let name = last.ident.to_string();
    name == "Self" || name == target
}
