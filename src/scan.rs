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

/// Every test fn in a parsed file, plus the context of each module that hosts tests:
/// its `use` lines and the signatures of its non-test fns, structs, enums, consts and the like.
/// `base` is the file's module path.
pub fn tests_in(file: &syn::File, path: &str, base: &[String], src: &str) -> (Vec<TestFn>, Vec<(String, String)>) {
    let lines: Vec<&str> = src.lines().collect();
    let mut module = base.to_vec();
    let mut tests = Vec::new();
    let mut contexts = Vec::new();
    collect_tests(&file.items, path, &mut module, &lines, &mut tests, &mut contexts);
    (tests, contexts)
}

/// Source lines `start..=end` (1-based), each trimmed, joined by a space.
fn snippet(lines: &[&str], start: u32, end: u32) -> String {
    let end = (end as usize).min(lines.len());
    lines.get(start as usize - 1..end).map(|l| l.iter().map(|s| s.trim()).collect::<Vec<_>>().join(" ")).unwrap_or_default()
}

fn collect_tests(
    items: &[Item],
    path: &str,
    module: &mut Vec<String>,
    lines: &[&str],
    out: &mut Vec<TestFn>,
    contexts: &mut Vec<(String, String)>,
) {
    let mut context: Vec<String> = Vec::new();
    let mut has_tests = false;
    for item in items {
        match item {
            Item::Fn(f) if is_test(&f.attrs) => {
                has_tests = true;
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
            Item::Fn(f) => {
                // The signature's first line includes any visibility written before it.
                let sig = f.sig.span();
                context.push(snippet(lines, line(sig), end_line(sig)));
            }
            Item::Use(u) => {
                let s = u.span();
                context.push(snippet(lines, line(s), end_line(s)));
            }
            Item::Struct(_) | Item::Enum(_) | Item::Union(_) | Item::Const(_) | Item::Static(_) | Item::Type(_) | Item::Trait(_) => {
                // First line of the item after its attributes: `pub struct Foo {`, `const X: u64 = 3;`.
                let ident = match item {
                    Item::Struct(s) => s.ident.span(),
                    Item::Enum(e) => e.ident.span(),
                    Item::Union(u) => u.ident.span(),
                    Item::Const(c) => c.ident.span(),
                    Item::Static(s) => s.ident.span(),
                    Item::Type(t) => t.ident.span(),
                    Item::Trait(t) => t.ident.span(),
                    _ => unreachable!(),
                };
                context.push(snippet(lines, line(ident), line(ident)));
            }
            Item::Mod(m) => {
                if let Some((_, items)) = &m.content {
                    module.push(ident_name(&m.ident));
                    collect_tests(items, path, module, lines, out, contexts);
                    module.pop();
                }
            }
            _ => {}
        }
    }
    if has_tests {
        contexts.push((module.join("::"), context.join("\n")));
    }
}

/// A named item and its line span.
#[derive(Clone, PartialEq, Eq)]
pub struct Named {
    /// `module::Type::method`, for display.
    pub path: String,
    /// Last segment without `!` (`next_at`, `Store`, `my_macro`).
    pub name: String,
    /// The impl or trait type a method, const or associated type belongs to.
    pub owner: Option<String>,
    /// A test fn or an inline module: shown, but never a changed item for static evidence.
    pub container_or_test: bool,
    start: u32,
    end: u32,
}

/// The innermost named item enclosing each line of `ranges` (deduplicated, in line order).
/// Lines outside every item yield a single `None`.
pub fn items_at(file: &syn::File, base: &[String], ranges: &[(u32, u32)]) -> Vec<Option<Named>> {
    let mut named = Vec::new();
    let mut module = base.to_vec();
    collect_named(&file.items, &mut module, &mut named);
    let mut out: Vec<Option<Named>> = Vec::new();
    for l in ranges.iter().flat_map(|&(a, b)| a..=b) {
        let found = named.iter().filter(|n| n.start <= l && l <= n.end).min_by_key(|n| n.end - n.start);
        if !out.iter().any(|o| o.as_ref() == found) {
            out.push(found.cloned());
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
    let push = |out: &mut Vec<Named>, module: &[String], owner: Option<&str>, name: String, span: Span, special: bool| {
        let shown = match owner {
            Some(o) => format!("{o}::{name}"),
            None => name.clone(),
        };
        out.push(Named {
            path: qualified(module, &shown),
            name: name.trim_end_matches('!').to_owned(),
            owner: owner.map(str::to_owned),
            container_or_test: special,
            start: line(span),
            end: end_line(span),
        });
    };
    for item in items {
        match item {
            Item::Fn(f) => push(out, module, None, ident_name(&f.sig.ident), f.span(), is_test(&f.attrs)),
            Item::Struct(s) => push(out, module, None, ident_name(&s.ident), s.span(), false),
            Item::Enum(e) => push(out, module, None, ident_name(&e.ident), e.span(), false),
            Item::Union(u) => push(out, module, None, ident_name(&u.ident), u.span(), false),
            Item::Const(c) => push(out, module, None, ident_name(&c.ident), c.span(), false),
            Item::Static(s) => push(out, module, None, ident_name(&s.ident), s.span(), false),
            Item::Type(t) => push(out, module, None, ident_name(&t.ident), t.span(), false),
            Item::Macro(m) => {
                if let Some(ident) = &m.ident
                    && m.mac.path.is_ident("macro_rules")
                {
                    push(out, module, None, format!("{}!", ident_name(ident)), m.span(), false);
                }
            }
            Item::Trait(t) => {
                let tn = ident_name(&t.ident);
                push(out, module, None, tn.clone(), t.span(), false);
                for ti in &t.items {
                    let (name, span) = match ti {
                        TraitItem::Fn(f) => (&f.sig.ident, f.span()),
                        TraitItem::Const(c) => (&c.ident, c.span()),
                        TraitItem::Type(ty) => (&ty.ident, ty.span()),
                        _ => continue,
                    };
                    push(out, module, Some(&tn), ident_name(name), span, false);
                }
            }
            Item::Impl(imp) => {
                let tn = self_type_name(&imp.self_ty);
                push(out, module, None, tn.clone(), imp.span(), false);
                for ii in &imp.items {
                    let (name, span) = match ii {
                        ImplItem::Fn(f) => (&f.sig.ident, f.span()),
                        ImplItem::Const(c) => (&c.ident, c.span()),
                        ImplItem::Type(ty) => (&ty.ident, ty.span()),
                        _ => continue,
                    };
                    push(out, module, Some(&tn), ident_name(name), span, false);
                }
            }
            Item::Mod(m) => {
                let name = ident_name(&m.ident);
                if let Some((brace, items)) = &m.content {
                    let start = m.attrs.iter().map(|a| line(a.pound_token.span)).chain([line(m.mod_token.span)]).min().unwrap_or(0);
                    out.push(Named {
                        path: qualified(module, &name),
                        name: name.clone(),
                        owner: None,
                        container_or_test: true,
                        start,
                        end: end_line(brace.span.close()),
                    });
                    module.push(name);
                    collect_named(items, module, out);
                    module.pop();
                }
            }
            _ => {}
        }
    }
}
