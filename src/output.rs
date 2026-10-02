//! Layer 9: runner commands, the nextest filter, the stderr summary, the JSON report and `explain`.

use std::fmt::Write as _;

use serde_json::{Value, json};

use crate::config::{Config, CoveragePolicy, OnJevError, Runner, WithoutJev};
use crate::jev::Usage;
use crate::select::{BOOST, Candidate, JevState, Judge, Must, Screen, Selection, VIEWS, evidence_kind, is_static};

/// Whether the coverage map says `c` executes a changed function.
fn covers(c: &Candidate) -> bool {
    c.coverage.as_ref().is_some_and(|f| !f.is_empty())
}

/// The summary's one `coverage:` line.
fn coverage_line(s: &Selection) -> String {
    let cv = &s.coverage;
    let Some(map) = &cv.map else {
        return format!("coverage: {}", cv.reason.as_deref().unwrap_or("off"));
    };
    let commit = cv.commit.as_deref().map_or("", |c| &c[..c.len().min(12)]);
    let age = cv.age_commits.map_or_else(|| "not in HEAD's history".to_owned(), |n| format!("{} behind the base", crate::coverage::n_commits(n)));
    let mut line = format!("coverage: {map} @{commit} ({age}), policy {}: {} covered", cv.used.as_str(), cv.covered);
    let _ = match cv.used {
        CoveragePolicy::Gate => write!(line, ", {} gated out (not-covered)", cv.gated_out),
        CoveragePolicy::Must => write!(line, ", {} must (covered)", count(s, |c| c.must == Some(Must::Covered))),
        CoveragePolicy::Boost => write!(line, ", {} boosted +{}", count(s, |c| c.must.is_none() && covers(c)), crate::coverage::COVER_BOOST),
        CoveragePolicy::Off => Ok(()),
    };
    let _ = write!(line, ", {} unknown to the map", cv.unknown);
    if let Some(b) = cv.blind.first() {
        let more = if cv.blind.len() > 1 { format!(" (+{} more)", cv.blind.len() - 1) } else { String::new() };
        let _ = write!(line, "; blind: {} — {}{more}", b.item, b.why);
    }
    if let Some(r) = &cv.reason {
        let _ = write!(line, "; {r}");
    }
    line
}

/// The report's top-level `coverage` object.
fn coverage_json(s: &Selection) -> Value {
    let cv = &s.coverage;
    let name = |p: usize| s.ws.packages[p].name.as_str();
    json!({
        "map": cv.map,
        "commit": cv.commit,
        "age_commits": cv.age_commits,
        "policy_requested": cv.requested.as_str(),
        "policy_used": cv.used.as_str(),
        "covered": cv.covered,
        "gated_out": cv.gated_out,
        "unknown": cv.unknown,
        "blind_items": cv.blind.iter().map(|b| json!({"package": name(b.package), "item": b.item, "why": b.why})).collect::<Vec<_>>(),
        "no_gate_packages": cv.no_gate.iter().map(|&p| name(p)).collect::<Vec<_>>(),
        "reason": cv.reason,
    })
}

/// What to run.
pub struct Plan {
    /// The nextest filterset (`None` when nothing narrows: full run without quarantine, or nothing to run).
    pub filter: Option<String>,
    /// One command per line; empty = nothing worth running.
    pub commands: Vec<Vec<String>>,
    /// Things the chosen runner cannot express.
    pub notes: Vec<String>,
}

/// Package names a filterset names with `package(=NAME)`; `None` when it names none (it may match
/// any package).
fn filterset_packages(set: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = set;
    while let Some(at) = rest.find("package(") {
        rest = &rest[at + "package(".len()..];
        let end = rest.find(')').unwrap_or(rest.len());
        let name = rest[..end].trim().trim_start_matches('=').trim();
        if !name.is_empty() {
            out.push(name.to_owned());
        }
    }
    (!out.is_empty()).then_some(out)
}

fn names_term(package: &str, names: &[&str]) -> String {
    format!("package(={package}) & test(/(^|::)({})($|::)/)", names.join("|"))
}

pub fn plan(s: &Selection, cfg: &Config, runner: Runner, extra: &[String]) -> Plan {
    let ws = &s.ws;
    let never = &cfg.tests.never;
    let extras: Vec<&String> = s.rule_runs.iter().chain(&cfg.tests.always).collect();
    let mut notes = Vec::new();

    // The nextest expression.
    let mut terms: Vec<String> = Vec::new();
    let mut changed_terms: Vec<String> = Vec::new();
    let mut pkgs: Vec<&str> = Vec::new();
    if s.full.is_none() {
        for &(pkg, _) in &s.reached {
            let name = ws.packages[pkg].name.as_str();
            if s.whole.contains_key(&pkg) {
                terms.push(format!("package(={name})"));
                pkgs.push(name);
                continue;
            }
            let mut names: Vec<&str> =
                s.candidates.iter().filter(|c| c.pkg == pkg && c.selected).map(|c| c.test.name.as_str()).collect();
            if names.is_empty() {
                continue;
            }
            names.sort_unstable();
            names.dedup();
            terms.push(names_term(name, &names));
            pkgs.push(name);
            let mut changed: Vec<&str> = s
                .candidates
                .iter()
                .filter(|c| c.pkg == pkg && c.must == Some(Must::Changed))
                .map(|c| c.test.name.as_str())
                .collect();
            changed.sort_unstable();
            changed.dedup();
            if !changed.is_empty() {
                changed_terms.push(names_term(name, &changed));
            }
        }
    }
    let wrap = |t: &str| format!("({t})");
    let mut parts: Vec<String> = Vec::new();
    if s.full.is_some() {
        // Everything runs; only the quarantine narrows.
        if !never.is_empty() {
            parts.push(format!("all() & not ({})", never.join(" | ")));
        }
    } else {
        if !terms.is_empty() {
            let selected = terms.iter().map(|t| wrap(t)).collect::<Vec<_>>().join(" | ");
            if never.is_empty() {
                parts.push(selected);
            } else {
                parts.push(format!("(({selected}) & not ({}))", never.join(" | ")));
                parts.extend(changed_terms.iter().map(|t| wrap(t)));
            }
        }
        parts.extend(extras.iter().map(|t| wrap(t)));
    }
    let filter = (!parts.is_empty()).then(|| parts.join(" | "));

    let mut commands: Vec<Vec<String>> = Vec::new();
    let cmd = |args: &[&str]| -> Vec<String> { args.iter().map(|a| (*a).to_owned()).collect() };
    match runner {
        Runner::Nextest | Runner::Auto => {
            if s.full.is_some() {
                let mut c = cmd(&["cargo", "nextest", "run", "--workspace"]);
                if let Some(f) = &filter {
                    c.extend(["-E".to_owned(), f.clone()]);
                }
                c.extend(extra.iter().cloned());
                commands.push(c);
            } else if let Some(f) = &filter {
                let mut c = cmd(&["cargo", "nextest", "run"]);
                let mut all: Vec<String> = pkgs.iter().map(|p| (*p).to_owned()).collect();
                let mut workspace = false;
                for set in &extras {
                    match filterset_packages(set) {
                        Some(names) => all.extend(names),
                        None => workspace = true,
                    }
                }
                if workspace {
                    c.push("--workspace".into());
                } else {
                    let mut seen = std::collections::HashSet::new();
                    for p in all {
                        if seen.insert(p.clone()) {
                            c.extend(["-p".to_owned(), p]);
                        }
                    }
                }
                c.extend(["-E".to_owned(), f.clone()]);
                c.extend(extra.iter().cloned());
                commands.push(c);
            }
        }
        Runner::Cargo => {
            if !never.is_empty() {
                notes.push(format!("cargo runner ignores tests.never ({}); use nextest to quarantine", never.join(", ")));
            }
            if s.full.is_some() {
                let mut c = cmd(&["cargo", "test", "--workspace"]);
                c.extend(extra.iter().cloned());
                commands.push(c);
            } else {
                let mut whole: Vec<String> = Vec::new();
                for set in &extras {
                    match filterset_packages(set) {
                        Some(names) => whole.extend(names),
                        None => notes.push(format!("cargo runner cannot express filterset `{set}`; use nextest")),
                    }
                }
                let mut done = std::collections::HashSet::new();
                for &(pkg, _) in &s.reached {
                    let name = ws.packages[pkg].name.clone();
                    let is_whole = s.whole.contains_key(&pkg) || whole.contains(&name);
                    let mut filters: Vec<String> = s
                        .candidates
                        .iter()
                        .filter(|c| c.pkg == pkg && c.selected)
                        .map(|c| if c.test.module.is_empty() { c.test.name.clone() } else { format!("{}::{}", c.test.module, c.test.name) })
                        .collect();
                    if !is_whole && filters.is_empty() {
                        continue;
                    }
                    done.insert(name.clone());
                    let mut c = cmd(&["cargo", "test", "-p"]);
                    c.push(name);
                    c.extend(extra.iter().cloned());
                    if !is_whole {
                        filters.sort_unstable();
                        filters.dedup();
                        c.push("--".into());
                        c.extend(filters);
                    }
                    commands.push(c);
                }
                for name in whole {
                    if done.insert(name.clone()) {
                        let mut c = cmd(&["cargo", "test", "-p"]);
                        c.push(name);
                        c.extend(extra.iter().cloned());
                        commands.push(c);
                    }
                }
            }
        }
    }
    Plan { filter, commands, notes }
}

pub fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-./=:,@%+".contains(&b));
    if plain { s.to_owned() } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

pub fn command_line(c: &[String]) -> String {
    c.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

fn count(s: &Selection, f: impl Fn(&Candidate) -> bool) -> usize {
    s.candidates.iter().filter(|c| f(c)).count()
}

fn has_reason(c: &Candidate, r: &str) -> bool {
    c.reasons.contains(&r)
}

/// One stderr line when Jev could not judge (no key, `--no-jev`, or a failure), naming the cause
/// and the fallback selection.
pub fn jev_notice(s: &Selection, cfg: &Config) -> Option<String> {
    if let Some(e) = &s.jev_error {
        let fallback = match cfg.select.on_jev_error {
            OnJevError::Full => "running the full suite (on_jev_error = full)",
            _ => "selecting every reached test (on_jev_error = reach)",
        };
        return Some(format!("jevtest: Jev failed: {e}; {fallback}"));
    }
    let JevState::Off(why) = &s.jev else { return None };
    let fallback = match cfg.select.without_jev {
        WithoutJev::Reach => "selecting every reached test (without_jev = reach)",
        WithoutJev::Evidence => "selecting must-runs and tests with static evidence (without_jev = evidence)",
    };
    Some(if why == "no API key" {
        format!(
            "jevtest: Jev off: no API key (set {} or write it to {}); {fallback}",
            cfg.jev.key_env.join(" or "),
            cfg.jev.key_file.display()
        )
    } else {
        format!("jevtest: Jev off ({why}); {fallback}")
    })
}

/// The per-layer summary printed to stderr.
pub fn summary(s: &Selection, cfg: &Config, profile: Option<&str>, plan: &Plan, verbose: bool) -> String {
    let ws = &s.ws;
    let name = |p: usize| ws.packages[p].name.as_str();
    let mut o = String::new();
    let config = cfg.source.as_ref().map_or_else(|| "defaults".to_owned(), |p| p.display().to_string());
    let _ = writeln!(
        o,
        "jevtest {}: {} [{}..{}]  (config {config}{})",
        env!("CARGO_PKG_VERSION"),
        s.scope.what,
        &s.base[..s.base.len().min(12)],
        s.target.label(),
        profile.map(|p| format!(", profile {p}")).unwrap_or_default()
    );
    let rs = s.files.iter().filter(|f| f.path().ends_with(".rs")).count();
    let _ = writeln!(o, "intake: {} changed files ({rs} .rs), {} changed items", s.files.len(), s.items.len());
    if verbose {
        for f in &s.files {
            match (&f.old_path, &f.new_path) {
                (Some(a), Some(b)) if a != b => {
                    let _ = writeln!(o, "  {:<9} {a} -> {b}", f.status.as_str());
                }
                _ => {
                    let _ = writeln!(o, "  {:<9} {}", f.status.as_str(), f.path());
                }
            }
        }
        for sym in &s.symbols {
            let _ = writeln!(o, "  item {sym}");
        }
    }
    if !s.ignored.is_empty() {
        let _ = writeln!(o, "path policy: {} ignored", s.ignored.len());
    }
    for e in &s.escalations {
        let pkg = e.package.map(|p| format!(" [{}]", name(p))).unwrap_or_default();
        let _ = writeln!(o, "path policy: {}{pkg}: {} ({})", e.kind, e.file, e.reason);
    }
    for r in &s.rules_fired {
        let _ = writeln!(o, "path policy: [[rule]] #{} fired on {}", r.index + 1, r.files.join(", "));
    }
    let reached: Vec<String> =
        s.reached.iter().map(|&(p, d)| if d == 0 { name(p).to_owned() } else { format!("{}+{d}", name(p)) }).collect();
    let _ = writeln!(
        o,
        "reach: changed {} [{}]; reached {} [{}]",
        s.changed.len(),
        s.changed.iter().map(|&p| name(p)).collect::<Vec<_>>().join(", "),
        s.reached.len(),
        reached.join(", ")
    );
    if let Some(why) = &s.full {
        let _ = writeln!(o, "FULL RUN: {why}");
    } else {
        let must = |m: Must| count(s, |c| c.must == Some(m));
        let _ = writeln!(o, "discovery: {} tests", s.candidates.len());
        let _ = writeln!(
            o,
            "static: must {} (changed {}, package {}, direct {}, helper {}); evidence on {} (direct {}, helper {}, transitive {}), boosted {}",
            count(s, |c| c.must.is_some_and(|m| m != Must::Covered)),
            must(Must::Changed),
            must(Must::Package),
            must(Must::Direct),
            must(Must::Helper),
            count(s, |c| c.evidence.as_ref().is_some_and(|(k, _)| is_static(*k))),
            count(s, |c| matches!(&c.evidence, Some((crate::evidence::EvidenceKind::Direct, _)))),
            count(s, |c| matches!(&c.evidence, Some((crate::evidence::EvidenceKind::Helper, _)))),
            count(s, |c| matches!(&c.evidence, Some((crate::evidence::EvidenceKind::Transitive(_), _)))),
            count(s, |c| c.bonus > 0.0 && c.evidence.as_ref().is_some_and(|(k, _)| is_static(*k))),
        );
        let _ = writeln!(o, "{}", coverage_line(s));
        match &s.jev {
            JevState::Off(_) => {
                if let Some(n) = jev_notice(s, cfg) {
                    let _ = writeln!(o, "{n}");
                }
            }
            JevState::On { key_source, offline } => {
                let groups_asked = s.stages.iter().find(|(n, _)| *n == "screen").map_or(0, |(_, u)| u.asked);
                let groups_dropped = s.groups.iter().filter(|g| g.noul.is_some_and(|n| n < cfg.select.group_threshold)).count();
                let _ = writeln!(
                    o,
                    "screening: {} groups, {groups_asked} asked, {groups_dropped} dropped (< {}) → {} tests screened out",
                    s.groups.len(),
                    cfg.select.group_threshold,
                    count(s, |c| c.screen == Screen::Dropped)
                );
                let _ = writeln!(
                    o,
                    "judging: {} tests judged in views [{}]; picked top-{} (at most {:.0}% per view) names {}, body {}, threshold(>= {}) {}, union {}; unjudged {}",
                    count(s, |c| c.judge == Judge::Judged),
                    cfg.jev.views.join(", "),
                    cfg.select.top_n,
                    cfg.select.top_fraction * 100.0,
                    count(s, |c| has_reason(c, "top-n:names")),
                    count(s, |c| has_reason(c, "top-n:body")),
                    cfg.select.threshold,
                    count(s, |c| has_reason(c, "threshold")),
                    count(s, |c| c.judge == Judge::Judged && c.selected),
                    count(s, |c| has_reason(c, "unjudged") || has_reason(c, "uncached")),
                );
                let src = key_source.unwrap_or("none");
                for (stage, u) in &s.stages {
                    let _ = writeln!(
                        o,
                        "jev {stage}: {} questions, {} requests ({} split), {} cache hits{}, {} in + {} out tokens, ${:.4}, {} ms",
                        u.asked,
                        u.requests,
                        u.splits,
                        u.cache_hits,
                        if u.uncached > 0 { format!(", {} uncached", u.uncached) } else { String::new() },
                        u.input_tokens,
                        u.output_tokens,
                        u.est_cost_usd(),
                        u.wall_ms
                    );
                }
                let _ = writeln!(o, "jev: key from {src}{}", if *offline { ", offline (cache only)" } else { "" });
                if let Some(n) = jev_notice(s, cfg) {
                    let _ = writeln!(o, "{n}");
                }
            }
        }
        let _ = writeln!(
            o,
            "policy: selected {} of {} (max-tests dropped {}, min-tests added {})",
            count(s, |c| c.selected),
            s.candidates.len(),
            count(s, |c| has_reason(c, "max-tests")),
            count(s, |c| has_reason(c, "min-tests")),
        );
    }
    if !s.rule_runs.is_empty() || !cfg.tests.always.is_empty() || !cfg.tests.never.is_empty() {
        let _ = writeln!(
            o,
            "filtersets: rules {}, always {}, never {}",
            s.rule_runs.len(),
            cfg.tests.always.len(),
            cfg.tests.never.len()
        );
    }
    for n in &plan.notes {
        let _ = writeln!(o, "note: {n}");
    }
    if verbose {
        for c in s.candidates.iter().filter(|c| c.selected) {
            let _ = writeln!(o, "  + {} {} [{}]", name(c.pkg), test_path(c), c.reasons.join(", "));
        }
    }
    let _ = write!(o, "done in {} ms", s.wall_ms);
    o
}

fn test_path(c: &Candidate) -> String {
    if c.test.module.is_empty() { c.test.name.clone() } else { format!("{}::{}", c.test.module, c.test.name) }
}

/// `--format list`: one selected test per line.
pub fn list(s: &Selection) -> String {
    let mut o = String::new();
    for c in s.candidates.iter().filter(|c| c.selected) {
        let _ = writeln!(o, "{}\t{}\t{}:{}", s.ws.packages[c.pkg].name, test_path(c), c.test.file, c.test.start);
    }
    o
}

fn usage_json(stage: &str, u: &Usage) -> Value {
    json!({
        "stage": stage,
        "asked": u.asked,
        "requests": u.requests,
        "splits": u.splits,
        "cache_hits": u.cache_hits,
        "uncached": u.uncached,
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "ms": u.wall_ms as u64,
        "est_cost_usd": u.est_cost_usd(),
        "failures": u.failures,
    })
}

pub fn report(s: &Selection, cfg: &Config, profile: Option<&str>, plan: &Plan) -> Value {
    let ws = &s.ws;
    let name = |p: usize| ws.packages[p].name.as_str();
    let per_view = |vals: &[Option<f64>; 2]| json!({VIEWS[0]: vals[0], VIEWS[1]: vals[1]});
    let candidates: Vec<Value> = s
        .candidates
        .iter()
        .map(|c| {
            json!({
                "package": name(c.pkg),
                "file": c.test.file,
                "line": c.test.start,
                "end_line": c.test.end,
                "module": c.test.module,
                "name": c.test.name,
                "reach_depth": c.depth,
                "must": c.must.map(Must::as_str),
                "evidence": c.evidence.as_ref().map(|(k, syms)| json!({"kind": evidence_kind(*k), "symbols": syms})),
                "coverage": json!({"known": c.coverage.is_some(), "covered": c.coverage.as_deref().unwrap_or_default()}),
                "screen": format!("{:?}", c.screen).to_lowercase(),
                "group_noul": c.group.and_then(|g| s.groups[g].noul),
                "nouls": per_view(&c.nouls),
                "score": c.score,
                "ranks": json!({VIEWS[0]: c.ranks[0], VIEWS[1]: c.ranks[1]}),
                "reasons": c.reasons,
                "selected": c.selected,
            })
        })
        .collect();
    let (jev_enabled, jev_off, key_source, offline) = match &s.jev {
        JevState::On { key_source, offline } => (true, None, *key_source, *offline),
        JevState::Off(why) => (false, Some(why.as_str()), None, false),
    };
    let mut total = Usage::default();
    for (_, u) in &s.stages {
        total.asked += u.asked;
        total.requests += u.requests;
        total.splits += u.splits;
        total.cache_hits += u.cache_hits;
        total.uncached += u.uncached;
        total.input_tokens += u.input_tokens;
        total.output_tokens += u.output_tokens;
        total.wall_ms += u.wall_ms;
    }
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "base": s.base,
        "head": s.target.label(),
        "changes": s.scope.report(),
        "profile": profile,
        "config": cfg.source.as_ref().map(|p| p.display().to_string()),
        "changed_files": s.files.iter().map(|f| json!({
            "path": f.path(),
            "old_path": f.old_path,
            "status": f.status.as_str(),
            "new_ranges": f.new_ranges,
            "old_ranges": f.old_ranges,
        })).collect::<Vec<_>>(),
        "ignored_files": s.ignored,
        "escalations": s.escalations.iter().map(|e| json!({
            "file": e.file,
            "kind": e.kind,
            "package": e.package.map(name),
            "reason": e.reason,
        })).collect::<Vec<_>>(),
        "rules_fired": s.rules_fired.iter().map(|r| {
            let rule = &cfg.rules[r.index];
            json!({"rule": r.index + 1, "files": r.files, "run": rule.run, "packages": rule.packages, "full": rule.full})
        }).collect::<Vec<_>>(),
        "full_run": s.full,
        "changed_packages": s.changed.iter().map(|&p| name(p)).collect::<Vec<_>>(),
        "reached_packages": s.reached.iter().map(|&(p, d)| json!({"name": name(p), "depth": d})).collect::<Vec<_>>(),
        "whole_packages": s.whole.iter().map(|(&p, why)| json!({"name": name(p), "reason": why})).collect::<Vec<_>>(),
        "changed_items": s.symbols,
        "candidates": candidates,
        "selected": s.candidates.iter().filter(|c| c.selected).count(),
        "jev": {
            "enabled": jev_enabled,
            "off_reason": jev_off,
            "key_source": key_source,
            "offline": offline,
            "views": cfg.jev.views,
            "top_n": cfg.select.top_n,
            "top_fraction": cfg.select.top_fraction,
            "threshold": cfg.select.threshold,
            "group_threshold": cfg.select.group_threshold,
            "groups": s.groups.len(),
            "error": s.jev_error,
            "stages": s.stages.iter().map(|(n, u)| usage_json(n, u)).collect::<Vec<_>>(),
            "total": usage_json("total", &total),
        },
        "filtersets": {"rules": s.rule_runs, "always": cfg.tests.always, "never": cfg.tests.never},
        "coverage": coverage_json(s),
        "filter": plan.filter,
        "commands": plan.commands,
        "command": plan.commands.iter().map(|c| command_line(c)).collect::<Vec<_>>().join("\n"),
        "notes": plan.notes,
        "wall_ms": s.wall_ms as u64,
    })
}

/// `explain`'s coverage verdict for one candidate.
fn coverage_verdict(s: &Selection, c: &Candidate) -> String {
    let cv = &s.coverage;
    if cv.map.is_none() {
        return format!("layer skipped: {}", cv.reason.as_deref().unwrap_or("off"));
    }
    let Some(fns) = &c.coverage else {
        return "not in the map (new, renamed, or not run when it was built) → never gated".to_owned();
    };
    if !fns.is_empty() {
        let effect = match (cv.used, c.must) {
            (CoveragePolicy::Must, Some(Must::Covered)) => "→ must".to_owned(),
            (_, Some(m)) => format!("(already must: {})", m.as_str()),
            (CoveragePolicy::Boost, None) => format!("→ boost +{}, skips screening", crate::coverage::COVER_BOOST),
            _ => "→ not gated".to_owned(),
        };
        return format!("executes changed {} {effect}", fns.join(", "));
    }
    if c.gated {
        return "executes none of the changed functions → gated out (not-covered)".to_owned();
    }
    let why = if let Some(m) = c.must {
        format!("must: {}", m.as_str())
    } else if c.evidence.is_some() {
        "has static evidence".to_owned()
    } else if cv.no_gate.contains(&c.pkg) {
        "its package is exempt: coverage-blind changes (see blind_items)".to_owned()
    } else {
        format!("policy {}", cv.used.as_str())
    };
    format!("executes none of the changed functions; not gated ({why})")
}

/// `explain PATTERN`: every layer's verdict for each matching candidate.
pub fn explain(s: &Selection, cfg: &Config, pattern: &str) -> String {
    let ws = &s.ws;
    let name = |p: usize| ws.packages[p].name.as_str();
    let mut o = String::new();
    if let Some(why) = &s.full {
        let _ = writeln!(o, "full run: every test runs ({why})");
        return o;
    }
    let sel = &cfg.select;
    let mut hits = 0;
    for c in &s.candidates {
        let full = format!("{}::{}", name(c.pkg), test_path(c));
        if !full.contains(pattern) && !c.test.file.contains(pattern) {
            continue;
        }
        hits += 1;
        let _ = writeln!(o, "{} {}  ({}:{})", name(c.pkg), test_path(c), c.test.file, c.test.start);
        let _ = writeln!(
            o,
            "  reach:      {}",
            if c.depth == 0 { "changed package".to_owned() } else { format!("{} reverse-dependency hop(s) from a changed package", c.depth) }
        );
        let _ = writeln!(
            o,
            "  path:       {}",
            s.whole.get(&c.pkg).map_or_else(|| "no package-wide escalation".to_owned(), |w| format!("whole package: {w}"))
        );
        let _ = writeln!(o, "  discovery:  lines {}-{}, module `{}`", c.test.start, c.test.end, c.test.module);
        let ev = match (&c.must, &c.evidence) {
            (Some(Must::Changed), _) => "its own lines changed → must".to_owned(),
            (_, Some((k, syms))) if is_static(*k) => format!(
                "{} via {}{}",
                evidence_kind(*k),
                syms.join(", "),
                if c.must.is_some() { " → must".to_owned() } else { format!(" → boost +{BOOST}, skips screening") }
            ),
            (Some(Must::Package), _) => "whole package → must".to_owned(),
            _ => "none".to_owned(),
        };
        let _ = writeln!(o, "  evidence:   {ev}");
        let _ = writeln!(o, "  coverage:   {}", coverage_verdict(s, c));
        let screen = match c.screen {
            Screen::NotAsked if c.must.is_some() => "skipped (must)".to_owned(),
            Screen::NotAsked if c.gated => "skipped (gated out by coverage)".to_owned(),
            Screen::NotAsked => "skipped (Jev off)".to_owned(),
            Screen::KeptEvidence => "kept without asking (a group member has static evidence or a coverage boost)".to_owned(),
            Screen::Single => "skipped (only test in its group; judged directly)".to_owned(),
            Screen::Kept | Screen::Dropped => {
                let n = c.group.and_then(|g| s.groups[g].noul).unwrap_or(0.0);
                let verdict = if c.screen == Screen::Kept { "kept" } else { "DROPPED" };
                format!("group Noul {n:.3} vs group_threshold {}: {verdict}", sel.group_threshold)
            }
            Screen::OverBudget => "not asked (max_questions reached)".to_owned(),
            Screen::Failed => "Jev request failed".to_owned(),
            Screen::Uncached => "not in cache (offline)".to_owned(),
        };
        let _ = writeln!(o, "  screening:  {screen}");
        let judge = match c.judge {
            Judge::Judged => {
                let views: Vec<String> = (0..2)
                    .filter_map(|v| {
                        let n = c.nouls[v]?;
                        Some(format!("{} {n:.3} (rank {})", VIEWS[v], c.ranks[v].map_or("-".to_owned(), |r| r.to_string())))
                    })
                    .collect();
                format!(
                    "{}; score {:.3}{}",
                    views.join(", "),
                    c.score.unwrap_or(0.0),
                    if c.boost() > 0.0 { format!(" (incl. +{} boost)", c.boost()) } else { String::new() }
                )
            }
            Judge::NotAsked => "not asked".to_owned(),
            Judge::OverBudget => "not asked (max_questions reached)".to_owned(),
            Judge::Failed => "Jev request failed".to_owned(),
            Judge::Uncached => "not in cache (offline)".to_owned(),
        };
        let _ = writeln!(o, "  judging:    {judge}");
        let _ = writeln!(
            o,
            "  policy:     {} [{}]  (top_n {}, top_fraction {}, threshold {})",
            if c.selected { "SELECTED" } else { "dropped" },
            c.reasons.join(", "),
            sel.top_n,
            sel.top_fraction,
            sel.threshold
        );
    }
    if hits == 0 {
        let reached: Vec<&str> = s.reached.iter().map(|&(p, _)| name(p)).collect();
        let _ = writeln!(
            o,
            "no candidate test matches `{pattern}`; reached packages: [{}] (tests outside them are not candidates)",
            reached.join(", ")
        );
    }
    o
}
