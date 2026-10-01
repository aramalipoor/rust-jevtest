//! Static test enumeration and changed-symbol extraction with syn.

use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::{Attribute, ImplItem, Item, TraitItem, Type};

pub struct TestFn {
    pub file: String,
    pub start: u32,
    pub end: u32,
    pub module: String,
    pub name: String,
    pub source: String,
}

/// Module path implied by a file's location inside its package (informational only).
pub fn module_base(rel: &str) -> Vec<String> {
    let parts: Vec<&str> = rel.split('/').collect();
    let rest: &[&str] = match parts.as_slice() {
        ["src", "bin", _] | ["tests", _] => &[],
        ["src", "bin", _, rest @ ..] | ["tests", _, rest @ ..] => rest,
        ["src", rest @ ..] => rest,
        _ => &[],
    };
    let mut out: Vec<String> = rest.iter().map(|s| s.strip_suffix(".rs").unwrap_or(s).to_owned()).collect();
    match out.last().map(String::as_str) {
        Some("mod") => {
            out.pop();
        }
        Some("lib" | "main") if out.len() == 1 => {
            out.pop();
        }
        _ => {}
    }
    out
}

fn line(span: Span) -> u32 {
    span.start().line as u32
}

fn end_line(span: Span) -> u32 {
    span.end().line as u32
}

fn is_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        let segs = &a.path().segments;
        segs.last().is_some_and(|s| s.ident == "test")
            || segs.first().is_some_and(|s| s.ident == "rstest" || s.ident == "test_case")
    })
}

fn ident_name(ident: &syn::Ident) -> String {
    let s = ident.to_string();
    match s.strip_prefix("r#") {
        Some(raw) => raw.to_owned(),
        None => s,
    }
}

/// Every test fn in a parsed file. `base` is the file's module path.
pub fn tests_in(file: &syn::File, path: &str, base: &[String], src: &str) -> Vec<TestFn> {
    let lines: Vec<&str> = src.lines().collect();
    let mut module = base.to_vec();
    let mut out = Vec::new();
    collect_tests(&file.items, path, &mut module, &lines, &mut out);
    out
}

fn collect_tests(items: &[Item], path: &str, module: &mut Vec<String>, lines: &[&str], out: &mut Vec<TestFn>) {
    for item in items {
        match item {
            Item::Fn(f) if is_test(&f.attrs) => {
                let start = f.attrs.iter().map(|a| line(a.pound_token.span)).min().unwrap_or_else(|| line(f.sig.fn_token.span));
                let end = end_line(f.block.brace_token.span.close());
                let source = lines
                    .get(start as usize - 1..(end as usize).min(lines.len()))
                    .map(|l| l.join("\n"))
                    .unwrap_or_default();
                out.push(TestFn {
                    file: path.to_owned(),
                    start,
                    end,
                    module: module.join("::"),
                    name: ident_name(&f.sig.ident),
                    source,
                });
            }
            Item::Mod(m) => {
                if let Some((_, items)) = &m.content {
                    module.push(ident_name(&m.ident));
                    collect_tests(items, path, module, lines, out);
                    module.pop();
                }
            }
            _ => {}
        }
    }
}

struct Named {
    path: String,
    start: u32,
    end: u32,
}

/// The innermost named item enclosing each line, as `module::item` paths (deduplicated, in line order).
/// Lines outside every item yield `None` entries collapsed into a single `None`.
pub fn symbols_at(file: &syn::File, base: &[String], lines: &[u32]) -> Vec<Option<String>> {
    let mut named = Vec::new();
    let mut module = base.to_vec();
    collect_named(&file.items, &mut module, &mut named);
    let mut out: Vec<Option<String>> = Vec::new();
    for &l in lines {
        let found = named
            .iter()
            .filter(|n| n.start <= l && l <= n.end)
            .min_by_key(|n| n.end - n.start)
            .map(|n| n.path.clone());
        if !out.contains(&found) {
            out.push(found);
        }
    }
    out
}

fn qualified(module: &[String], name: &str) -> String {
    if module.is_empty() {
        name.to_owned()
    } else {
        format!("{}::{name}", module.join("::"))
    }
}

fn self_type_name(ty: &Type) -> String {
    match ty {
        Type::Path(p) => p.path.segments.last().map(|s| ident_name(&s.ident)).unwrap_or_default(),
        Type::Reference(r) => self_type_name(&r.elem),
        _ => "_".to_owned(),
    }
}

fn collect_named(items: &[Item], module: &mut Vec<String>, out: &mut Vec<Named>) {
    let push = |out: &mut Vec<Named>, module: &[String], name: String, span: Span| {
        out.push(Named { path: qualified(module, &name), start: line(span), end: end_line(span) });
    };
    for item in items {
        match item {
            Item::Fn(f) => push(out, module, ident_name(&f.sig.ident), f.span()),
            Item::Struct(s) => push(out, module, ident_name(&s.ident), s.span()),
            Item::Enum(e) => push(out, module, ident_name(&e.ident), e.span()),
            Item::Union(u) => push(out, module, ident_name(&u.ident), u.span()),
            Item::Const(c) => push(out, module, ident_name(&c.ident), c.span()),
            Item::Static(s) => push(out, module, ident_name(&s.ident), s.span()),
            Item::Type(t) => push(out, module, ident_name(&t.ident), t.span()),
            Item::Macro(m) => {
                if let Some(ident) = &m.ident
                    && m.mac.path.is_ident("macro_rules")
                {
                    push(out, module, format!("{}!", ident_name(ident)), m.span());
                }
            }
            Item::Trait(t) => {
                let tn = ident_name(&t.ident);
                push(out, module, tn.clone(), t.span());
                for ti in &t.items {
                    let (name, span) = match ti {
                        TraitItem::Fn(f) => (&f.sig.ident, f.span()),
                        TraitItem::Const(c) => (&c.ident, c.span()),
                        TraitItem::Type(ty) => (&ty.ident, ty.span()),
                        _ => continue,
                    };
                    push(out, module, format!("{tn}::{}", ident_name(name)), span);
                }
            }
            Item::Impl(imp) => {
                let tn = self_type_name(&imp.self_ty);
                push(out, module, tn.clone(), imp.span());
                for ii in &imp.items {
                    let (name, span) = match ii {
                        ImplItem::Fn(f) => (&f.sig.ident, f.span()),
                        ImplItem::Const(c) => (&c.ident, c.span()),
                        ImplItem::Type(ty) => (&ty.ident, ty.span()),
                        _ => continue,
                    };
                    push(out, module, format!("{tn}::{}", ident_name(name)), span);
                }
            }
            Item::Mod(m) => {
                let name = ident_name(&m.ident);
                if let Some((brace, items)) = &m.content {
                    let start = m.attrs.iter().map(|a| line(a.pound_token.span)).chain([line(m.mod_token.span)]).min().unwrap_or(0);
                    out.push(Named { path: qualified(module, &name), start, end: end_line(brace.span.close()) });
                    module.push(name);
                    collect_named(items, module, out);
                    module.pop();
                }
            }
            _ => {}
        }
    }
}
