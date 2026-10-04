//! The selection pipeline: intake, path policy, reach, discovery, static evidence, Jev screening
//! and judging, and the selection policy. Every layer's verdict is kept per candidate test.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde_json::{Value, json};

use crate::config::{Config, CoveragePolicy, NonRust, OnJevError, Outside, StaticEvidence, WithoutJev};
use crate::coverage::{self, COVER_BOOST, LooseFile};
use crate::evidence::{self, ChangedItem, EvidenceKind, SourceFile, TestRef};
use crate::git::{self, FileChange, Range, Source, Status, Target};
use crate::jev::{self, Answer, Usage};
use crate::manifest::{self, Verdict};
use crate::outside;
use crate::scan::{self, TestFn};
use crate::workspace::{PathDep, Workspace};

/// Ranking bonus for tests with static evidence that is not a must.
pub const BOOST: f64 = 0.2;

/// Per-run switches from the command line that are not config keys.
pub struct Switches {
    pub no_jev: bool,
    pub offline: bool,
}

/// Why a test must run, regardless of Jev.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Must {
    /// The test's own lines changed.
    Changed,
    /// Its whole package must run (manifest, build script, non-Rust file, parse failure, rule).
    Package,
    /// Its body names a changed item (`static_evidence = "must"`).
    Direct,
    /// A same-module helper it calls names a changed item (`static_evidence = "must"`).
    Helper,
    /// The coverage map says it executes a changed function (`coverage.policy = "must"`).
    Covered,
}

impl Must {
    pub fn as_str(self) -> &'static str {
        match self {
            Must::Changed => "changed",
            Must::Package => "package",
            Must::Direct => "direct",
            Must::Helper => "helper",
            Must::Covered => "covered",
        }
    }
}

pub fn evidence_kind(kind: EvidenceKind) -> String {
    match kind {
        EvidenceKind::Direct => "direct".into(),
        EvidenceKind::Helper => "helper".into(),
        EvidenceKind::Transitive(d) => format!("transitive({d})"),
        EvidenceKind::Covered(n) => format!("covered({n})"),
        EvidenceKind::Dependency => "dependency".into(),
    }
}

/// Static evidence (from source names), as opposed to the coverage map's `Covered` or a changed
/// `Dependency`; only static evidence earns [`BOOST`].
pub fn is_static(kind: EvidenceKind) -> bool {
    matches!(kind, EvidenceKind::Direct | EvidenceKind::Helper | EvidenceKind::Transitive(_))
}

/// Stage-1 (screening) verdict.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    /// Not screened: a must, or Jev was off.
    NotAsked,
    /// Kept without asking: a member of its group has static evidence.
    KeptEvidence,
    /// Its group has one test: judged directly.
    Single,
    /// Group Noul at or above `group_threshold`.
    Kept,
    /// Group Noul below `group_threshold`.
    Dropped,
    /// Past `max_questions`.
    OverBudget,
    Failed,
    Uncached,
}

/// Stage-2 (judging) verdict.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Judge {
    NotAsked,
    Judged,
    OverBudget,
    Failed,
    Uncached,
}

pub struct Candidate {
    pub pkg: usize,
    /// Reverse-dependency hops from a changed package.
    pub depth: u32,
    pub test: TestFn,
    pub must: Option<Must>,
    pub evidence: Option<(EvidenceKind, Vec<String>)>,
    /// `None`: the coverage map does not know the test (or there is no map); else the changed
    /// functions it executes.
    pub coverage: Option<Vec<String>>,
    /// Dropped by the coverage gate before Jev.
    pub gated: bool,
    /// Ranking bonus: [`BOOST`] for static evidence, [`COVER_BOOST`] for coverage under
    /// `policy = "boost"`; a test with a bonus skips screening.
    pub bonus: f64,
    pub group: Option<usize>,
    pub screen: Screen,
    pub judge: Judge,
    /// Per view, in [`VIEWS`] order (names, body).
    pub nouls: [Option<f64>; 2],
    pub ranks: [Option<usize>; 2],
    pub score: Option<f64>,
    pub reasons: Vec<&'static str>,
    pub selected: bool,
}

impl Candidate {
    pub fn boost(&self) -> f64 {
        self.bonus
    }

    /// Ordering key for `max_tests` / `min_tests`: score, else group Noul, else the boost.
    fn key(&self, groups: &[Group]) -> f64 {
        self.score.or_else(|| self.group.and_then(|g| groups[g].noul)).unwrap_or(0.0).max(self.boost())
    }
}

/// View names, indexing [`Candidate::nouls`] and [`Candidate::ranks`].
pub const VIEWS: [&str; 2] = ["names", "body"];

pub struct Group {
    pub pkg: usize,
    pub file: String,
    pub module: String,
    pub members: Vec<usize>,
    pub noul: Option<f64>,
    /// The module's `use` lines and item signatures, for the screening question.
    context: String,
}

pub struct Escalation {
    pub file: String,
    /// `full` (whole workspace), `whole` (one package), `changed` (the package counts as
    /// changed: a dependency moved) or `ignored` (examined, changes nothing: a version stamp).
    pub kind: &'static str,
    pub package: Option<usize>,
    pub reason: String,
}

pub struct RuleFired {
    /// 0-based index into `config.rules`.
    pub index: usize,
    pub files: Vec<String>,
}

pub enum JevState {
    /// Jev judged (or tried to): the key's source name, or `None` offline without a key.
    On { key_source: Option<&'static str>, offline: bool },
    /// Jev skipped: why.
    Off(String),
}

pub struct Selection {
    pub base: String,
    pub target: Target,
    /// How the diff was chosen (`[changes]` / scope flags).
    pub scope: crate::changes::Scope,
    pub files: Vec<FileChange>,
    pub ws: Workspace,
    pub ignored: Vec<String>,
    /// Files outside every package that no Rust source names (or all of them under
    /// `paths.outside = "ignore"`).
    pub outside_ignored: Vec<String>,
    pub escalations: Vec<Escalation>,
    pub rules_fired: Vec<RuleFired>,
    /// Changed packages, by name.
    pub changed: Vec<usize>,
    /// (package, reverse-dependency hops); packages a rule names appear with 0.
    pub reached: Vec<(usize, u32)>,
    /// `pkg: module::Item` lines given to Jev and the report.
    pub symbols: Vec<String>,
    pub items: Vec<ChangedItem>,
    /// Packages whose every test must run → why.
    pub whole: BTreeMap<usize, String>,
    /// Packages changed through a dependency (lockfile, manifest dependency tables, a path
    /// dependency) → what changed.
    pub dependency: BTreeMap<usize, Vec<String>>,
    pub candidates: Vec<Candidate>,
    pub groups: Vec<Group>,
    pub stages: Vec<(&'static str, Usage)>,
    pub jev: JevState,
    /// Jev errors that triggered `on_jev_error`.
    pub jev_error: Option<String>,
    /// Why every test runs, when it does.
    pub full: Option<String>,
    /// Nextest filtersets OR'ed in by fired rules.
    pub rule_runs: Vec<String>,
    /// The coverage layer's map, policy and verdicts.
    pub coverage: coverage::State,
    pub wall_ms: u128,
    /// Wall time per layer in run order, in ms.
    pub timings: Vec<(&'static str, f64)>,
}

/// Splits a run's wall time into named layers.
struct Laps {
    last: Instant,
    laps: Vec<(&'static str, f64)>,
}

impl Laps {
    fn mark(&mut self, layer: &'static str) {
        let now = Instant::now();
        self.laps.push((layer, (now - self.last).as_secs_f64() * 1000.0));
        self.last = now;
    }
}

pub fn globset(patterns: &[String]) -> Result<GlobSet, String> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(GlobBuilder::new(p).literal_separator(true).build().map_err(|e| format!("bad glob {p}: {e}"))?);
    }
    b.build().map_err(|e| e.to_string())
}

fn overlaps(ranges: &[Range], start: u32, end: u32) -> bool {
    ranges.iter().any(|&(a, b)| a <= end && start <= b)
}

pub fn run(root: &Path, cfg: &Config, mut scope: crate::changes::Scope, sw: &Switches) -> Result<Selection, String> {
    let started = Instant::now();
    let mut laps = Laps { last: started, laps: Vec::new() };
    let sel = &cfg.select;
    let base = scope.base.clone();
    let target = scope.target.clone();

    // Layer 1: intake (the diff the scope already ran and narrowed to `changes.files`).
    let mut new_src = Source::new(root, &target)?;
    let files = std::mem::take(&mut scope.changes);
    laps.mark("intake");
    let ws = Workspace::load(root)?;
    laps.mark("cargo metadata");
    let full_run = globset(&cfg.paths.full_run)?;
    let ignore = globset(&cfg.paths.ignore)?;
    let rule_sets: Vec<GlobSet> = cfg.rules.iter().map(|r| globset(&r.when)).collect::<Result<_, _>>()?;
    let mut old_src: Option<Source> = None;
    let (cov_map, cov_state) = coverage::open(root, &cfg.coverage, &base);
    // Changed lines outside items, per file side; only the coverage layer reads them.
    let mut loose: Vec<LooseFile> = Vec::new();

    let mut out = Selection {
        base,
        target,
        scope,
        files: Vec::new(),
        ws,
        ignored: Vec::new(),
        outside_ignored: Vec::new(),
        escalations: Vec::new(),
        rules_fired: Vec::new(),
        changed: Vec::new(),
        reached: Vec::new(),
        symbols: Vec::new(),
        items: Vec::new(),
        whole: BTreeMap::new(),
        dependency: BTreeMap::new(),
        candidates: Vec::new(),
        groups: Vec::new(),
        stages: Vec::new(),
        jev: JevState::Off(String::new()),
        jev_error: None,
        full: None,
        rule_runs: Vec::new(),
        coverage: cov_state,
        wall_ms: 0,
        timings: Vec::new(),
    };
    let ws = &out.ws;

    // Layer 2: path policy, plus changed items from both sides of every changed `.rs` file.
    let mut changed: BTreeSet<usize> = BTreeSet::new();
    let mut fired: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut full_reasons: Vec<String> = Vec::new();
    let mut seen_items: HashSet<(String, String)> = HashSet::new();
    // Non-ignored files outside every package and path dependency; `paths.outside` decides.
    // `outside_gitlink[i]`: `outside[i]` is a submodule.
    let mut outside: Vec<String> = Vec::new();
    let mut outside_gitlink: Vec<bool> = Vec::new();
    let mut path_deps = PathDeps::default();
    let root_pkg = ws.packages.iter().position(|p| p.dir.is_empty());
    for f in &files {
        let mut paths: Vec<&str> = f.new_path.iter().chain(&f.old_path).map(String::as_str).collect();
        paths.dedup();
        // Sides of this file that are Rust source inside a package and need item extraction.
        let mut rust_new: Option<usize> = None;
        let mut rust_old: Option<usize> = None;
        for path in paths {
            if full_run.is_match(path) {
                full_reasons.push(format!("{path} matches paths.full_run"));
                out.escalations.push(Escalation { file: path.into(), kind: "full", package: None, reason: "paths.full_run".into() });
                continue;
            }
            // Rules add must-runs; only an `exclusive` one replaces the handling below.
            let mut exclusive = false;
            for (i, set) in rule_sets.iter().enumerate() {
                if set.is_match(path) {
                    fired.entry(i).or_default().push(path.to_owned());
                    exclusive |= cfg.rules[i].exclusive;
                }
            }
            if exclusive {
                continue;
            }
            if ignore.is_match(path) {
                out.ignored.push(path.to_owned());
                continue;
            }
            let owner = ws.owner(path);
            let rel = |pkg: usize| {
                let pdir = &ws.packages[pkg].dir;
                if pdir.is_empty() { path } else { &path[pdir.len() + 1..] }
            };
            if path == "Cargo.toml" || path == "Cargo.lock" || owner.is_some_and(|p| rel(p) == "Cargo.toml") {
                let old = if f.old_path.as_deref() == Some(path) && !matches!(f.status, Status::Added | Status::Untracked) {
                    if old_src.is_none() {
                        old_src = Some(Source::at(root, &out.base)?);
                    }
                    old_src.as_mut().and_then(|s| s.read(path))
                } else {
                    None
                };
                let new = if f.new_path.as_deref() == Some(path) { new_src.read(path) } else { None };
                let verdict = match (path, owner) {
                    ("Cargo.toml", _) => manifest::root(old.as_deref(), new.as_deref(), ws, root_pkg),
                    ("Cargo.lock", _) => manifest::lockfile(old.as_deref(), new.as_deref(), ws),
                    (_, Some(pkg)) => manifest::member(old.as_deref(), new.as_deref(), ws, pkg),
                    (_, None) => unreachable!("a member manifest has an owner"),
                };
                let esc = |kind, package, reason| Escalation { file: path.into(), kind, package, reason };
                match verdict {
                    Verdict::Ignore(why) => out.escalations.push(esc("ignored", owner, why)),
                    Verdict::Changed(list) => {
                        for (pkg, why) in list {
                            changed.insert(pkg);
                            out.dependency.entry(pkg).or_default().push(why.clone());
                            out.escalations.push(esc("changed", Some(pkg), why));
                        }
                    }
                    Verdict::Whole(why) => {
                        let pkg = owner.expect("only member manifests run whole");
                        changed.insert(pkg);
                        whole_package(&mut out.whole, &mut out.escalations, pkg, path, why);
                    }
                    Verdict::Full(why) => {
                        full_reasons.push(format!("{path}: {why}"));
                        out.escalations.push(esc("full", None, why));
                    }
                }
                continue;
            }
            let Some(pkg) = owner else {
                match path_deps.find(root, ws, &mut new_src, path) {
                    Some((name, dir, members)) => {
                        let why = format!("path dependency {name} ({dir}) changed");
                        for m in members {
                            changed.insert(m);
                            let list = out.dependency.entry(m).or_default();
                            if !list.contains(&why) {
                                list.push(why.clone());
                                out.escalations.push(Escalation { file: path.into(), kind: "changed", package: Some(m), reason: why.clone() });
                            }
                        }
                    }
                    None => {
                        outside.push(path.to_owned());
                        outside_gitlink.push(f.gitlink);
                    }
                }
                continue;
            };
            changed.insert(pkg);
            let rel = rel(pkg);
            let whole_why = if rel == "build.rs" {
                Some("build script changed")
            } else if !path.ends_with(".rs") {
                (sel.non_rust == NonRust::WholePackage).then_some("non-Rust file changed")
            } else {
                None
            };
            if let Some(why) = whole_why {
                whole_package(&mut out.whole, &mut out.escalations, pkg, path, why.into());
                continue;
            }
            if !path.ends_with(".rs") {
                continue; // non_rust = "jev": the package counts as changed; Jev reads the diff.
            }
            if f.new_path.as_deref() == Some(path) {
                rust_new = Some(pkg);
            }
            if f.old_path.as_deref() == Some(path) && !matches!(f.status, Status::Added | Status::Untracked) {
                rust_old = Some(pkg);
            }
        }

        let item_path = f.path().to_owned();
        if let (Some(pkg), Some(path)) = (rust_new, f.new_path.as_deref()) {
            let pdir = &ws.packages[pkg].dir;
            let rel = if pdir.is_empty() { path } else { &path[pdir.len() + 1..] };
            let src = new_src.read(path);
            match src.as_deref().map(syn::parse_file) {
                Some(Ok(file)) => {
                    let found = scan::items_at(&file, &scan::module_base(rel), &f.new_ranges);
                    record_items(&mut out.symbols, &mut out.items, &mut seen_items, ws, pkg, &item_path, path, found, "");
                    if cov_map.is_some() {
                        let lines = scan::loose_lines(&file, src.as_deref().unwrap_or_default(), &f.new_ranges);
                        loose.push(LooseFile { pkg, path: path.to_owned(), loose: lines });
                    }
                }
                Some(Err(e)) => {
                    let at = e.span().start();
                    let why = format!("does not parse ({}:{}: {e})", at.line, at.column + 1);
                    out.symbols.push(format!("{}: {path} (unparsable)", ws.packages[pkg].name));
                    whole_package(&mut out.whole, &mut out.escalations, pkg, path, why);
                }
                None => whole_package(&mut out.whole, &mut out.escalations, pkg, path, "cannot read the new side".into()),
            }
            proc_macro2::extra::invalidate_current_thread_spans();
        }
        if let (Some(pkg), Some(path)) = (rust_old, f.old_path.as_deref())
            && !f.old_ranges.is_empty()
        {
            if old_src.is_none() {
                old_src = Some(Source::at(root, &out.base)?);
            }
            let pdir = &ws.packages[pkg].dir;
            let rel = if pdir.is_empty() { path } else { &path[pdir.len() + 1..] };
            let src = old_src.as_mut().and_then(|s| s.read(path));
            if let Some(Ok(file)) = src.as_deref().map(syn::parse_file) {
                let found = scan::items_at(&file, &scan::module_base(rel), &f.old_ranges);
                let tag = if f.status == Status::Deleted { " (deleted file)" } else { " (old side)" };
                record_items(&mut out.symbols, &mut out.items, &mut seen_items, ws, pkg, &item_path, path, found, tag);
                if cov_map.is_some() {
                    let lines = scan::loose_lines(&file, src.as_deref().unwrap_or_default(), &f.old_ranges);
                    loose.push(LooseFile { pkg, path: path.to_owned(), loose: lines });
                }
            }
            proc_macro2::extra::invalidate_current_thread_spans();
        }
    }
    drop(old_src);
    laps.mark("path policy");

    // Every file of the target side, listed once with blob ids for the reference scan and
    // discovery.
    let mut listing: Option<Vec<(String, Option<String>)>> = None;
    match cfg.paths.outside {
        Outside::Full => {
            for path in outside {
                full_reasons.push(format!("{path} is outside every package"));
                out.escalations.push(Escalation { file: path, kind: "full", package: None, reason: "outside every package (paths.outside = \"full\")".into() });
            }
        }
        Outside::Ignore => out.outside_ignored = outside,
        Outside::Referenced if !outside.is_empty() => {
            let all = listing.insert(new_src.files(root)?);
            let finder = outside::Finder::new(&outside, &outside_gitlink);
            let index = outside::Index::new(&cfg.jev.cache_dir);
            // (package, listing index) of every Rust file of every package, each scanned once.
            let scanned: Vec<(usize, usize)> =
                (0..ws.packages.len()).flat_map(|pkg| rust_files(all, &ws.packages[pkg].dir).into_iter().map(move |i| (pkg, i))).collect();
            // Literals from the cache by blob id; the rest (and every working-tree file) read.
            let mut literals: Vec<Option<Vec<outside::Literal>>> =
                scanned.iter().map(|&(_, i)| all[i].1.as_deref().and_then(|id| index.get(id))).collect();
            let missing: Vec<usize> = (0..scanned.len()).filter(|&k| literals[k].is_none()).collect();
            let paths: Vec<String> = missing.iter().map(|&k| all[scanned[k].1].0.clone()).collect();
            for (&k, src) in missing.iter().zip(new_src.read_many(&paths)) {
                let Some(src) = src else { continue };
                let (key, cached) = match &all[scanned[k].1].1 {
                    Some(id) => (id.clone(), None),
                    None => {
                        let key = outside::content_key(&src);
                        let cached = index.get(&key);
                        (key, cached)
                    }
                };
                literals[k] = Some(cached.unwrap_or_else(|| {
                    let found = outside::literals(&src);
                    index.put(&key, &found);
                    found
                }));
            }
            // (package, outside file) → the first literal naming it, packages in order.
            let mut found: BTreeMap<(usize, usize), outside::Reference> = BTreeMap::new();
            for (&(pkg, i), lits) in scanned.iter().zip(&literals) {
                let Some(lits) = lits else { continue };
                for r in finder.scan(&all[i].0, lits) {
                    found.entry((pkg, r.file)).or_insert(r);
                }
            }
            let mut read = vec![false; outside.len()];
            for ((pkg, i), r) in found {
                read[i] = true;
                changed.insert(pkg);
                let why = format!("reads {} (\"{}\" at {})", outside[i], r.literal, r.at);
                whole_package(&mut out.whole, &mut out.escalations, pkg, &outside[i], why);
            }
            out.outside_ignored = outside.into_iter().zip(read).filter(|(_, r)| !r).map(|(p, _)| p).collect();
        }
        Outside::Referenced => {}
    }

    laps.mark("outside references");
    // Packages a fired rule names: run whole, and (unless `reach = false`) reach their reverse
    // dependencies as a changed package does.
    let mut seeds: BTreeSet<usize> = BTreeSet::new();
    for (index, files) in fired {
        let rule = &cfg.rules[index];
        out.rule_runs.extend(rule.run.iter().cloned());
        for name in &rule.packages {
            let pkg = ws.by_name(name).ok_or_else(|| format!("[[rule]] #{}: no workspace package `{name}`", index + 1))?;
            out.whole.entry(pkg).or_insert_with(|| format!("[[rule]] #{}", index + 1));
            if rule.reach {
                seeds.insert(pkg);
            }
        }
        if rule.full {
            full_reasons.push(format!("[[rule]] #{} (full = true) fired on {}", index + 1, files.join(", ")));
        }
        out.rules_fired.push(RuleFired { index, files });
    }
    out.rule_runs.sort_unstable();
    out.rule_runs.dedup();
    for (&pkg, whys) in &out.dependency {
        for why in whys {
            out.symbols.push(format!("{}: dependency: {why}", ws.packages[pkg].name));
        }
    }
    if !full_reasons.is_empty() {
        out.full = Some(full_reasons.join("; "));
    }

    // Layer 3: reach, from the changed packages and the packages rules name.
    let by_name = |a: &usize, b: &usize| ws.packages[*a].name.cmp(&ws.packages[*b].name);
    let mut changed: Vec<usize> = changed.into_iter().collect();
    changed.sort_unstable_by(by_name);
    let mut from = changed.clone();
    let mut extra: Vec<usize> = seeds.into_iter().filter(|p| !changed.contains(p)).collect();
    extra.sort_unstable_by(by_name);
    from.extend(extra);
    let mut reached = ws.affected(&from, sel.reach_depth);
    for &pkg in out.whole.keys() {
        if !reached.iter().any(|r| r.0 == pkg) {
            reached.push((pkg, 0));
        }
    }
    out.changed = changed;
    out.reached = reached;
    out.files = files;
    if out.full.is_some() {
        laps.mark("reach");
        out.timings = laps.laps;
        out.wall_ms = started.elapsed().as_millis();
        return Ok(out);
    }

    laps.mark("reach");

    // Layer 4: discovery.
    let ws = &out.ws;
    let new_ranges: HashMap<&str, &[Range]> =
        out.files.iter().filter_map(|f| Some((f.new_path.as_deref()?, f.new_ranges.as_slice()))).collect();
    // Every candidate file of every reached package, read in one batch, parsed on all cores.
    if listing.is_none() {
        listing = Some(new_src.files(root)?);
    }
    let all = listing.as_deref().unwrap_or_default();
    let mut owners: Vec<(usize, u32)> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    for &(pkg, depth) in &out.reached {
        for i in test_files(all, &ws.packages[pkg].dir) {
            owners.push((pkg, depth));
            paths.push(all[i].0.clone());
        }
    }
    let texts = new_src.read_many(&paths);
    let mut kept: Vec<(usize, u32)> = Vec::with_capacity(paths.len());
    let mut sources: Vec<(String, String)> = Vec::with_capacity(paths.len());
    for ((owner, path), text) in owners.into_iter().zip(paths).zip(texts) {
        if let Some(text) = text {
            kept.push(owner);
            sources.push((path, text));
        }
    }
    let owners = kept;
    // One parse per file serves discovery and, when it runs, static evidence.
    let want_evidence = sel.static_evidence != StaticEvidence::Off && !out.items.is_empty();
    let found = parse_parallel(&sources, |i, file| {
        let (path, src) = &sources[i];
        let pdir = ws.packages[owners[i].0].dir.as_str();
        let rel = if pdir.is_empty() { path.as_str() } else { &path[pdir.len() + 1..] };
        let (tests, ctx) = scan::tests_in(file, path, &scan::module_base(rel), src);
        (tests, ctx, want_evidence.then(|| evidence::local(file)))
    });
    // (file, module) → that module's context for stage-1 group questions.
    let mut contexts: HashMap<(String, String), String> = HashMap::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut locals: Vec<Option<evidence::LocalFile>> = Vec::with_capacity(sources.len());
    for (((pkg, depth), (path, _)), found) in owners.iter().copied().zip(&sources).zip(found) {
        let (tests, ctx, local) = match found {
            Ok(found) => found,
            Err(e) => {
                locals.push(None);
                whole_package(&mut out.whole, &mut out.escalations, pkg, path, format!("does not parse: {e}"));
                continue;
            }
        };
        locals.push(local);
        contexts.extend(ctx.into_iter().map(|(module, c)| ((path.clone(), module), c)));
        let ranges = new_ranges.get(path.as_str()).copied().unwrap_or_default();
        for test in tests {
            let must = overlaps(ranges, test.start, test.end).then_some(Must::Changed);
            candidates.push(Candidate {
                pkg,
                depth,
                test,
                must,
                evidence: None,
                coverage: None,
                gated: false,
                bonus: 0.0,
                group: None,
                screen: Screen::NotAsked,
                judge: Judge::NotAsked,
                nouls: [None; 2],
                ranks: [None; 2],
                score: None,
                reasons: Vec::new(),
                selected: false,
            });
        }
    }
    for c in &mut candidates {
        if c.must.is_none() && out.whole.contains_key(&c.pkg) {
            c.must = Some(Must::Package);
        }
    }

    laps.mark("discovery");

    // Layer 5: static evidence.
    if want_evidence {
        let files: Vec<SourceFile> = sources.iter().map(|(path, _)| SourceFile { path }).collect();
        let refs: Vec<TestRef> = candidates
            .iter()
            .map(|c| TestRef {
                path: c.test.file.clone(),
                start: c.test.start as usize,
                end: c.test.end as usize,
                name: c.test.name.clone(),
            })
            .collect();
        for ev in evidence::evidence(&files, locals, &out.items, &refs, sel.call_graph_depth) {
            let c = &mut candidates[ev.test];
            if c.must.is_none()
                && sel.static_evidence == StaticEvidence::Must
                && matches!(ev.kind, EvidenceKind::Direct | EvidenceKind::Helper)
            {
                c.must = Some(if ev.kind == EvidenceKind::Direct { Must::Direct } else { Must::Helper });
            }
            c.evidence = Some((ev.kind, ev.symbols));
        }
    }
    drop(sources);
    // A changed dependency is evidence for every test of the package that has none stronger.
    for c in &mut candidates {
        if c.evidence.is_none()
            && let Some(whys) = out.dependency.get(&c.pkg)
        {
            c.evidence = Some((EvidenceKind::Dependency, whys.clone()));
        }
    }

    laps.mark("static evidence");

    // Layer 5b: coverage map (gate / must / boost).
    if let Some(map) = &cov_map {
        coverage::apply(&mut out.coverage, map, &out.ws, &out.files, &out.items, &loose, &out.escalations, &mut candidates);
    }
    drop(cov_map);
    let cov_boost = out.coverage.used == CoveragePolicy::Boost;
    for c in &mut candidates {
        c.bonus = if c.must.is_some() {
            0.0
        } else if cov_boost && c.coverage.as_ref().is_some_and(|f| !f.is_empty()) {
            COVER_BOOST
        } else if c.evidence.as_ref().is_some_and(|(k, _)| is_static(*k)) {
            BOOST
        } else {
            0.0
        };
    }

    // Layers 6 and 7: Jev.
    let open: Vec<usize> = (0..candidates.len()).filter(|&i| candidates[i].must.is_none() && !candidates[i].gated).collect();
    let mut groups: Vec<Group> = Vec::new();
    {
        let mut group_of: HashMap<(&str, &str), usize> = HashMap::new();
        for &i in &open {
            let t = &candidates[i].test;
            let g = *group_of.entry((t.file.as_str(), t.module.as_str())).or_insert_with(|| {
                let key = (t.file.clone(), t.module.clone());
                let context = contexts.remove(&key).unwrap_or_default();
                groups.push(Group { pkg: candidates[i].pkg, file: key.0, module: key.1, members: Vec::new(), noul: None, context });
                groups.len() - 1
            });
            groups[g].members.push(i);
        }
    }
    for (g, group) in groups.iter().enumerate() {
        for &i in &group.members {
            candidates[i].group = Some(g);
        }
    }

    out.jev = if sw.no_jev {
        JevState::Off("--no-jev".into())
    } else {
        match cfg.jev.resolve_key() {
            Some((key, source)) => {
                let (url, model) = cfg.jev.endpoint(source);
                let key = (!sw.offline).then_some(key);
                let client = jev::Client::new(url, model, key, cfg.jev.timeout_secs, &cfg.jev.cache_dir);
                if !open.is_empty() {
                    judge_all(root, cfg, &mut out, &client, &mut candidates, &mut groups, &mut new_src)?;
                }
                JevState::On { key_source: Some(source), offline: sw.offline }
            }
            None if sw.offline => {
                let client = jev::Client::new(&cfg.jev.base_url, &cfg.jev.model, None, cfg.jev.timeout_secs, &cfg.jev.cache_dir);
                if !open.is_empty() {
                    judge_all(root, cfg, &mut out, &client, &mut candidates, &mut groups, &mut new_src)?;
                }
                JevState::On { key_source: None, offline: true }
            }
            None => JevState::Off("no API key".into()),
        }
    };

    laps.mark("jev");

    // Layer 8: policy.
    let failure = out.stages.iter().find_map(|(_, u)| u.failures.first().cloned());
    if let Some(what) = failure {
        match sel.on_jev_error {
            OnJevError::Fail => return Err(format!("Jev failed: {what} (on_jev_error = \"fail\")")),
            OnJevError::Full => {
                out.full = Some(format!("Jev failed: {what} (on_jev_error = \"full\")"));
            }
            OnJevError::Reach => {}
        }
        out.jev_error = Some(what);
    }
    // Per view, take the top `top_n`, but never more than `top_fraction` of the tests that view
    // ranked: a fixed 30 is a narrow cut of 1,000 judged tests and half of 60.
    let top = |view: usize| {
        let pool = candidates.iter().filter(|c| c.ranks[view].is_some()).count();
        sel.top_n.min((sel.top_fraction * pool as f64).ceil() as usize)
    };
    let top_n = [top(0), top(1)];
    let jev_off = matches!(out.jev, JevState::Off(_));
    for c in &mut candidates {
        if let Some(m) = c.must {
            c.reasons.push(m.as_str());
            c.selected = true;
            continue;
        }
        if c.gated {
            c.reasons.push("not-covered");
            continue;
        }
        if jev_off {
            match sel.without_jev {
                WithoutJev::Reach => c.reasons.push("reach"),
                WithoutJev::Evidence if c.evidence.is_some() => c.reasons.push("evidence"),
                WithoutJev::Evidence => c.reasons.push("no-evidence"),
            }
            c.selected = c.reasons[0] != "no-evidence";
            continue;
        }
        if out.jev_error.is_some() && sel.on_jev_error == OnJevError::Reach {
            c.reasons.push("jev-error");
            c.selected = true;
            continue;
        }
        match (c.screen, c.judge) {
            (Screen::Dropped, _) => c.reasons.push("screened-out"),
            (Screen::OverBudget, _) | (_, Judge::OverBudget) => c.reasons.push("unjudged"),
            (Screen::Uncached, _) | (_, Judge::Uncached) => c.reasons.push("uncached"),
            (Screen::Failed, _) | (_, Judge::Failed) => c.reasons.push("jev-failed"),
            (_, Judge::Judged) => {
                if c.ranks[0].is_some_and(|r| r <= top_n[0]) {
                    c.reasons.push("top-n:names");
                }
                if c.ranks[1].is_some_and(|r| r <= top_n[1]) {
                    c.reasons.push("top-n:body");
                }
                if c.score.is_some_and(|s| s >= sel.threshold) {
                    c.reasons.push("threshold");
                }
                if c.reasons.is_empty() {
                    c.reasons.push("below-cut");
                }
            }
            _ => c.reasons.push("unjudged"),
        }
        c.selected = !matches!(c.reasons[0], "screened-out" | "below-cut" | "jev-failed");
    }

    if sel.max_tests > 0 {
        let selected = candidates.iter().filter(|c| c.selected).count();
        if selected > sel.max_tests {
            let mut picks: Vec<usize> = (0..candidates.len()).filter(|&i| candidates[i].selected && candidates[i].must.is_none()).collect();
            picks.sort_by(|&a, &b| candidates[a].key(&groups).total_cmp(&candidates[b].key(&groups)).then(b.cmp(&a)));
            for &i in picks.iter().take(selected - sel.max_tests) {
                candidates[i].selected = false;
                candidates[i].reasons.push("max-tests");
            }
        }
    }
    if sel.min_tests > 0 {
        let selected = candidates.iter().filter(|c| c.selected).count();
        if selected < sel.min_tests {
            let mut rest: Vec<usize> = (0..candidates.len()).filter(|&i| !candidates[i].selected).collect();
            rest.sort_by(|&a, &b| candidates[b].key(&groups).total_cmp(&candidates[a].key(&groups)).then(a.cmp(&b)));
            for &i in rest.iter().take(sel.min_tests - selected) {
                candidates[i].selected = true;
                candidates[i].reasons.push("min-tests");
            }
        }
    }

    out.candidates = candidates;
    out.groups = groups;
    laps.mark("policy");
    out.timings = laps.laps;
    out.wall_ms = started.elapsed().as_millis();
    Ok(out)
}

/// Finds the path dependency (a crate in the repo that is not a member: a `[patch]` path, an
/// excluded crate) a file outside every member belongs to.
#[derive(Default)]
struct PathDeps {
    /// Directory → whether it holds a Cargo.toml (target side).
    manifests: HashMap<String, bool>,
    /// Crate directory → (package name, members depending on it); `None`: not a path dependency.
    crates: HashMap<String, Option<(String, Vec<usize>)>>,
    /// The target side's root Cargo.lock, read once.
    lock: Option<Option<String>>,
    /// `cargo metadata` path dependencies, loaded only when there is no lockfile.
    metadata: Option<Vec<PathDep>>,
}

impl PathDeps {
    /// The nearest ancestor directory of `path` holding a Cargo.toml names the package; the
    /// lockfile graph (else `cargo metadata` with dependencies) says which members depend on it.
    /// Returns (name, directory, members) when some member does.
    fn find(&mut self, root: &Path, ws: &Workspace, src: &mut Source, path: &str) -> Option<(String, String, Vec<usize>)> {
        let manifests = &mut self.manifests;
        let dir = path
            .rmatch_indices('/')
            .map(|(i, _)| &path[..i])
            .find(|dir| *manifests.entry((*dir).to_owned()).or_insert_with(|| src.read(&format!("{dir}/Cargo.toml")).is_some()))?;
        if !self.crates.contains_key(dir) {
            let found = self.lookup(root, ws, src, dir);
            self.crates.insert(dir.to_owned(), found);
        }
        let (name, members) = self.crates[dir].as_ref()?;
        (!members.is_empty()).then(|| (name.clone(), dir.to_owned(), members.clone()))
    }

    fn lookup(&mut self, root: &Path, ws: &Workspace, src: &mut Source, dir: &str) -> Option<(String, Vec<usize>)> {
        let manifest: toml::Table = src.read(&format!("{dir}/Cargo.toml"))?.parse().ok()?;
        let name = manifest.get("package")?.get("name")?.as_str()?.to_owned();
        let members = match self.lock.get_or_insert_with(|| src.read("Cargo.lock")) {
            Some(lock) => manifest::path_dependents(lock, &name, ws)?,
            None => {
                let deps = self.metadata.get_or_insert_with(|| {
                    ws.path_deps(root).unwrap_or_else(|e| {
                        eprintln!("jevtest: {e}; path dependencies count as outside every package");
                        Vec::new()
                    })
                });
                deps.iter().find(|d| d.dir == dir)?.dependents.clone()
            }
        };
        Some((name, members))
    }
}

/// Marks every test of `pkg` as must (first reason wins) and records the escalation.
fn whole_package(whole: &mut BTreeMap<usize, String>, escalations: &mut Vec<Escalation>, pkg: usize, file: &str, why: String) {
    if let std::collections::btree_map::Entry::Vacant(slot) = whole.entry(pkg) {
        slot.insert(why.clone());
        escalations.push(Escalation { file: file.into(), kind: "whole", package: Some(pkg), reason: why });
    }
}

#[allow(clippy::too_many_arguments)]
fn record_items(
    symbols: &mut Vec<String>,
    items: &mut Vec<ChangedItem>,
    seen: &mut HashSet<(String, String)>,
    ws: &Workspace,
    pkg: usize,
    item_path: &str,
    path: &str,
    found: Vec<Option<scan::Named>>,
    tag: &str,
) {
    let pname = &ws.packages[pkg].name;
    for item in found {
        let Some(item) = item else {
            if seen.insert((pname.clone(), format!("{path} (top level)"))) {
                symbols.push(format!("{pname}: {path} (top level){tag}"));
            }
            continue;
        };
        if !seen.insert((pname.clone(), item.path.clone())) {
            continue;
        }
        symbols.push(format!("{pname}: {}{tag}", item.path));
        if !matches!(item.kind, scan::Kind::Test | scan::Kind::Mod) {
            items.push(ChangedItem { path: item_path.to_owned(), name: item.name, owner: item.owner, kind: item.kind });
        }
    }
}

/// Stage 1 (screening groups) and stage 2 (judging tests in every configured view).
fn judge_all(
    root: &Path,
    cfg: &Config,
    out: &mut Selection,
    client: &jev::Client,
    candidates: &mut [Candidate],
    groups: &mut [Group],
    src: &mut Source,
) -> Result<(), String> {
    let j = &cfg.jev;
    let ws = &out.ws;
    let diff = git::context_diff(root, &out.base, &out.target, &out.files, src)?;
    let cut = jev::truncate(&diff, j.max_state_chars);
    let diff = if cut.len() < diff.len() { format!("{cut}\n[diff truncated]") } else { diff };
    let state = json!({"change": {
        "changed_packages": out.changed.iter().map(|&p| ws.packages[p].name.as_str()).collect::<Vec<_>>(),
        "changed_symbols": out.symbols,
        "diff": diff,
    }});
    let mut budget = j.max_questions;

    // Stage 1: one question per multi-test group without static evidence.
    let mut asked: Vec<usize> = Vec::new();
    let mut questions: Vec<Value> = Vec::new();
    for (g, group) in groups.iter().enumerate() {
        let screen = if group.members.iter().any(|&i| candidates[i].bonus > 0.0) {
            Screen::KeptEvidence
        } else if group.members.len() == 1 {
            Screen::Single
        } else if budget == 0 {
            Screen::OverBudget
        } else {
            budget -= 1;
            asked.push(g);
            let names: Vec<&str> = group.members.iter().map(|&i| candidates[i].test.name.as_str()).collect();
            let context = jev::truncate(&group.context, j.max_group_chars);
            questions.push(jev::group_question(&ws.packages[group.pkg].name, &group.file, &group.module, &names, context));
            Screen::NotAsked
        };
        for &i in &group.members {
            candidates[i].screen = screen;
        }
    }
    if !questions.is_empty() {
        let (answers, usage) = client.judge(&state, &questions, j.batch, j.concurrency);
        for (&g, answer) in asked.iter().zip(answers) {
            let screen = match answer {
                Answer::Noul(n) => {
                    groups[g].noul = Some(n);
                    if n < cfg.select.group_threshold { Screen::Dropped } else { Screen::Kept }
                }
                Answer::Failed => Screen::Failed,
                Answer::Uncached => Screen::Uncached,
            };
            for &i in &groups[g].members {
                candidates[i].screen = screen;
            }
        }
        let failed = !usage.failures.is_empty();
        out.stages.push(("screen", usage));
        if failed {
            // Selection falls back to `on_jev_error`; judging would only fail the same way.
            return Ok(());
        }
    }

    // Stage 2: every surviving test, one question per view, all views in the same requests.
    let views: Vec<usize> = VIEWS.iter().enumerate().filter(|(_, v)| j.views.iter().any(|w| w == *v)).map(|(i, _)| i).collect();
    let mut judged: Vec<usize> = Vec::new();
    let mut questions: Vec<Value> = Vec::new();
    for (i, c) in candidates.iter_mut().enumerate() {
        if !matches!(c.screen, Screen::KeptEvidence | Screen::Single | Screen::Kept) {
            continue;
        }
        if budget < views.len() {
            c.judge = Judge::OverBudget;
            continue;
        }
        budget -= views.len();
        judged.push(i);
        let package = &ws.packages[c.pkg].name;
        for &v in &views {
            questions.push(if v == 0 {
                jev::names_question(package, &c.test)
            } else {
                jev::body_question(package, &c.test, j.max_test_chars)
            });
        }
    }
    if questions.is_empty() {
        return Ok(());
    }
    let (answers, usage) = client.judge(&state, &questions, j.batch, j.concurrency);
    for (&i, answers) in judged.iter().zip(answers.chunks(views.len())) {
        let c = &mut candidates[i];
        for (&v, a) in views.iter().zip(answers) {
            if let Answer::Noul(n) = a {
                c.nouls[v] = Some(*n);
            }
        }
        c.judge = if c.nouls.iter().any(Option::is_some) {
            Judge::Judged
        } else if answers.contains(&Answer::Failed) {
            Judge::Failed
        } else {
            Judge::Uncached
        };
        let boost = c.boost();
        c.score = c.nouls.iter().flatten().copied().reduce(f64::max).map(|s| (s + boost).min(1.0));
    }
    out.stages.push(("judge", usage));

    for &v in &views {
        let mut order: Vec<usize> = judged.iter().copied().filter(|&i| candidates[i].nouls[v].is_some()).collect();
        let key = |i: usize| (candidates[i].nouls[v].unwrap_or(0.0) + candidates[i].boost()).min(1.0);
        order.sort_by(|&a, &b| key(b).total_cmp(&key(a)).then(a.cmp(&b)));
        for (rank, &i) in order.iter().enumerate() {
            candidates[i].ranks[v] = Some(rank + 1);
        }
    }
    Ok(())
}

/// Parses every `(path, source)` with syn on all cores and runs `f(index, file)` on each tree;
/// a file that does not parse yields its error. Results keep the input order.
fn parse_parallel<T: Send>(sources: &[(String, String)], f: impl Fn(usize, &syn::File) -> T + Sync) -> Vec<Result<T, String>> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(sources.len()).max(1);
    let mut out: Vec<Option<Result<T, String>>> = sources.iter().map(|_| None).collect();
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                // syn and the visitors recurse once per nesting level; give deep expressions room.
                std::thread::Builder::new()
                    .stack_size(32 << 20)
                    .spawn_scoped(s, || {
                        let mut done = Vec::new();
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some((_, src)) = sources.get(i) else { break };
                            let r = syn::parse_file(src).map(|file| f(i, &file)).map_err(|e| e.to_string());
                            proc_macro2::extra::invalidate_current_thread_spans();
                            done.push((i, r));
                        }
                        done
                    })
                    .expect("spawn parser thread")
            })
            .collect();
        for w in workers {
            for (i, r) in w.join().unwrap_or_else(|e| std::panic::resume_unwind(e)) {
                out[i] = Some(r);
            }
        }
    });
    out.into_iter().map(|r| r.unwrap_or_else(|| Err("not parsed".into()))).collect()
}

/// Listing indices of the `.rs` files under package dir `pdir` that may hold its tests.
fn test_files(all: &[(String, Option<String>)], pdir: &str) -> Vec<usize> {
    package_rs(all, pdir, false)
}

/// Listing indices of every `.rs` file of the package at `pdir` (build.rs, benches and examples too).
fn rust_files(all: &[(String, Option<String>)], pdir: &str) -> Vec<usize> {
    package_rs(all, pdir, true)
}

/// `.rs` files under `pdir`, minus `target/` and nested packages; benches and examples only with
/// `all_targets`.
fn package_rs(all: &[(String, Option<String>)], pdir: &str, all_targets: bool) -> Vec<usize> {
    fn rel<'p>(p: &'p str, pdir: &str) -> Option<&'p str> {
        if pdir.is_empty() { Some(p) } else { p.strip_prefix(pdir)?.strip_prefix('/') }
    }
    // Subdirectories (relative to the package) holding their own Cargo.toml.
    let nested: HashSet<&str> = all.iter().filter_map(|(p, _)| rel(p, pdir)?.strip_suffix("/Cargo.toml")).collect();
    (0..all.len())
        .filter(|&i| {
            let p = &all[i].0;
            if !p.ends_with(".rs") {
                return false;
            }
            let Some(r) = rel(p, pdir) else { return false };
            if (!all_targets && (r.starts_with("benches/") || r.starts_with("examples/"))) || r.split('/').any(|c| c == "target") {
                return false;
            }
            !r.match_indices('/').any(|(i, _)| nested.contains(&r[..i]))
        })
        .collect()
}
