//! non-exhaustive-lint — flag `pub` items marked `#[non_exhaustive]`
//! that have no constructor in the same file.
//!
//! Motivation: `feedback_non_exhaustive_needs_constructor.md`. A public
//! #[non_exhaustive] type without a constructor is unbuildable outside
//! the defining crate (downstream users cannot use struct literal
//! syntax, and without a `fn new`/`fn builder`/`impl (Try)From` they
//! have no way to obtain a value). v0.3.2 shipped such a dead API.
//!
//! Scope (per plan decision):
//!   - A constructor in the SAME FILE as the type definition is
//!     sufficient — re-exports are not followed, cross-file
//!     constructors are not tolerated.
//!   - Only `pub struct`s. Enums are intentionally skipped —
//!     `#[non_exhaustive]` on an enum restricts `match` exhaustiveness
//!     but does not block variant construction (`Enum::Variant` still
//!     works downstream). Variant-level `#[non_exhaustive]` is also
//!     out of scope.
//!   - `pub(crate)` / `pub(super)` are ignored — only `pub` items.
//!   - Accepted constructor shapes within the same file:
//!       * an `impl Ty { fn new(...) -> ... }` (any visibility)
//!       * an `impl Ty { fn builder(...) -> ... }` (any visibility)
//!       * `impl From<_> for Ty`
//!       * `impl TryFrom<_> for Ty`
//!
//! Scan root: `crates/` — the published surface. `tools/`, `examples/`,
//! and `benches/` are excluded.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use syn::{Attribute, Item, ItemImpl, ItemStruct, Visibility, visit::Visit};
use walkdir::WalkDir;

fn main() -> ExitCode {
    let repo = std::env::current_dir().expect("cwd");
    let scan_root = repo.join("crates");
    let mut findings: Vec<Finding> = Vec::new();
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
        let Ok(src) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(file) = syn::parse_file(&src) else {
            continue;
        };
        let mut v = FileVisitor::default();
        v.visit_file(&file);
        for (ty_name, span_line) in v.non_exhaustive_pub_types {
            if !v.constructors.get(&ty_name).copied().unwrap_or(false) {
                findings.push(Finding {
                    path: path.strip_prefix(&repo).unwrap_or(path).to_path_buf(),
                    ty: ty_name,
                    line: span_line,
                });
            }
        }
    }

    if findings.is_empty() {
        println!("non-exhaustive-lint: OK — no violations found in crates/");
        return ExitCode::SUCCESS;
    }

    eprintln!("non-exhaustive-lint: {} violation(s)", findings.len());
    eprintln!();
    eprintln!("Each of the following `pub` items is marked `#[non_exhaustive]`");
    eprintln!("but has no sibling constructor (fn new / fn builder / impl From /");
    eprintln!("impl TryFrom) in the same file. Downstream consumers cannot build");
    eprintln!("a value — the type is effectively dead API.");
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
    /// (type name, approximate line number) for each `pub` struct/enum
    /// carrying `#[non_exhaustive]`.
    non_exhaustive_pub_types: Vec<(String, usize)>,
    /// Type name -> true if any constructor found in-file.
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
        self.non_exhaustive_pub_types.push((s.ident.to_string(), 0));
    }

    fn check_impl(&mut self, i: &ItemImpl) {
        // Target type name (only if it's a plain path like `Foo` or `Foo<T>`).
        let Some(target) = impl_target_name(i) else {
            return;
        };

        // `impl From<...> for Target` and `impl TryFrom<...> for Target`.
        if let Some((_not, trait_, _for)) = &i.trait_ {
            if let Some(last) = trait_.segments.last() {
                let name = last.ident.to_string();
                if name == "From" || name == "TryFrom" {
                    self.constructors.insert(target, true);
                    return;
                }
            }
            // Other trait impls don't count as constructors.
            return;
        }

        // Inherent impl — look for `fn new` / `fn builder` on the type.
        for item in &i.items {
            if let syn::ImplItem::Fn(m) = item {
                let name = m.sig.ident.to_string();
                if name == "new" || name == "builder" {
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

fn impl_target_name(i: &ItemImpl) -> Option<String> {
    let ty = &*i.self_ty;
    if let syn::Type::Path(tp) = ty {
        return tp.path.segments.last().map(|s| s.ident.to_string());
    }
    None
}

// Line numbers are printed as 0 today — `syn` spans round-trip through
// `proc_macro2` and surfacing `LineColumn` from non-proc-macro context
// isn't stable. The type name is grep-able; that's sufficient for the
// CI error to point at the right file.
