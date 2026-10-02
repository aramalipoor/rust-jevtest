//! Static evidence, no model: which tests name a changed item directly, through a same-file
//! helper, or through the name-based call graph.
//!
//! Each file is parsed once (in parallel) and every fn body reduced to the names it references:
//! single-segment paths, every segment of longer paths, method calls, struct literal types, and
//! identifiers inside macro token streams (including `{name}` captures in format strings).
//! References resolve to a definition in the same file first: a single-segment name to a free fn
//! of its module or an enclosing one, `x.m()` to a method `m` defined in the file, `T::m` to the
//! file's `impl T { fn m }`. Unresolved references match by name across all files (a method
//! call matches any changed `Owner::m`, since the receiver type is unknown). Matching by name
//! skips names shorter than 3 characters and a stop-list of ubiquitous names; a method with such
//! a name still matches its qualified `Owner::name` form.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use syn::visit::{self, Visit};
use syn::{Attribute, Block, Expr, ImplItem, Item, Signature, TraitItem, Type};

#[derive(Clone, Copy, Debug)]
pub struct SourceFile<'a> {
    pub path: &'a str,
    pub source: &'a str,
}

#[derive(Clone, Debug)]
pub struct ChangedItem {
    pub path: String,
    /// Last segment, e.g. `next_at`.
    pub name: String,
    /// The impl (or trait) type, e.g. `Schedule`.
    pub owner: Option<String>,
    /// What the item is; the coverage layer gates only on executable bodies.
    pub kind: crate::scan::Kind,
}

#[derive(Clone, Debug)]
pub struct TestRef {
    pub path: String,
    /// 1-based inclusive lines, attributes included.
    pub start: usize,
    pub end: usize,
    pub name: String,
}

/// Ordered strongest first: `Direct` > `Helper` > `Transitive(1)` > `Transitive(2)` ...
/// `Covered(n)` comes from the coverage map, not from this module: the test executed `n` changed
/// functions when the map was built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceKind {
    Direct,
    Helper,
    Transitive(u8),
    Covered(u32),
}

#[derive(Clone, Debug)]
pub struct Evidence {
    /// Index into `tests`.
    pub test: usize,
    pub kind: EvidenceKind,
    /// The changed items that triggered it (`Store::open`), or for helper and transitive
    /// evidence the call chain down to one (`setup -> Store::open`).
    pub symbols: Vec<String>,
}

const STOP: &[&str] = &[
    "new", "default", "from", "into", "clone", "build", "run", "get", "set", "len", "is_empty", "iter", "map", "unwrap",
    "expect", "to_string", "as_ref", "fmt", "eq", "hash",
];

/// Whether a name is specific enough to match by name alone across files.
fn keyable(name: &str) -> bool {
    name.chars().nth(2).is_some() && !STOP.contains(&name)
}

/// One evidence per test that has any, strongest kind, sorted by test index.
pub fn evidence(files: &[SourceFile], changed: &[ChangedItem], tests: &[TestRef], call_graph_depth: u8) -> Vec<Evidence> {
    if changed.is_empty() || tests.is_empty() {
        return Vec::new();
    }
    let mut g = Graph::build(files, parse_all(files));
    let file_of: HashMap<&str, usize> = files.iter().enumerate().map(|(i, f)| (f.path, i)).collect();

    // Tests first, so they never act as intermediate callers.
    let test_node: Vec<Option<u32>> = tests
        .iter()
        .map(|t| {
            let fi = *file_of.get(t.path.as_str())?;
            let name = g.names.get(&t.name)?;
            let k = g.file_nodes[fi]
                .clone()
                .find(|&k| g.nodes[k as usize].name == name && (t.start..=t.end).contains(&(g.nodes[k as usize].line as usize)))?;
            g.nodes[k as usize].test = true;
            Some(k)
        })
        .collect();

    for c in changed {
        g.add_changed(c, file_of.get(c.path.as_str()).copied());
    }
    // Hop 1 always runs: same-file helpers count even when caller expansion is off.
    for hop in 1..=call_graph_depth.max(1) {
        if !g.expand(hop) {
            break;
        }
    }

    let mut out = Vec::new();
    for (ti, k) in test_node.into_iter().enumerate() {
        let Some(k) = k else { continue };
        let (best, mut found) = g.matches(k);
        if found.is_empty() {
            continue;
        }
        let file = g.nodes[k as usize].file;
        let kind = if best == 0 {
            EvidenceKind::Direct
        } else if best == 1 && found.iter().any(|&r| g.reasons[r as usize].node.is_some_and(|h| g.nodes[h as usize].file == file)) {
            found.retain(|&r| g.reasons[r as usize].node.is_some_and(|h| g.nodes[h as usize].file == file));
            EvidenceKind::Helper
        } else if best <= call_graph_depth {
            EvidenceKind::Transitive(best)
        } else {
            continue;
        };
        let mut symbols: Vec<String> = Vec::with_capacity(found.len());
        for r in found {
            let label = &g.reasons[r as usize].label;
            if !symbols.contains(label) {
                symbols.push(label.clone());
            }
        }
        out.push(Evidence { test: ti, kind, symbols });
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Global graph

/// A reference after resolution: a same-file free fn node, or a keyable name matched globally.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Ref {
    Local(u32),
    Global(u32),
}

struct Node {
    file: u32,
    name: u32,
    owner: Option<u32>,
    test: bool,
    /// Line of the fn's name.
    line: u32,
    refs: Vec<Ref>,
    /// `A::b` pairs whose `b` is not keyable (only those can match a qualified key).
    quals: Vec<(u32, u32)>,
}

/// Why a name or node is affected: a changed item (depth 0) or a fn that reaches one.
struct Reason {
    label: String,
    depth: u8,
    node: Option<u32>,
}

/// Reasons of the smallest depth seen for a key.
struct Entry {
    depth: u8,
    reasons: Vec<u32>,
}

#[derive(Default)]
struct Names {
    ids: HashMap<String, u32>,
    text: Vec<String>,
    keyable: Vec<bool>,
}

impl Names {
    fn intern(&mut self, s: String) -> u32 {
        if let Some(&id) = self.ids.get(s.as_str()) {
            return id;
        }
        let id = self.text.len() as u32;
        self.keyable.push(keyable(&s));
        self.text.push(s.clone());
        self.ids.insert(s, id);
        id
    }

    fn get(&self, s: &str) -> Option<u32> {
        self.ids.get(s).copied()
    }
}

struct Graph {
    names: Names,
    nodes: Vec<Node>,
    file_nodes: Vec<Range<u32>>,
    reasons: Vec<Reason>,
    /// Per node, the reason it is affected.
    node_reason: Vec<Option<u32>>,
    /// Per name id, an index into `entries` (`u32::MAX` = none).
    bare: Vec<u32>,
    qual: HashMap<(u32, u32), u32>,
    entries: Vec<Entry>,
}

impl Graph {
    fn build(files: &[SourceFile], parsed: Vec<Option<LocalFile>>) -> Graph {
        let mut names = Names::default();
        let mut nodes = Vec::new();
        let mut file_nodes = Vec::with_capacity(files.len());
        for (fi, lf) in parsed.into_iter().enumerate() {
            let start = nodes.len() as u32;
            if let Some(lf) = lf {
                let mut remap = vec![0u32; lf.ids.len()];
                for (s, local) in lf.ids {
                    remap[local as usize] = names.intern(s);
                }
                // Same-file definitions a reference resolves to before falling back to its name.
                let mut free: HashMap<(u32, u32), u32> = HashMap::new();
                let mut owned: HashMap<(u32, u32), u32> = HashMap::new();
                let mut methods: HashMap<u32, Vec<u32>> = HashMap::new();
                for (k, n) in lf.nodes.iter().enumerate() {
                    let node = start + k as u32;
                    let name = remap[n.name as usize];
                    match n.owner {
                        _ if n.test => {}
                        None => {
                            free.entry((n.scope, name)).or_insert(node);
                        }
                        Some(o) => {
                            owned.entry((remap[o as usize], name)).or_insert(node);
                            methods.entry(name).or_default().push(node);
                        }
                    }
                }
                for n in lf.nodes {
                    let mut refs = Vec::with_capacity(n.plain.len() + n.methods.len() + n.segs.len() + n.lasts.len());
                    let mut quals: Vec<(u32, u32)> = Vec::new();
                    let global = |refs: &mut Vec<Ref>, id: u32| {
                        if names.keyable[id as usize] {
                            refs.push(Ref::Global(id));
                        }
                    };
                    for &l in &n.plain {
                        let id = remap[l as usize];
                        match resolve(&free, &lf.parents, n.scope, id) {
                            Some(node) => refs.push(Ref::Local(node)),
                            None => global(&mut refs, id),
                        }
                    }
                    // `x.m()`: a method of that name defined in this file is the likeliest target.
                    for &l in &n.methods {
                        let id = remap[l as usize];
                        match methods.get(&id) {
                            Some(local) => refs.extend(local.iter().map(|&node| Ref::Local(node))),
                            None => global(&mut refs, id),
                        }
                    }
                    for &l in &n.segs {
                        global(&mut refs, remap[l as usize]);
                    }
                    for &(a, b) in &n.lasts {
                        let (a, b) = (remap[a as usize], remap[b as usize]);
                        if let Some(&node) = owned.get(&(a, b)) {
                            refs.push(Ref::Local(node));
                        } else if names.keyable[b as usize] {
                            refs.push(Ref::Global(b));
                        } else {
                            quals.push((a, b));
                        }
                    }
                    refs.sort_unstable();
                    refs.dedup();
                    quals.sort_unstable();
                    quals.dedup();
                    nodes.push(Node {
                        file: fi as u32,
                        name: remap[n.name as usize],
                        owner: n.owner.map(|o| remap[o as usize]),
                        test: n.test,
                        line: n.line,
                        refs,
                        quals,
                    });
                }
            }
            file_nodes.push(start..nodes.len() as u32);
        }
        let node_count = nodes.len();
        let name_count = names.text.len();
        Graph {
            names,
            nodes,
            file_nodes,
            reasons: Vec::new(),
            node_reason: vec![None; node_count],
            bare: vec![u32::MAX; name_count],
            qual: HashMap::new(),
            entries: Vec::new(),
        }
    }

    fn reason(&mut self, label: String, depth: u8, node: Option<u32>) -> u32 {
        self.reasons.push(Reason { label, depth, node });
        (self.reasons.len() - 1) as u32
    }

    /// Makes `name` (or `owner::name` when the name alone is too common) lead to `reason`.
    fn key(&mut self, name: u32, owner: Option<u32>, reason: u32) {
        let depth = self.reasons[reason as usize].depth;
        let slot = if self.names.keyable[name as usize] {
            &mut self.bare[name as usize]
        } else if let Some(owner) = owner {
            self.qual.entry((owner, name)).or_insert(u32::MAX)
        } else {
            return;
        };
        if *slot == u32::MAX {
            *slot = self.entries.len() as u32;
            self.entries.push(Entry { depth, reasons: vec![reason] });
            return;
        }
        let e = &mut self.entries[*slot as usize];
        if depth < e.depth {
            e.depth = depth;
            e.reasons.clear();
        }
        if depth == e.depth {
            e.reasons.push(reason);
        }
    }

    fn add_changed(&mut self, c: &ChangedItem, file: Option<usize>) {
        let name = clean(&c.name);
        let owner = c.owner.as_deref().map(clean_owner);
        let label = match owner {
            Some(o) => format!("{o}::{name}"),
            None => name.to_owned(),
        };
        // A name no file mentions can match nothing.
        let Some(nid) = self.names.get(name) else { return };
        let oid = owner.and_then(|o| self.names.get(o));
        if owner.is_some() && oid.is_none() && !self.names.keyable[nid as usize] {
            return;
        }
        let r = self.reason(label, 0, None);
        self.key(nid, oid, r);
        if let Some(fi) = file {
            for k in self.file_nodes[fi].clone() {
                let n = &self.nodes[k as usize];
                let owner_ok = match (owner, oid) {
                    (None, _) => n.owner.is_none(),
                    (Some(_), o) => o.is_some() && n.owner == o,
                };
                if !n.test && n.name == nid && owner_ok {
                    self.node_reason[k as usize] = Some(r);
                }
            }
        }
    }

    /// Each reason a ref leads to, if any.
    fn ref_reasons(&self, r: Ref) -> &[u32] {
        match r {
            Ref::Local(node) => match &self.node_reason[node as usize] {
                Some(reason) => std::slice::from_ref(reason),
                None => &[],
            },
            Ref::Global(name) => match self.bare[name as usize] {
                u32::MAX => &[],
                e => &self.entries[e as usize].reasons,
            },
        }
    }

    fn qual_reasons(&self, q: &(u32, u32)) -> &[u32] {
        match self.qual.get(q) {
            Some(&e) => &self.entries[e as usize].reasons,
            None => &[],
        }
    }

    /// One caller hop: every non-test fn not yet affected that references a reason of depth
    /// `hop - 1` becomes affected at depth `hop`. False when nothing new was reached.
    fn expand(&mut self, hop: u8) -> bool {
        let mut hits: Vec<(u32, u32)> = Vec::new();
        for (k, n) in self.nodes.iter().enumerate() {
            if n.test || self.node_reason[k].is_some() {
                continue;
            }
            let via = n
                .refs
                .iter()
                .flat_map(|&r| self.ref_reasons(r))
                .chain(n.quals.iter().flat_map(|q| self.qual_reasons(q)))
                .find(|&&r| self.reasons[r as usize].depth < hop);
            if let Some(&via) = via {
                hits.push((k as u32, via));
            }
        }
        for &(k, via) in &hits {
            let n = &self.nodes[k as usize];
            let (name, owner) = (n.name, n.owner);
            let own = match owner {
                Some(o) => format!("{}::{}", self.names.text[o as usize], self.names.text[name as usize]),
                None => self.names.text[name as usize].clone(),
            };
            let label = format!("{own} -> {}", self.reasons[via as usize].label);
            let r = self.reason(label, hop, Some(k));
            self.node_reason[k as usize] = Some(r);
            self.key(name, owner, r);
        }
        !hits.is_empty()
    }

    /// The smallest depth among everything node `k` references, with every reason at that depth.
    fn matches(&self, k: u32) -> (u8, Vec<u32>) {
        let n = &self.nodes[k as usize];
        let mut best = u8::MAX;
        let mut found = Vec::new();
        let all = n.refs.iter().flat_map(|&r| self.ref_reasons(r)).chain(n.quals.iter().flat_map(|q| self.qual_reasons(q)));
        for &r in all {
            let d = self.reasons[r as usize].depth;
            if d < best {
                best = d;
                found.clear();
            }
            if d == best {
                found.push(r);
            }
        }
        (best, found)
    }
}

/// The free fn a single-segment name resolves to in `scope` or an enclosing module of the file
/// (enclosing modules approximate `use super::*`).
fn resolve(free: &HashMap<(u32, u32), u32>, parents: &[Option<u32>], scope: u32, name: u32) -> Option<u32> {
    let mut s = Some(scope);
    while let Some(sc) = s {
        if let Some(&node) = free.get(&(sc, name)) {
            return Some(node);
        }
        s = parents[sc as usize];
    }
    None
}

fn clean(name: &str) -> &str {
    let name = name.rsplit("::").next().unwrap_or(name).trim();
    let name = name.strip_suffix('!').unwrap_or(name);
    name.strip_prefix("r#").unwrap_or(name)
}

fn clean_owner(owner: &str) -> &str {
    let owner = owner.split('<').next().unwrap_or(owner);
    clean(owner)
}

// ---------------------------------------------------------------------------------------------
// Per-file parsing (parallel); names are file-local ids until merged.

struct LocalFile {
    ids: HashMap<String, u32>,
    nodes: Vec<RawNode>,
    /// Module scopes: index 0 is the file, then each inline `mod` with its enclosing scope.
    parents: Vec<Option<u32>>,
}

struct RawNode {
    scope: u32,
    name: u32,
    owner: Option<u32>,
    test: bool,
    line: u32,
    /// Single-segment references; resolve to a free fn of this file's module chain first.
    plain: Vec<u32>,
    /// Method-call names; resolve to a method defined in this file first.
    methods: Vec<u32>,
    /// Non-final segments of longer paths (`a` and `b` in `a::b::c`), matched by name.
    segs: Vec<u32>,
    /// Final segment with its qualifier (`(b, c)`); resolves to this file's `impl b { fn c }` first.
    lasts: Vec<(u32, u32)>,
}

fn parse_all(files: &[SourceFile]) -> Vec<Option<LocalFile>> {
    let mut out: Vec<Option<LocalFile>> = (0..files.len()).map(|_| None).collect();
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(files.len()).max(1);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                // syn and the visitor recurse once per nesting level; give deep expressions room.
                std::thread::Builder::new()
                    .stack_size(32 << 20)
                    .spawn_scoped(s, || {
                        let mut got = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(f) = files.get(i) else { break got };
                            got.push((i, parse(f.source)));
                            // The parsed tree is gone; free its span bookkeeping too.
                            proc_macro2::extra::invalidate_current_thread_spans();
                        }
                    })
                    .expect("spawn evidence parser thread")
            })
            .collect();
        for w in workers {
            for (i, lf) in w.join().unwrap_or_else(|e| std::panic::resume_unwind(e)) {
                out[i] = lf;
            }
        }
    });
    out
}

/// None when the file does not parse (the caller's path policy already covers that file).
fn parse(src: &str) -> Option<LocalFile> {
    let file = syn::parse_file(src).ok()?;
    let mut lf = LocalFile { ids: HashMap::new(), nodes: Vec::new(), parents: vec![None] };
    walk(&file.items, 0, &mut lf);
    Some(lf)
}

fn walk(items: &[Item], scope: u32, lf: &mut LocalFile) {
    for item in items {
        match item {
            Item::Fn(f) => add_fn(lf, scope, &f.attrs, &f.sig, &f.block, None),
            Item::Impl(imp) => {
                let owner = type_name(&imp.self_ty).map(|n| intern(&mut lf.ids, n));
                for ii in &imp.items {
                    if let ImplItem::Fn(f) = ii {
                        add_fn(lf, scope, &f.attrs, &f.sig, &f.block, owner);
                    }
                }
            }
            Item::Trait(t) => {
                let owner = Some(intern(&mut lf.ids, raw(t.ident.to_string())));
                for ti in &t.items {
                    if let TraitItem::Fn(f) = ti
                        && let Some(block) = &f.default
                    {
                        add_fn(lf, scope, &f.attrs, &f.sig, block, owner);
                    }
                }
            }
            Item::Mod(m) => {
                if let Some((_, items)) = &m.content {
                    let child = lf.parents.len() as u32;
                    lf.parents.push(Some(scope));
                    walk(items, child, lf);
                }
            }
            _ => {}
        }
    }
}

fn is_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        let segs = &a.path().segments;
        segs.last().is_some_and(|s| s.ident == "test")
            || segs.first().is_some_and(|s| s.ident == "rstest" || s.ident == "test_case")
    })
}

fn type_name(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(p) => p.path.segments.last().map(|s| raw(s.ident.to_string())),
        Type::Reference(r) => type_name(&r.elem),
        Type::Paren(p) => type_name(&p.elem),
        Type::Group(g) => type_name(&g.elem),
        _ => None,
    }
}

fn raw(s: String) -> String {
    match s.strip_prefix("r#") {
        Some(rest) => rest.to_owned(),
        None => s,
    }
}

fn intern(ids: &mut HashMap<String, u32>, s: String) -> u32 {
    if let Some(&id) = ids.get(s.as_str()) {
        return id;
    }
    let id = ids.len() as u32;
    ids.insert(s, id);
    id
}

fn add_fn(lf: &mut LocalFile, scope: u32, attrs: &[Attribute], sig: &Signature, block: &Block, owner: Option<u32>) {
    let mut c = Collector {
        ids: &mut lf.ids,
        owner,
        calling: false,
        plain: Vec::new(),
        methods: Vec::new(),
        segs: Vec::new(),
        lasts: Vec::new(),
        bound: Vec::new(),
    };
    c.visit_signature(sig);
    c.visit_block(block);
    let Collector { plain, mut methods, mut segs, mut lasts, mut bound, .. } = c;
    bound.sort_unstable();
    bound.dedup();
    // A lowercase name bound by a pattern in this fn and never called is a local variable.
    let mut plain: Vec<u32> =
        plain.into_iter().filter(|&(id, called)| called || bound.binary_search(&id).is_err()).map(|(id, _)| id).collect();
    for v in [&mut plain, &mut methods, &mut segs] {
        v.sort_unstable();
        v.dedup();
    }
    lasts.sort_unstable();
    lasts.dedup();
    let name = intern(&mut lf.ids, raw(sig.ident.to_string()));
    lf.nodes.push(RawNode {
        scope,
        name,
        owner,
        test: is_test(attrs),
        line: sig.ident.span().start().line as u32,
        plain,
        methods,
        segs,
        lasts,
    });
}

struct Collector<'a> {
    ids: &'a mut HashMap<String, u32>,
    /// The impl or trait type, which `Self` stands for.
    owner: Option<u32>,
    /// Set while visiting the callee path of a call expression.
    calling: bool,
    plain: Vec<(u32, bool)>,
    methods: Vec<u32>,
    segs: Vec<u32>,
    lasts: Vec<(u32, u32)>,
    bound: Vec<u32>,
}

impl Collector<'_> {
    fn word(&mut self, s: String) -> Option<u32> {
        match s.as_str() {
            "Self" => self.owner,
            "self" | "super" | "crate" | "_" => None,
            _ => Some(intern(self.ids, raw(s))),
        }
    }

    fn path(&mut self, p: &syn::Path, called: bool) {
        let Some(last) = p.segments.last() else { return };
        let init = p.segments.len() - 1;
        let mut prev = None;
        for seg in p.segments.iter().take(init) {
            prev = self.word(seg.ident.to_string());
            if let Some(id) = prev {
                self.segs.push(id);
            }
        }
        let Some(id) = self.word(last.ident.to_string()) else { return };
        match prev {
            Some(q) => self.lasts.push((q, id)),
            // `foo`, or `super::foo` / `crate::foo` / `::foo`: never a local variable when qualified.
            None => self.plain.push((id, called || init > 0 || p.leading_colon.is_some())),
        }
    }

    /// Identifiers in a macro's tokens, classified like the AST walk: `a::b` (segment and
    /// qualified final), `.m` (method), a lone ident (single-segment; called when parentheses follow).
    fn tokens(&mut self, ts: TokenStream) {
        let tts: Vec<TokenTree> = ts.into_iter().collect();
        let mut prev: Option<u32> = None;
        let mut colons = 0u8;
        let mut after_dot = false;
        for (i, tt) in tts.iter().enumerate() {
            match tt {
                TokenTree::Ident(ident) => {
                    let id = self.word(ident.to_string());
                    if let Some(id) = id {
                        let path_next = matches!(tts.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == ':' && p.spacing() == Spacing::Joint);
                        if path_next {
                            self.segs.push(id);
                        } else if colons == 2 {
                            match prev {
                                Some(q) => self.lasts.push((q, id)),
                                None => self.plain.push((id, true)),
                            }
                        } else if after_dot {
                            self.methods.push(id);
                        } else {
                            let called = matches!(tts.get(i + 1), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis);
                            self.plain.push((id, called));
                        }
                    }
                    prev = id;
                    colons = 0;
                    after_dot = false;
                }
                TokenTree::Punct(p) if p.as_char() == ':' => colons += 1,
                TokenTree::Punct(p) => {
                    after_dot = p.as_char() == '.';
                    prev = None;
                    colons = 0;
                }
                TokenTree::Group(g) => {
                    self.tokens(g.stream());
                    prev = None;
                    colons = 0;
                    after_dot = false;
                }
                TokenTree::Literal(lit) => {
                    self.captures(&lit.to_string());
                    prev = None;
                    colons = 0;
                    after_dot = false;
                }
            }
        }
    }

    /// Inline format arguments: `{name}` / `{name:?}` inside a string literal.
    fn captures(&mut self, lit: &str) {
        if !(lit.starts_with('"') || lit.starts_with("r\"") || lit.starts_with("r#")) {
            return;
        }
        let b = lit.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'{' {
                i += 1;
                continue;
            }
            if b.get(i + 1) == Some(&b'{') {
                i += 2;
                continue;
            }
            let start = i + 1;
            let mut j = start;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if j > start && !b[start].is_ascii_digit() && matches!(b.get(j), Some(b'}' | b':')) {
                let id = intern(self.ids, lit[start..j].to_owned());
                self.plain.push((id, false));
            }
            i = j;
        }
    }
}

impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_path(&mut self, p: &'ast syn::Path) {
        let called = std::mem::take(&mut self.calling);
        self.path(p, called);
        visit::visit_path(self, p);
    }

    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        self.calling = matches!(&*c.func, Expr::Path(p) if p.qself.is_none());
        self.visit_expr(&c.func);
        self.calling = false;
        for arg in &c.args {
            self.visit_expr(arg);
        }
    }

    fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
        if let Some(id) = self.word(m.method.to_string()) {
            self.methods.push(id);
        }
        visit::visit_expr_method_call(self, m);
    }

    fn visit_pat_ident(&mut self, p: &'ast syn::PatIdent) {
        let s = p.ident.to_string();
        if s.starts_with(|c: char| c.is_ascii_lowercase() || c == '_') {
            let id = intern(self.ids, raw(s));
            self.bound.push(id);
        }
        visit::visit_pat_ident(self, p);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        visit::visit_macro(self, m);
        self.tokens(m.tokens.clone());
    }
}
