//! # api-diff — public-surface differ for two Rust source trees
//!
//! Walks each tree, parses every `*.rs` file with `syn`, collects every
//! `pub` item, normalises the signature so parameter *names* are erased
//! (but types and return type are preserved), and joins the two sets into
//! a CSV.
//!
//! Goal: tell us **what** to implement for parity, never **how**.
//!
//! ## Why erase parameter names?
//!
//! The diff exists to drive parity work. Two functions
//! `fn read(path: &Path) -> Result<Bytes>` and
//! `fn read(p: &Path) -> Result<Bytes>` are the same observable surface.
//! The hash should match. Implementation details (parameter names) belong
//! to the *implementer*, not the spec.
//!
//! ## Why not reflection / rustdoc-json?
//!
//! rustdoc-json requires the target crate to compile (incl. heavy deps like
//! GDAL). `syn` parses source files in isolation; we can diff one tree
//! against another without building either.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

/// What kind of public symbol this entry represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Fn,
    Struct,
    Enum,
    Trait,
    TypeAlias,
    Const,
    Static,
    Mod,
}

impl ItemKind {
    /// Short tag suitable for CSV emission.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Fn        => "fn",
            Self::Struct    => "struct",
            Self::Enum      => "enum",
            Self::Trait     => "trait",
            Self::TypeAlias => "type",
            Self::Const     => "const",
            Self::Static    => "static",
            Self::Mod       => "mod",
        }
    }
}

/// One public item observed in a source tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// Crate this symbol belongs to (the first path component below `src/`).
    pub crate_name: String,
    /// Module path from the crate root, dot-separated. Empty for crate root.
    pub module_path: String,
    /// Item kind (fn / struct / …).
    pub kind: ItemKind,
    /// Simple name (no path, no generics).
    pub name: String,
    /// Hash of the normalised signature.
    pub signature_hash: String,
}

impl Symbol {
    /// Stable join key — same logical symbol in either tree maps to the
    /// same key. Crate is excluded so renames across crates still match.
    pub fn join_key(&self) -> String {
        format!("{}::{}::{}", self.module_path, self.kind.tag(), self.name)
    }
}

/// Side of the diff a symbol came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// One row in the diff CSV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    pub module_path: String,
    pub kind: ItemKind,
    pub name: String,
    pub in_left: bool,
    pub in_right: bool,
    pub signature_match: Option<bool>,
    pub left_hash: Option<String>,
    pub right_hash: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────

/// Walk `root` (expected to be a Cargo workspace or single-crate `src/`),
/// parse every `*.rs` file, return the public symbols.
pub fn collect_tree(root: &Path) -> Result<Vec<Symbol>> {
    let mut out = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "rs") {
            let src = std::fs::read_to_string(p)
                .with_context(|| format!("read {}", p.display()))?;
            let (crate_name, module_path) = derive_module_path(root, p);
            collect_file(&src, &crate_name, &module_path, &mut out)
                .with_context(|| format!("parse {}", p.display()))?;
        }
    }
    Ok(out)
}

/// Compute a SHA-2-flavoured stable hash for a signature string.
/// (We use `DefaultHasher` for simplicity; the value is opaque to humans
/// but stable enough to detect type changes.)
pub fn hash_signature(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Join two symbol sets into diff rows.
pub fn diff(left: &[Symbol], right: &[Symbol]) -> Vec<DiffRow> {
    let mut by_key: HashMap<String, (Option<&Symbol>, Option<&Symbol>)> = HashMap::new();
    for s in left {
        by_key.entry(s.join_key()).or_insert((None, None)).0 = Some(s);
    }
    for s in right {
        by_key.entry(s.join_key()).or_insert((None, None)).1 = Some(s);
    }
    let mut rows: Vec<DiffRow> = by_key
        .into_values()
        .map(|(l, r)| {
            // Reason: every map entry was inserted via either the `left` or
            // the `right` loop above, and each of those assigns exactly one
            // side to `Some(_)`. So at least one of `l`/`r` is always
            // populated by construction.
            let any = match l.or(r) {
                Some(s) => s,
                None => unreachable!("entry inserted with at least one side set"),
            };
            let signature_match = match (l, r) {
                (Some(a), Some(b)) => Some(a.signature_hash == b.signature_hash),
                _ => None,
            };
            DiffRow {
                module_path: any.module_path.clone(),
                kind: any.kind,
                name: any.name.clone(),
                in_left: l.is_some(),
                in_right: r.is_some(),
                signature_match,
                left_hash: l.map(|s| s.signature_hash.clone()),
                right_hash: r.map(|s| s.signature_hash.clone()),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        a.module_path
            .cmp(&b.module_path)
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

/// Write CSV rows to `out`.
pub fn write_csv<W: std::io::Write>(out: W, rows: &[DiffRow]) -> Result<()> {
    let mut w = csv::Writer::from_writer(out);
    w.write_record([
        "module_path",
        "kind",
        "name",
        "in_left",
        "in_right",
        "signature_match",
        "left_hash",
        "right_hash",
    ])?;
    for r in rows {
        w.write_record(&[
            r.module_path.clone(),
            r.kind.tag().to_string(),
            r.name.clone(),
            r.in_left.to_string(),
            r.in_right.to_string(),
            r.signature_match
                .map(|b| b.to_string())
                .unwrap_or_default(),
            r.left_hash.clone().unwrap_or_default(),
            r.right_hash.clone().unwrap_or_default(),
        ])?;
    }
    w.flush()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Internals
// ─────────────────────────────────────────────────────────────────────────

/// Derive (crate_name, module_path) from a file path relative to `root`.
fn derive_module_path(root: &Path, p: &Path) -> (String, String) {
    let rel = p.strip_prefix(root).unwrap_or(p);
    let mut comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if let Some(last) = comps.last_mut() {
        if last.ends_with(".rs") {
            last.truncate(last.len() - 3);
        }
        if last == "mod" || last == "lib" || last == "main" {
            comps.pop();
        }
    }
    // First two components are typically "crates/<crate-name>/src/..."
    // Normalise that to crate_name = <crate-name>, module_path = rest.
    let crate_name = if let (Some(_crates), Some(name)) = (comps.first(), comps.get(1)) {
        name.clone()
    } else {
        "unknown".to_string()
    };
    let trimmed: Vec<String> = comps
        .into_iter()
        .skip_while(|c| c != "src")
        .skip(1) // drop "src"
        .collect();
    (crate_name, trimmed.join("."))
}

fn collect_file(
    src: &str,
    crate_name: &str,
    module_path: &str,
    out: &mut Vec<Symbol>,
) -> Result<()> {
    let file = syn::parse_file(src)?;
    walk_items(&file.items, crate_name, module_path, out);
    Ok(())
}

fn walk_items(items: &[syn::Item], crate_name: &str, module_path: &str, out: &mut Vec<Symbol>) {
    for item in items {
        if let Some(sym) = symbol_from_item(item, crate_name, module_path) {
            out.push(sym);
        }
        // Recurse into inline `pub mod foo { ... }`.
        if let syn::Item::Mod(m) = item {
            if matches!(m.vis, syn::Visibility::Public(_)) {
                if let Some((_, items)) = &m.content {
                    let mp = if module_path.is_empty() {
                        m.ident.to_string()
                    } else {
                        format!("{module_path}.{}", m.ident)
                    };
                    walk_items(items, crate_name, &mp, out);
                }
            }
        }
    }
}

fn symbol_from_item(item: &syn::Item, crate_name: &str, module_path: &str) -> Option<Symbol> {
    let (kind, name, sig_src) = match item {
        syn::Item::Fn(f) if is_public(&f.vis) => {
            (ItemKind::Fn, f.sig.ident.to_string(), normalise_fn_sig(&f.sig))
        }
        syn::Item::Struct(s) if is_public(&s.vis) => {
            (ItemKind::Struct, s.ident.to_string(), normalise_struct(s))
        }
        syn::Item::Enum(e) if is_public(&e.vis) => {
            (ItemKind::Enum, e.ident.to_string(), normalise_enum(e))
        }
        syn::Item::Trait(t) if is_public(&t.vis) => {
            (ItemKind::Trait, t.ident.to_string(), normalise_trait(t))
        }
        syn::Item::Type(t) if is_public(&t.vis) => {
            (ItemKind::TypeAlias, t.ident.to_string(), normalise_type_alias(t))
        }
        syn::Item::Const(c) if is_public(&c.vis) => (
            ItemKind::Const,
            c.ident.to_string(),
            format!("const {}: {}", c.ident, format_type(&c.ty)),
        ),
        syn::Item::Static(s) if is_public(&s.vis) => (
            ItemKind::Static,
            s.ident.to_string(),
            format!("static {}: {}", s.ident, format_type(&s.ty)),
        ),
        syn::Item::Mod(m) if is_public(&m.vis) => (
            ItemKind::Mod,
            m.ident.to_string(),
            format!("mod {}", m.ident),
        ),
        _ => return None,
    };
    Some(Symbol {
        crate_name: crate_name.to_string(),
        module_path: module_path.to_string(),
        kind,
        name,
        signature_hash: hash_signature(&sig_src),
    })
}

fn is_public(v: &syn::Visibility) -> bool {
    matches!(v, syn::Visibility::Public(_))
}

/// Normalise a function signature by erasing parameter *names* but keeping
/// types, generics, return type, and asyncness.
pub fn normalise_fn_sig(sig: &syn::Signature) -> String {
    let mut out = String::new();
    if sig.asyncness.is_some() { out.push_str("async "); }
    if sig.unsafety.is_some()  { out.push_str("unsafe "); }
    out.push_str("fn ");
    out.push_str(&sig.ident.to_string());
    if !sig.generics.params.is_empty() {
        out.push('<');
        out.push_str(
            &sig.generics
                .params
                .iter()
                .map(|p| quote_to_string(p))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('>');
    }
    out.push('(');
    let params: Vec<String> = sig
        .inputs
        .iter()
        .map(|arg| match arg {
            syn::FnArg::Receiver(r) => {
                if r.reference.is_some() {
                    if r.mutability.is_some() { "&mut self".into() } else { "&self".into() }
                } else if r.mutability.is_some() {
                    "mut self".into()
                } else {
                    "self".into()
                }
            }
            syn::FnArg::Typed(pt) => format!("_: {}", format_type(&pt.ty)),
        })
        .collect();
    out.push_str(&params.join(","));
    out.push(')');
    match &sig.output {
        syn::ReturnType::Default => {}
        syn::ReturnType::Type(_, ty) => {
            out.push_str(" -> ");
            out.push_str(&format_type(ty));
        }
    }
    out
}

fn normalise_struct(s: &syn::ItemStruct) -> String {
    let mut out = format!("struct {}", s.ident);
    if !s.generics.params.is_empty() {
        out.push('<');
        out.push_str(
            &s.generics
                .params
                .iter()
                .map(|p| quote_to_string(p))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('>');
    }
    match &s.fields {
        syn::Fields::Unit => {}
        syn::Fields::Named(named) => {
            out.push_str(" { ");
            let fields: Vec<String> = named
                .named
                .iter()
                .filter(|f| is_public(&f.vis))
                .map(|f| {
                    let n = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_default();
                    format!("{n}: {}", format_type(&f.ty))
                })
                .collect();
            out.push_str(&fields.join(","));
            out.push_str(" }");
        }
        syn::Fields::Unnamed(unn) => {
            out.push('(');
            let fields: Vec<String> = unn
                .unnamed
                .iter()
                .filter(|f| is_public(&f.vis))
                .map(|f| format_type(&f.ty))
                .collect();
            out.push_str(&fields.join(","));
            out.push(')');
        }
    }
    out
}

fn normalise_enum(e: &syn::ItemEnum) -> String {
    let variants: Vec<String> = e.variants.iter().map(|v| v.ident.to_string()).collect();
    format!("enum {} {{ {} }}", e.ident, variants.join(","))
}

fn normalise_trait(t: &syn::ItemTrait) -> String {
    let mut sig = format!("trait {}", t.ident);
    if !t.generics.params.is_empty() {
        sig.push('<');
        sig.push_str(
            &t.generics
                .params
                .iter()
                .map(|p| quote_to_string(p))
                .collect::<Vec<_>>()
                .join(","),
        );
        sig.push('>');
    }
    sig
}

fn normalise_type_alias(t: &syn::ItemType) -> String {
    format!("type {} = {}", t.ident, format_type(&t.ty))
}

fn format_type(ty: &syn::Type) -> String {
    quote_to_string(ty)
}

/// Tiny helper: pretty-print any `syn` node by leveraging its `ToTokens` impl,
/// then strip whitespace runs so trivial formatting doesn't perturb the hash.
fn quote_to_string<T: quote_lite::ToTokens>(t: &T) -> String {
    let s = t.to_tokens();
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws { out.push(' '); }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

// Minimal in-house ToTokens facade — we depend on `syn` already; just call
// `prettyplease`-style formatting via `quote::ToTokens` from `syn`'s re-export.
mod quote_lite {
    pub trait ToTokens {
        fn to_tokens(&self) -> String;
    }

    impl<T> ToTokens for T
    where
        T: quote::ToTokens,
    {
        fn to_tokens(&self) -> String {
            let mut ts = proc_macro2::TokenStream::new();
            quote::ToTokens::to_tokens(self, &mut ts);
            ts.to_string()
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_str(src: &str) -> Vec<Symbol> {
        let mut out = Vec::new();
        collect_file(src, "test-crate", "test_mod", &mut out).expect("parse");
        out
    }

    #[test]
    fn collects_public_fn() {
        let syms = collect_str("pub fn foo(x: i32) -> i32 { x }");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "foo");
        assert_eq!(syms[0].kind, ItemKind::Fn);
    }

    #[test]
    fn skips_private_fn() {
        let syms = collect_str("fn private(x: i32) -> i32 { x }");
        assert!(syms.is_empty());
    }

    #[test]
    fn signature_hash_ignores_param_names() {
        let a = collect_str("pub fn read(path: &std::path::Path) -> bool { true }");
        let b = collect_str("pub fn read(p: &std::path::Path) -> bool { true }");
        assert_eq!(a[0].signature_hash, b[0].signature_hash);
    }

    #[test]
    fn signature_hash_detects_type_change() {
        let a = collect_str("pub fn read(path: &str) -> bool { true }");
        let b = collect_str("pub fn read(path: &std::path::Path) -> bool { true }");
        assert_ne!(a[0].signature_hash, b[0].signature_hash);
    }

    #[test]
    fn signature_hash_detects_return_change() {
        let a = collect_str("pub fn read() -> bool { true }");
        let b = collect_str("pub fn read() -> Result<bool, ()> { Ok(true) }");
        assert_ne!(a[0].signature_hash, b[0].signature_hash);
    }

    #[test]
    fn signature_hash_detects_asyncness() {
        let a = collect_str("pub fn fetch() {}");
        let b = collect_str("pub async fn fetch() {}");
        assert_ne!(a[0].signature_hash, b[0].signature_hash);
    }

    #[test]
    fn collects_public_struct_named_fields() {
        let syms = collect_str("pub struct Point { pub x: f64, pub y: f64 }");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].kind, ItemKind::Struct);
        assert_eq!(syms[0].name, "Point");
    }

    #[test]
    fn struct_hash_ignores_private_fields() {
        let a = collect_str("pub struct A { pub x: i32, private: bool }");
        let b = collect_str("pub struct A { pub x: i32 }");
        assert_eq!(a[0].signature_hash, b[0].signature_hash);
    }

    #[test]
    fn collects_public_enum() {
        let syms = collect_str("pub enum Color { Red, Green, Blue }");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].kind, ItemKind::Enum);
    }

    #[test]
    fn diff_classifies_left_only_right_only_both() {
        let left = collect_str("pub fn shared(x: i32) -> i32 { x }");
        let only_left = collect_str("pub fn only_left() {}");
        let only_right = collect_str("pub fn only_right() {}");
        let right_signature_diff = collect_str("pub fn shared(x: f64) -> f64 { x }");

        let l = [left[0].clone(), only_left[0].clone()];
        let r = [right_signature_diff[0].clone(), only_right[0].clone()];
        let rows = diff(&l, &r);

        let shared = rows.iter().find(|r| r.name == "shared").unwrap();
        assert!(shared.in_left && shared.in_right);
        assert_eq!(shared.signature_match, Some(false));

        let lo = rows.iter().find(|r| r.name == "only_left").unwrap();
        assert!(lo.in_left && !lo.in_right);
        assert_eq!(lo.signature_match, None);

        let ro = rows.iter().find(|r| r.name == "only_right").unwrap();
        assert!(!ro.in_left && ro.in_right);
    }

    #[test]
    fn recurses_into_inline_pub_mod() {
        let syms = collect_str("pub mod inner { pub fn nested() {} }");
        let nested = syms.iter().find(|s| s.name == "nested");
        assert!(nested.is_some(), "should descend into pub mod");
    }

    #[test]
    fn pub_crate_is_not_pub() {
        let syms = collect_str("pub(crate) fn restricted() {}");
        assert!(syms.is_empty(), "pub(crate) is internal — must not be diff'd");
    }
}
