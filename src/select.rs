//! The selection pipeline: intake, path policy, reach, discovery, static evidence, Jev screening
//! and judging, and the selection policy. Every layer's verdict is kept per candidate test.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde_json::{Value, json};

use crate::config::{Config, NonRust, OnJevError, StaticEvidence, WithoutJev};
use crate::evidence::{self, ChangedItem, EvidenceKind, SourceFile, TestRef};
use crate::git::{self, FileChange, Range, Source, Status, Target};
use crate::jev::{self, Answer, Usage};
use crate::scan::{self, TestFn};
use crate::workspace::Workspace;

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
}

impl Must {
    pub fn as_str(self) -> &'static str {
        match self {
            Must::Changed => "changed",
            Must::Package => "package",
            Must::Direct => "direct",
            Must::Helper => "helper",
        }
    }
}

pub fn evidence_kind(kind: EvidenceKind) -> String {
    match kind {
        EvidenceKind::Direct => "direct".into(),
        EvidenceKind::Helper => "helper".into(),
        EvidenceKind::Transitive(d) => format!("transitive({d})"),
    }
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
        if self.evidence.is_some() && self.must.is_none() { BOOST } else { 0.0 }
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
    /// `full` (whole workspace) or `whole` (one package).
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
    pub escalations: Vec<Escalation>,
    pub rules_fired: Vec<RuleFired>,
    /// Changed packages, by name.
    pub changed: Vec<usize>,
    /// (package, reverse-dependency hops); rule packages appear with 0.
    pub reached: Vec<(usize, u32)>,
    /// `pkg: module::Item` lines given to Jev and the report.
    pub symbols: Vec<String>,
    pub items: Vec<ChangedItem>,
    /// Packages whose every test must run → why.
    pub whole: BTreeMap<usize, String>,
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
    pub wall_ms: u128,
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

pub fn run(root: &Path, cfg: &Config, scope: crate::changes::Scope, sw: &Switches) -> Result<Selection, String> {
    let started = Instant::now();
    let sel = &cfg.select;
    let base = scope.base.clone();
    let target = scope.target.clone();

    // Layer 1: intake, narrowed to `changes.files` when given.
    let mut files = git::changed_files(root, &base, &target, cfg.changes.include_untracked)?;
    if !cfg.changes.files.is_empty() {
        files = git::restrict(root, files, &cfg.changes.files, &target)?;
    }
    let ws = Workspace::load(root)?;
    let full_run = globset(&cfg.paths.full_run)?;
    let ignore = globset(&cfg.paths.ignore)?;
    let rule_sets: Vec<GlobSet> = cfg.rules.iter().map(|r| globset(&r.when)).collect::<Result<_, _>>()?;
    let mut new_src = Source::new(root, &target)?;
    let mut old_src: Option<Source> = None;

    let mut out = Selection {
        base,
        target,
        scope,
        files: Vec::new(),
        ws,
        ignored: Vec::new(),
        escalations: Vec::new(),
        rules_fired: Vec::new(),
        changed: Vec::new(),
        reached: Vec::new(),
        symbols: Vec::new(),
        items: Vec::new(),
        whole: BTreeMap::new(),
        candidates: Vec::new(),
        groups: Vec::new(),
        stages: Vec::new(),
        jev: JevState::Off(String::new()),
        jev_error: None,
        full: None,
        rule_runs: Vec::new(),
        wall_ms: 0,
    };
    let ws = &out.ws;

    // Layer 2: path policy, plus changed items from both sides of every changed `.rs` file.
    let mut changed: BTreeSet<usize> = BTreeSet::new();
    let mut fired: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut full_reasons: Vec<String> = Vec::new();
    let mut seen_items: HashSet<(String, String)> = HashSet::new();
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
            let mut by_rule = false;
            for (i, set) in rule_sets.iter().enumerate() {
                if !set.is_match(path) {
                    continue;
                }
                by_rule = true;
                fired.entry(i).or_default().push(path.to_owned());
            }
            if by_rule {
                continue;
            }
            if ignore.is_match(path) {
                out.ignored.push(path.to_owned());
                continue;
            }
            let Some(pkg) = ws.owner(path) else {
                full_reasons.push(format!("{path} is outside every package"));
                out.escalations.push(Escalation { file: path.into(), kind: "full", package: None, reason: "outside every package".into() });
                continue;
            };
            changed.insert(pkg);
            let pdir = &ws.packages[pkg].dir;
            let rel = if pdir.is_empty() { path } else { &path[pdir.len() + 1..] };
            let whole_why = if rel == "Cargo.toml" {
                Some("manifest changed")
            } else if rel == "build.rs" {
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
            match new_src.read(path).map(|src| syn::parse_file(&src)) {
                Some(Ok(file)) => {
                    let found = scan::items_at(&file, &scan::module_base(rel), &f.new_ranges);
                    record_items(&mut out.symbols, &mut out.items, &mut seen_items, ws, pkg, &item_path, path, found, "");
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
            if let Some(Ok(file)) = old_src.as_mut().and_then(|s| s.read(path)).map(|src| syn::parse_file(&src)) {
                let found = scan::items_at(&file, &scan::module_base(rel), &f.old_ranges);
                let tag = if f.status == Status::Deleted { " (deleted file)" } else { " (old side)" };
                record_items(&mut out.symbols, &mut out.items, &mut seen_items, ws, pkg, &item_path, path, found, tag);
            }
            proc_macro2::extra::invalidate_current_thread_spans();
        }
    }
    drop(old_src);

    for (index, files) in fired {
        let rule = &cfg.rules[index];
        out.rule_runs.extend(rule.run.iter().cloned());
        for name in &rule.packages {
            let pkg = ws.by_name(name).ok_or_else(|| format!("[[rule]] #{}: no workspace package `{name}`", index + 1))?;
            out.whole.entry(pkg).or_insert_with(|| format!("[[rule]] #{}", index + 1));
        }
        if rule.full {
            full_reasons.push(format!("[[rule]] #{} (full = true) fired on {}", index + 1, files.join(", ")));
        }
        out.rules_fired.push(RuleFired { index, files });
    }
    out.rule_runs.dedup();
    if !full_reasons.is_empty() {
        out.full = Some(full_reasons.join("; "));
    }

    // Layer 3: reach.
    let mut changed: Vec<usize> = changed.into_iter().collect();
    changed.sort_unstable_by(|&a, &b| ws.packages[a].name.cmp(&ws.packages[b].name));
    let mut reached = ws.affected(&changed, sel.reach_depth);
    for &pkg in out.whole.keys() {
        if !reached.iter().any(|r| r.0 == pkg) {
            reached.push((pkg, 0));
        }
    }
    out.changed = changed;
    out.reached = reached;
    out.files = files;
    if out.full.is_some() {
        out.wall_ms = started.elapsed().as_millis();
        return Ok(out);
    }

    // Layer 4: discovery.
    let ws = &out.ws;
    let new_ranges: HashMap<&str, &[Range]> =
        out.files.iter().filter_map(|f| Some((f.new_path.as_deref()?, f.new_ranges.as_slice()))).collect();
    let mut sources: Vec<(String, String)> = Vec::new();
    // (file, module) → that module's context for stage-1 group questions.
    let mut contexts: HashMap<(String, String), String> = HashMap::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    for &(pkg, depth) in &out.reached {
        let pdir = ws.packages[pkg].dir.as_str();
        for path in test_files(&new_src.list(root, pdir)?, pdir) {
            let rel = if pdir.is_empty() { path.as_str() } else { &path[pdir.len() + 1..] };
            let Some(src) = new_src.read(&path) else { continue };
            match syn::parse_file(&src) {
                Ok(file) => {
                    let (tests, ctx) = scan::tests_in(&file, &path, &scan::module_base(rel), &src);
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
                Err(e) => whole_package(&mut out.whole, &mut out.escalations, pkg, &path, format!("does not parse: {e}")),
            }
            proc_macro2::extra::invalidate_current_thread_spans();
            sources.push((path, src));
        }
    }
    for c in &mut candidates {
        if c.must.is_none() && out.whole.contains_key(&c.pkg) {
            c.must = Some(Must::Package);
        }
    }

    // Layer 5: static evidence.
    if sel.static_evidence != StaticEvidence::Off && !out.items.is_empty() {
        let files: Vec<SourceFile> = sources
            .iter()
            .map(|(path, src)| SourceFile { path, source: src })
            .collect();
        let refs: Vec<TestRef> = candidates
            .iter()
            .map(|c| TestRef {
                path: c.test.file.clone(),
                start: c.test.start as usize,
                end: c.test.end as usize,
                name: c.test.name.clone(),
            })
            .collect();
        for ev in evidence::evidence(&files, &out.items, &refs, sel.call_graph_depth) {
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

    // Layers 6 and 7: Jev.
    let open: Vec<usize> = (0..candidates.len()).filter(|&i| candidates[i].must.is_none()).collect();
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
    out.wall_ms = started.elapsed().as_millis();
    Ok(out)
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
        if !item.container_or_test {
            items.push(ChangedItem { path: item_path.to_owned(), name: item.name, owner: item.owner });
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
        let screen = if group.members.iter().any(|&i| candidates[i].evidence.is_some()) {
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

/// Repo-relative `.rs` files under package dir `pdir` that may hold its tests.
fn test_files(all: &[String], pdir: &str) -> Vec<String> {
    let rel = |p: &str| -> Option<String> {
        if pdir.is_empty() { Some(p.to_owned()) } else { p.strip_prefix(pdir)?.strip_prefix('/').map(str::to_owned) }
    };
    // Subdirectories (relative to the package) holding their own Cargo.toml.
    let nested: HashSet<String> = all
        .iter()
        .filter_map(|p| rel(p))
        .filter_map(|r| r.strip_suffix("/Cargo.toml").map(str::to_owned))
        .collect();
    all.iter()
        .filter(|p| p.ends_with(".rs"))
        .filter(|p| {
            let Some(r) = rel(p) else { return false };
            if r.starts_with("benches/") || r.starts_with("examples/") || r.split('/').any(|c| c == "target") {
                return false;
            }
            !r.match_indices('/').any(|(i, _)| nested.contains(&r[..i]))
        })
        .cloned()
        .collect()
}
