//! Semantic diffs of Cargo manifests and the lockfile, so version stamps and dependency bumps do
//! not escalate like arbitrary build changes.
//!
//! - Workspace-member versions (`[workspace.package] version`, a member's `[package] version`,
//!   the `version` of a dependency entry that is a path to a member, and the members' entries in
//!   `Cargo.lock`) are ignored.
//! - Dependency edits make the package that declares them changed.
//! - Changed external packages in `Cargo.lock` make every member that depends on them,
//!   transitively, changed.
//! - Anything else in the root manifest or the lockfile runs the full suite; anything else in a
//!   member manifest runs that package whole.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use toml::{Table, Value};

use crate::workspace::Workspace;

/// What a manifest or lockfile change means for selection.
pub enum Verdict {
    /// Nothing that builds differently: why.
    Ignore(String),
    /// These packages changed (dependency edits), each with what changed for it.
    Changed(Vec<(usize, String)>),
    /// The package runs whole: why.
    Whole(String),
    /// The full suite runs: why.
    Full(String),
}

const DEP_TABLES: [&str; 5] = ["dependencies", "dev-dependencies", "build-dependencies", "dev_dependencies", "build_dependencies"];

fn parse(text: &str, what: &str) -> Result<Table, String> {
    text.parse::<Table>().map_err(|e| format!("{what} does not parse: {}", e.to_string().lines().next().unwrap_or_default()))
}

/// The root `Cargo.toml`. `root_pkg` is the package the root manifest also declares, if any.
pub fn root(old: Option<&str>, new: Option<&str>, ws: &Workspace, root_pkg: Option<usize>) -> Verdict {
    let (Some(old), Some(new)) = (old, new) else { return Verdict::Full("Cargo.toml added or deleted".into()) };
    let (mut a, mut b) = match (parse(old, "old Cargo.toml"), parse(new, "new Cargo.toml")) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return Verdict::Full(e),
    };
    let members = member_names(ws);
    normalize(&mut a, &members);
    normalize(&mut b, &members);
    if a == b {
        return Verdict::Ignore("only workspace-member versions changed".into());
    }
    if let Some(pkg) = root_pkg {
        let (da, db) = (take_deps(&mut a), take_deps(&mut b));
        if a == b {
            return Verdict::Changed(vec![(pkg, format!("dependencies changed: {}", differing(&da, &db).join(", ")))]);
        }
    }
    Verdict::Full(format!("{} changed", differing(&a, &b).join(", ")))
}

/// Package `pkg`'s own `Cargo.toml` (not the root one).
pub fn member(old: Option<&str>, new: Option<&str>, ws: &Workspace, pkg: usize) -> Verdict {
    let (Some(old), Some(new)) = (old, new) else { return Verdict::Whole("manifest added or deleted".into()) };
    let (mut a, mut b) = match (parse(old, "old manifest"), parse(new, "new manifest")) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return Verdict::Whole(e),
    };
    let members = member_names(ws);
    normalize(&mut a, &members);
    normalize(&mut b, &members);
    if a == b {
        return Verdict::Ignore("only the package version changed".into());
    }
    let (da, db) = (take_deps(&mut a), take_deps(&mut b));
    if a == b {
        return Verdict::Changed(vec![(pkg, format!("dependencies changed: {}", differing(&da, &db).join(", ")))]);
    }
    Verdict::Whole(format!("manifest changed: {}", differing(&a, &b).join(", ")))
}

fn member_names(ws: &Workspace) -> HashSet<&str> {
    ws.packages.iter().map(|p| p.name.as_str()).collect()
}

/// Drops member versions: `package.version`, `workspace.package.version` and `version` on
/// dependency entries that point at a member by path.
fn normalize(doc: &mut Table, members: &HashSet<&str>) {
    if let Some(Value::Table(p)) = doc.get_mut("package") {
        p.remove("version");
    }
    if let Some(Value::Table(w)) = doc.get_mut("workspace") {
        if let Some(Value::Table(p)) = w.get_mut("package") {
            p.remove("version");
        }
        if let Some(Value::Table(d)) = w.get_mut("dependencies") {
            strip_member_versions(d, members);
        }
    }
    for_each_dep_table(doc, |d| strip_member_versions(d, members));
}

fn for_each_dep_table(doc: &mut Table, mut f: impl FnMut(&mut Table)) {
    for key in DEP_TABLES {
        if let Some(Value::Table(d)) = doc.get_mut(key) {
            f(d);
        }
    }
    if let Some(Value::Table(targets)) = doc.get_mut("target") {
        for (_, t) in targets.iter_mut() {
            if let Value::Table(t) = t {
                for key in DEP_TABLES {
                    if let Some(Value::Table(d)) = t.get_mut(key) {
                        f(d);
                    }
                }
            }
        }
    }
}

fn strip_member_versions(deps: &mut Table, members: &HashSet<&str>) {
    for (name, spec) in deps.iter_mut() {
        if let Value::Table(t) = spec {
            let real = t.get("package").and_then(Value::as_str).unwrap_or(name);
            if t.contains_key("path") && members.contains(real) {
                t.remove("version");
            }
        }
    }
}

/// Removes the package's dependency tables (top level and per target) and returns them.
fn take_deps(doc: &mut Table) -> Table {
    let mut out = Table::new();
    for key in DEP_TABLES {
        if let Some(v) = doc.remove(key) {
            out.insert(key.into(), v);
        }
    }
    if let Some(Value::Table(targets)) = doc.get_mut("target") {
        for (cfg, t) in targets.iter_mut() {
            if let Value::Table(t) = t {
                for key in DEP_TABLES {
                    if let Some(v) = t.remove(key) {
                        out.insert(format!("target.{cfg}.{key}"), v);
                    }
                }
            }
        }
        targets.retain(|_, t| !matches!(t, Value::Table(t) if t.is_empty()));
        if targets.is_empty() {
            doc.remove("target");
        }
    }
    out
}

/// Keys (two levels deep, `[profile.release]`) whose values differ.
fn differing(a: &Table, b: &Table) -> Vec<String> {
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    let mut out = Vec::new();
    for k in keys {
        match (a.get(k), b.get(k)) {
            (Some(Value::Table(x)), Some(Value::Table(y))) if x != y => {
                let sub: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
                out.extend(sub.into_iter().filter(|s| x.get(*s) != y.get(*s)).map(|s| format!("[{k}.{s}]")));
            }
            (x, y) if x != y => out.push(format!("[{k}]")),
            _ => {}
        }
    }
    if out.len() > 6 {
        let more = out.len() - 6;
        out.truncate(6);
        out.push(format!("and {more} more"));
    }
    out
}

/// One `[[package]]` of a lockfile, normalized: members carry no version, and dependency strings
/// naming a member are reduced to the name.
#[derive(PartialEq)]
struct Entry {
    rest: Table,
    deps: Vec<String>,
}

/// (name, version, source); members have version `""`.
type Key = (String, String, String);

struct Lock {
    entries: HashMap<Key, Entry>,
    /// Everything but `[[package]]`.
    other: Table,
}

fn read_lock(text: &str, what: &str, members: &HashSet<&str>) -> Result<Lock, String> {
    let mut doc = parse(text, what)?;
    let packages = match doc.remove("package") {
        Some(Value::Array(a)) => a,
        None => Vec::new(),
        Some(_) => return Err(format!("{what}: `package` is not an array")),
    };
    let mut entries = HashMap::new();
    for p in packages {
        let Value::Table(mut t) = p else { return Err(format!("{what}: a package is not a table")) };
        let s = |t: &mut Table, k: &str| match t.remove(k) {
            Some(Value::String(s)) => s,
            _ => String::new(),
        };
        let name = s(&mut t, "name");
        let mut version = s(&mut t, "version");
        let source = s(&mut t, "source");
        if source.is_empty() && members.contains(name.as_str()) {
            version.clear();
        }
        let deps = match t.remove("dependencies") {
            Some(Value::Array(a)) => a
                .into_iter()
                .filter_map(|d| d.as_str().map(|d| normalize_dep(d, members)))
                .collect(),
            _ => Vec::new(),
        };
        entries.insert((name, version, source), Entry { rest: t, deps });
    }
    Ok(Lock { entries, other: doc })
}

/// `"name"`, `"name version"` or `"name version (source)"`; a member reference keeps only the name.
fn normalize_dep(dep: &str, members: &HashSet<&str>) -> String {
    let name = dep.split(' ').next().unwrap_or(dep);
    if !dep.contains('(') && members.contains(name) { name.to_owned() } else { dep.to_owned() }
}

impl Lock {
    /// Dependency key → keys of the entries that depend on it.
    fn reverse(&self) -> HashMap<&Key, Vec<&Key>> {
        let mut by_name: HashMap<&str, Vec<&Key>> = HashMap::new();
        for k in self.entries.keys() {
            by_name.entry(k.0.as_str()).or_default().push(k);
        }
        let mut rev: HashMap<&Key, Vec<&Key>> = HashMap::new();
        for (k, e) in &self.entries {
            for d in &e.deps {
                let mut parts = d.splitn(3, ' ');
                let name = parts.next().unwrap_or_default();
                let version = parts.next();
                let source = parts.next().map(|s| s.trim_start_matches('(').trim_end_matches(')'));
                for &target in by_name.get(name).into_iter().flatten() {
                    if version.is_some_and(|v| v != target.1) || source.is_some_and(|s| s != target.2) {
                        continue;
                    }
                    rev.entry(target).or_default().push(k);
                }
            }
        }
        rev
    }
}

/// The root `Cargo.lock`.
pub fn lockfile(old: Option<&str>, new: Option<&str>, ws: &Workspace) -> Verdict {
    let (Some(old), Some(new)) = (old, new) else { return Verdict::Full("Cargo.lock added or deleted".into()) };
    let members = member_names(ws);
    let (a, b) = match (read_lock(old, "old Cargo.lock", &members), read_lock(new, "new Cargo.lock", &members)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return Verdict::Full(e),
    };
    if a.other != b.other {
        return Verdict::Full(format!("Cargo.lock: {} changed", differing(&a.other, &b.other).join(", ")));
    }
    let is_member = |k: &Key| k.2.is_empty() && members.contains(k.0.as_str());
    // Changed entries, per side: removed or changed (old side), added or changed (new side).
    let gone: Vec<&Key> = a.entries.iter().filter(|(k, e)| b.entries.get(*k) != Some(*e)).map(|(k, _)| k).collect();
    let came: Vec<&Key> = b.entries.iter().filter(|(k, e)| a.entries.get(*k) != Some(*e)).map(|(k, _)| k).collect();

    // member name → what changed for it.
    let mut hits: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    // external name → (old versions, new versions).
    let mut versions: BTreeMap<&str, (BTreeSet<&str>, BTreeSet<&str>)> = BTreeMap::new();
    for (side, keys) in [(0, &gone), (1, &came)] {
        for k in keys {
            if is_member(k) {
                hits.entry(k.0.clone()).or_default().insert("its own dependencies".into());
            } else {
                let v = versions.entry(k.0.as_str()).or_default();
                if side == 0 { v.0.insert(k.1.as_str()) } else { v.1.insert(k.1.as_str()) };
            }
        }
    }
    for (lock, keys) in [(&a, &gone), (&b, &came)] {
        let rev = lock.reverse();
        for k in keys.iter().filter(|k| !is_member(k)) {
            let mut seen: HashSet<&Key> = HashSet::from([*k]);
            let mut queue: VecDeque<&Key> = VecDeque::from([*k]);
            while let Some(at) = queue.pop_front() {
                for &up in rev.get(at).into_iter().flatten() {
                    if seen.insert(up) {
                        if is_member(up) {
                            hits.entry(up.0.clone()).or_default().insert(k.0.clone());
                        }
                        queue.push_back(up);
                    }
                }
            }
        }
    }
    if hits.is_empty() {
        return Verdict::Ignore(if versions.is_empty() {
            "only workspace-member versions changed".into()
        } else {
            format!("no workspace member depends on the changed {}", versions.keys().copied().collect::<Vec<_>>().join(", "))
        });
    }
    let describe = |name: &str| -> String {
        let Some((o, n)) = versions.get(name) else { return name.to_owned() };
        let list = |s: &BTreeSet<&str>| s.iter().copied().collect::<Vec<_>>().join("/");
        match (o.is_empty(), n.is_empty()) {
            (true, _) => format!("{name} {} (added)", list(n)),
            (_, true) => format!("{name} {} (removed)", list(o)),
            _ if o == n => format!("{name} {} (checksum or dependencies)", list(n)),
            _ => format!("{name} {} → {}", list(o), list(n)),
        }
    };
    let mut out = Vec::new();
    for (member, names) in hits {
        let Some(pkg) = ws.by_name(&member) else { continue };
        let mut what: Vec<String> = names.iter().map(|n| describe(n)).collect();
        if what.len() > 5 {
            let more = what.len() - 5;
            what.truncate(5);
            what.push(format!("and {more} more"));
        }
        out.push((pkg, format!("Cargo.lock: {}", what.join(", "))));
    }
    Verdict::Changed(out)
}
