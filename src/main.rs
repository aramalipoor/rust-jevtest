//! `cargo jevtest`: pick the Rust tests worth running for a git diff.

mod git;
mod jev;
mod scan;
mod workspace;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde_json::{Value, json};

use git::{FileChange, Range, Source, Status};
use scan::TestFn;
use workspace::Workspace;

/// Pick the Rust tests worth running for a git diff and print (or run) a `cargo nextest` command.
#[derive(Parser)]
#[command(name = "cargo-jevtest", bin_name = "cargo jevtest", version)]
struct Cli {
    /// Base revision (default: merge-base of HEAD and origin/HEAD, origin/main or origin/master).
    #[arg(long)]
    base: Option<String>,
    /// Head revision (default: the working tree).
    #[arg(long)]
    head: Option<String>,
    /// Minimum Noul for a judged test to be selected.
    #[arg(long, default_value_t = 0.2)]
    threshold: f64,
    /// Minimum group Noul (stage 1) for a group's tests to be asked about one by one.
    #[arg(long, default_value_t = 0.1)]
    group_threshold: f64,
    /// Questions per Jev request (requests the API refuses as too large are split in half).
    #[arg(long, default_value_t = 100)]
    batch: usize,
    /// Concurrent Jev requests.
    #[arg(long, default_value_t = 4)]
    concurrency: usize,
    /// Most Jev questions (groups + tests); tests past it are selected unjudged.
    #[arg(long, default_value_t = 1500)]
    max_questions: usize,
    /// Diff characters given to Jev.
    #[arg(long, default_value_t = 24000)]
    max_state_chars: usize,
    /// Test source characters given to Jev per test.
    #[arg(long, default_value_t = 800)]
    max_test_chars: usize,
    /// Module context characters given to Jev per group.
    #[arg(long, default_value_t = 600)]
    max_group_chars: usize,
    /// Extra glob of changed files to ignore (repeatable).
    #[arg(long, value_name = "GLOB")]
    ignore: Vec<String>,
    /// Write a JSON report here.
    #[arg(long, value_name = "PATH")]
    json: Option<PathBuf>,
    /// Skip Jev: select every candidate test.
    #[arg(long)]
    no_jev: bool,
    /// Run the nextest command instead of printing it.
    #[arg(long)]
    run: bool,
    /// Directory inside the repository (default: current directory).
    #[arg(long, value_name = "DIR")]
    manifest_dir: Option<PathBuf>,
    /// Extra arguments passed to `cargo nextest run`.
    #[arg(last = true, value_name = "EXTRA_NEXTEST_ARGS")]
    extra: Vec<String>,
}

const ROOT_FULL: [&str; 4] = ["Cargo.toml", "Cargo.lock", "rust-toolchain", "rust-toolchain.toml"];
const DEFAULT_IGNORES: [&str; 3] = ["**/*.md", "docs/**", ".github/**"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reason {
    Changed,
    Package,
    Jev,
    JevGroup,
    Unjudged,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Reason::Changed => "changed",
            Reason::Package => "package",
            Reason::Jev => "jev",
            Reason::JevGroup => "jev-group",
            Reason::Unjudged => "unjudged",
        }
    }
}

struct Escalation {
    file: String,
    /// `full` (whole workspace) or `whole` (one package).
    kind: &'static str,
    package: Option<usize>,
    reason: String,
}

struct Candidate {
    pkg: usize,
    test: TestFn,
    reason: Reason,
    /// Stage-1 Noul of the test's group, when asked.
    group_noul: Option<f64>,
    noul: Option<f64>,
    selected: bool,
}

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "jevtest") {
        args.remove(1);
    }
    match run(Cli::parse_from(args)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("cargo-jevtest: {e}");
            ExitCode::from(2)
        }
    }
}

fn globset(patterns: impl IntoIterator<Item = impl AsRef<str>>) -> Result<GlobSet, String> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let p = p.as_ref();
        b.add(GlobBuilder::new(p).literal_separator(true).build().map_err(|e| format!("bad glob {p}: {e}"))?);
    }
    b.build().map_err(|e| e.to_string())
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    let dir = cli.manifest_dir.clone().unwrap_or_else(|| PathBuf::from("."));
    let root = git::repo_root(&dir)?;
    let base = match &cli.base {
        Some(b) => b.clone(),
        None => git::default_base(&root)?,
    };
    let head = cli.head.as_deref();
    let files = git::changed_files(&root, &base, head)?;
    let ws = Workspace::load(&root)?;
    let ignores = globset(DEFAULT_IGNORES.iter().copied().chain(cli.ignore.iter().map(String::as_str)))?;
    let mut source = Source::new(&root, head)?;

    // Step 3: classify changed files.
    let mut escalations: Vec<Escalation> = Vec::new();
    let mut ignored: Vec<&str> = Vec::new();
    let mut changed: BTreeSet<usize> = BTreeSet::new();
    let mut whole: HashSet<usize> = HashSet::new();
    let mut full = false;
    let mut symbols: Vec<String> = Vec::new();
    for f in &files {
        let mut paths: Vec<&str> = f.new_path.iter().chain(&f.old_path).map(String::as_str).collect();
        paths.dedup();
        for path in paths {
            let is_new_side = f.new_path.as_deref() == Some(path);
            let root_level = (!path.contains('/') && ROOT_FULL.contains(&path))
                || path.starts_with(".cargo/")
                || path.starts_with(".config/");
            if root_level {
                full = true;
                escalations.push(Escalation { file: path.into(), kind: "full", package: None, reason: "workspace-level file".into() });
                continue;
            }
            // Ignores apply inside packages too ("packages owning a changed (non-ignored) file").
            if ignores.is_match(path) {
                ignored.push(path);
                continue;
            }
            let Some(pkg) = ws.owner(path) else {
                full = true;
                escalations.push(Escalation { file: path.into(), kind: "full", package: None, reason: "outside every package".into() });
                continue;
            };
            changed.insert(pkg);
            let pdir = &ws.packages[pkg].dir;
            let rel = if pdir.is_empty() { path } else { &path[pdir.len() + 1..] };
            if !path.ends_with(".rs") || rel == "build.rs" {
                if whole.insert(pkg) {
                    let what = if rel == "Cargo.toml" { "manifest" } else if rel == "build.rs" { "build script" } else { "non-Rust file" };
                    escalations.push(Escalation { file: path.into(), kind: "whole", package: Some(pkg), reason: format!("{what} changed") });
                }
                continue;
            }
            let pname = &ws.packages[pkg].name;
            if !is_new_side {
                // Old side of a rename or a deletion: the code at this path is gone.
                if f.status == Status::Deleted {
                    symbols.push(format!("{pname}: {path}"));
                }
                continue;
            }
            let Some(src) = source.read(path) else {
                whole.insert(pkg);
                escalations.push(Escalation { file: path.into(), kind: "whole", package: Some(pkg), reason: "cannot read new side".into() });
                continue;
            };
            match syn::parse_file(&src) {
                Ok(file) => {
                    let lines: Vec<u32> = f.new_ranges.iter().flat_map(|&(a, b)| a..=b).collect();
                    for sym in scan::symbols_at(&file, &scan::module_base(rel), &lines) {
                        symbols.push(match sym {
                            Some(s) => format!("{pname}: {s}"),
                            None => format!("{pname}: {path} (top level)"),
                        });
                    }
                }
                Err(e) => {
                    whole.insert(pkg);
                    symbols.push(format!("{pname}: {path}"));
                    let at = e.span().start();
                    escalations.push(Escalation {
                        file: path.into(),
                        kind: "whole",
                        package: Some(pkg),
                        reason: format!("does not parse ({}:{}: {e})", at.line, at.column + 1),
                    });
                }
            }
            proc_macro2::extra::invalidate_current_thread_spans();
        }
    }
    symbols.dedup();

    let mut changed: Vec<usize> = changed.into_iter().collect();
    changed.sort_unstable_by(|&a, &b| ws.packages[a].name.cmp(&ws.packages[b].name));
    let mut report = Report {
        cli: &cli,
        base: &base,
        head,
        files: &files,
        ws: &ws,
        changed: &changed,
        affected: Vec::new(),
        escalations,
        ignored: &ignored,
        symbols: &symbols,
        candidates: Vec::new(),
        groups: 0,
        kept_groups: 0,
        stages: Vec::new(),
        jev_error: None,
        expr: None,
        command: Vec::new(),
    };

    if full {
        report.command = nextest_command(&["--workspace".to_owned()], &cli.extra);
        report.summary();
        report.write_json()?;
        return finish(&cli, &root, &report.command);
    }
    if changed.is_empty() {
        report.summary();
        report.write_json()?;
        println!("no Rust change; no tests worth running");
        return Ok(ExitCode::SUCCESS);
    }

    // Step 4: enumerate tests of affected packages.
    let affected = ws.affected(&changed);
    let new_ranges: HashMap<&str, &[Range]> = files
        .iter()
        .filter_map(|f| Some((f.new_path.as_deref()?, f.new_ranges.as_slice())))
        .collect();
    let mut candidates: Vec<Candidate> = Vec::new();
    // (file, module) → that module's context for stage-1 group questions.
    let mut contexts: HashMap<(String, String), String> = HashMap::new();
    for &(pkg, _) in &affected {
        let pdir = ws.packages[pkg].dir.as_str();
        for path in test_files(&source.list(&root, pdir)?, pdir) {
            let rel = if pdir.is_empty() { path.as_str() } else { &path[pdir.len() + 1..] };
            let Some(src) = source.read(&path) else { continue };
            let tests = match syn::parse_file(&src) {
                Ok(file) => {
                    let (tests, ctx) = scan::tests_in(&file, &path, &scan::module_base(rel), &src);
                    contexts.extend(ctx.into_iter().map(|(module, c)| ((path.clone(), module), c)));
                    tests
                }
                Err(e) => {
                    if whole.insert(pkg) {
                        report.escalations.push(Escalation { file: path.clone(), kind: "whole", package: Some(pkg), reason: format!("does not parse: {e}") });
                    }
                    Vec::new()
                }
            };
            proc_macro2::extra::invalidate_current_thread_spans();
            let ranges = new_ranges.get(path.as_str()).copied().unwrap_or_default();
            for test in tests {
                let hit = ranges.iter().any(|&(a, b)| a <= test.end && test.start <= b);
                let reason = if hit { Reason::Changed } else { Reason::Unjudged };
                candidates.push(Candidate { pkg, test, reason, group_noul: None, noul: None, selected: false });
            }
        }
    }
    for c in &mut candidates {
        if c.reason != Reason::Changed && whole.contains(&c.pkg) {
            c.reason = Reason::Package;
        }
    }
    report.affected = affected;

    // Step 5: Jev for non-must candidates, in two stages. Stage 1 asks one Noul per
    // (package, file, module) group; stage 2 asks per test inside groups that stay in.
    let open: Vec<usize> = (0..candidates.len()).filter(|&i| candidates[i].reason == Reason::Unjudged).collect();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut group_of: HashMap<(&str, &str), usize> = HashMap::new();
    for &i in &open {
        let t = &candidates[i].test;
        let g = *group_of.entry((t.file.as_str(), t.module.as_str())).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[g].push(i);
    }
    drop(group_of);
    if !cli.no_jev && !open.is_empty() {
        match jev::Client::from_env() {
            Ok(client) => {
                let diff = git::context_diff(&root, &base, head)?;
                let cut = jev::truncate(&diff, cli.max_state_chars);
                let diff = if cut.len() < diff.len() { format!("{cut}\n[diff truncated]") } else { diff };
                let state = json!({"change": {
                    "changed_packages": changed.iter().map(|&p| ws.packages[p].name.as_str()).collect::<Vec<_>>(),
                    "changed_symbols": symbols,
                    "diff": diff,
                }});

                let asked_groups = &groups[..groups.len().min(cli.max_questions)];
                let questions: Vec<Value> = asked_groups
                    .iter()
                    .map(|members| {
                        let first = &candidates[members[0]];
                        let names: Vec<&str> = members.iter().map(|&i| candidates[i].test.name.as_str()).collect();
                        let key = (first.test.file.clone(), first.test.module.clone());
                        let context = contexts.get(&key).map(String::as_str).unwrap_or_default();
                        jev::group_question(
                            &ws.packages[first.pkg].name,
                            &first.test.file,
                            &first.test.module,
                            &names,
                            jev::truncate(context, cli.max_group_chars),
                        )
                    })
                    .collect();
                let (group_nouls, usage) = client.judge(&state, questions, cli.batch, cli.concurrency);
                report.stages.push(("groups", asked_groups.len(), usage));

                let mut stage2: Vec<usize> = Vec::new();
                for (members, noul) in asked_groups.iter().zip(group_nouls) {
                    let Some(n) = noul else { continue };
                    for &i in members {
                        candidates[i].group_noul = Some(n);
                    }
                    if n < cli.group_threshold {
                        for &i in members {
                            candidates[i].reason = Reason::JevGroup;
                        }
                        continue;
                    }
                    report.kept_groups += 1;
                    if let [only] = members.as_slice() {
                        candidates[*only].reason = Reason::Jev;
                        candidates[*only].noul = Some(n);
                    } else {
                        stage2.extend(members);
                    }
                }
                stage2.truncate(cli.max_questions - asked_groups.len());
                if !stage2.is_empty() {
                    let questions: Vec<Value> = stage2
                        .iter()
                        .map(|&i| jev::test_question(&ws.packages[candidates[i].pkg].name, &candidates[i].test, cli.max_test_chars))
                        .collect();
                    let (nouls, usage) = client.judge(&state, questions, cli.batch, cli.concurrency);
                    report.stages.push(("tests", stage2.len(), usage));
                    for (&i, noul) in stage2.iter().zip(nouls) {
                        if let Some(n) = noul {
                            candidates[i].reason = Reason::Jev;
                            candidates[i].noul = Some(n);
                        }
                    }
                }
            }
            Err(e) => report.jev_error = Some(e),
        }
    }
    report.groups = groups.len();
    for c in &mut candidates {
        c.selected = match c.reason {
            Reason::Jev => c.noul.is_some_and(|n| n >= cli.threshold),
            Reason::JevGroup => false,
            _ => true,
        };
    }

    // Step 6: filterset and command.
    let mut terms = Vec::new();
    let mut pkg_args = Vec::new();
    for &(pkg, _) in &report.affected {
        let name = &ws.packages[pkg].name;
        let mut names: Vec<&str> =
            candidates.iter().filter(|c| c.pkg == pkg && c.selected).map(|c| c.test.name.as_str()).collect();
        if whole.contains(&pkg) {
            terms.push(format!("package(={name})"));
        } else if !names.is_empty() {
            names.sort_unstable();
            names.dedup();
            terms.push(format!("(package(={name}) & test(/(^|::)({})($|::)/))", names.join("|")));
        } else {
            continue;
        }
        pkg_args.extend(["-p".to_owned(), name.clone()]);
    }
    report.candidates = candidates;
    if terms.is_empty() {
        report.summary();
        report.write_json()?;
        println!("# no tests worth running");
        return Ok(ExitCode::SUCCESS);
    }
    let expr = terms.join(" | ");
    pkg_args.extend(["-E".to_owned(), expr.clone()]);
    report.expr = Some(expr);
    report.command = nextest_command(&pkg_args, &cli.extra);
    report.summary();
    report.write_json()?;
    finish(&cli, &root, &report.command)
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

fn nextest_command(args: &[String], extra: &[String]) -> Vec<String> {
    ["cargo", "nextest", "run"].into_iter().map(str::to_owned).chain(args.iter().cloned()).chain(extra.iter().cloned()).collect()
}

fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-./=:,@%+".contains(&b));
    if plain { s.to_owned() } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

fn finish(cli: &Cli, root: &Path, command: &[String]) -> Result<ExitCode, String> {
    if !cli.run {
        println!("{}", command.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" "));
        return Ok(ExitCode::SUCCESS);
    }
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = std::process::Command::new(cargo)
        .args(&command[1..])
        .current_dir(root)
        .status()
        .map_err(|e| format!("cannot run cargo nextest: {e}"))?;
    Ok(match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(c) => ExitCode::from(c.clamp(1, 255) as u8),
        None => ExitCode::FAILURE,
    })
}

struct Report<'a> {
    cli: &'a Cli,
    base: &'a str,
    head: Option<&'a str>,
    files: &'a [FileChange],
    ws: &'a Workspace,
    changed: &'a [usize],
    affected: Vec<(usize, u32)>,
    escalations: Vec<Escalation>,
    ignored: &'a [&'a str],
    symbols: &'a [String],
    candidates: Vec<Candidate>,
    /// Stage-1 groups formed from non-must candidates, and how many stayed in (Noul >= --group-threshold).
    groups: usize,
    kept_groups: usize,
    /// Per Jev stage: name, questions asked, usage.
    stages: Vec<(&'static str, usize, jev::Usage)>,
    jev_error: Option<String>,
    expr: Option<String>,
    command: Vec<String>,
}

impl Report<'_> {
    fn name(&self, pkg: usize) -> &str {
        &self.ws.packages[pkg].name
    }

    fn count(&self, reason: Reason) -> usize {
        self.candidates.iter().filter(|c| c.reason == reason).count()
    }

    fn summary(&self) {
        let names = |it: &mut dyn Iterator<Item = usize>| it.map(|p| self.name(p)).collect::<Vec<_>>().join(", ");
        eprintln!("jevtest: {}..{}", self.base, self.head.unwrap_or("working tree"));
        eprintln!("changed files ({}):", self.files.len());
        for f in self.files {
            match (&f.old_path, &f.new_path) {
                (Some(o), Some(n)) if o != n => eprintln!("  {:<8} {o} -> {n}", f.status.as_str()),
                _ => eprintln!("  {:<8} {}", f.status.as_str(), f.path()),
            }
        }
        if !self.ignored.is_empty() {
            eprintln!("ignored files: {}", self.ignored.len());
        }
        eprintln!("changed packages ({}): {}", self.changed.len(), names(&mut self.changed.iter().copied()));
        let rdeps: Vec<usize> = self.affected.iter().filter(|a| a.1 > 0).map(|a| a.0).collect();
        eprintln!("rdeps ({}): {}", rdeps.len(), names(&mut rdeps.iter().copied()));
        for e in &self.escalations {
            let pkg = e.package.map(|p| format!(" [{}]", self.name(p))).unwrap_or_default();
            eprintln!("escalation {}{pkg}: {} ({})", e.kind, e.file, e.reason);
        }
        let selected = self.candidates.iter().filter(|c| c.selected).count();
        let changed = self.count(Reason::Changed);
        let package = self.count(Reason::Package);
        eprintln!(
            "candidates {}  must {} (changed {changed}, package {package})  groups {}  group-dropped {}  judged {}  selected {selected}  unjudged {}",
            self.candidates.len(),
            changed + package,
            self.groups,
            self.count(Reason::JevGroup),
            self.count(Reason::Jev),
            self.count(Reason::Unjudged),
        );
        if self.cli.no_jev {
            eprintln!("jev: off (--no-jev)");
        } else if let Some(e) = &self.jev_error {
            eprintln!("jev FAILED: {e}; unjudged tests selected");
        }
        for (stage, asked, u) in &self.stages {
            let kept = if *stage == "groups" {
                format!(", kept {} (>= {})", self.kept_groups, self.cli.group_threshold)
            } else {
                format!(", selected {} (>= {})", self.count_selected(Reason::Jev), self.cli.threshold)
            };
            eprintln!(
                "jev {stage}: asked {asked}{kept}; {} requests ({} split as too large), {} cache hits, {} input + {} output tokens, {} ms",
                u.requests, u.splits, u.cache_hits, u.input_tokens, u.output_tokens, u.wall_ms
            );
            for f in &u.failures {
                eprintln!("jev {stage} FAILED: {f}; its tests selected unjudged");
            }
        }
    }

    fn count_selected(&self, reason: Reason) -> usize {
        self.candidates.iter().filter(|c| c.reason == reason && c.selected).count()
    }

    fn write_json(&self) -> Result<(), String> {
        let Some(path) = &self.cli.json else { return Ok(()) };
        let stages: Vec<Value> = self
            .stages
            .iter()
            .map(|(stage, asked, u)| {
                json!({
                    "stage": stage,
                    "asked": asked,
                    "requests": u.requests,
                    "cache_hits": u.cache_hits,
                    "splits": u.splits,
                    "input_tokens": u.input_tokens,
                    "output_tokens": u.output_tokens,
                    "wall_ms": u.wall_ms as u64,
                    "failures": u.failures,
                })
            })
            .collect();
        let v = json!({
            "base": self.base,
            "head": self.head,
            "changed_files": self.files.iter().map(|f| json!({
                "path": f.path(),
                "old_path": f.old_path,
                "status": f.status.as_str(),
                "new_ranges": f.new_ranges,
                "old_ranges": f.old_ranges,
            })).collect::<Vec<_>>(),
            "changed_packages": self.changed.iter().map(|&p| self.name(p)).collect::<Vec<_>>(),
            "affected_packages": self.affected.iter().map(|&(p, d)| json!({"name": self.name(p), "distance": d})).collect::<Vec<_>>(),
            "escalations": self.escalations.iter().map(|e| json!({
                "file": e.file,
                "kind": e.kind,
                "package": e.package.map(|p| self.name(p)),
                "reason": e.reason,
            })).collect::<Vec<_>>(),
            "ignored_files": self.ignored,
            "changed_symbols": self.symbols,
            "candidates": self.candidates.iter().map(|c| json!({
                "package": self.name(c.pkg),
                "file": c.test.file,
                "line": c.test.start,
                "end_line": c.test.end,
                "module": c.test.module,
                "name": c.test.name,
                "reason": c.reason.as_str(),
                "group_noul": c.group_noul,
                "noul": c.noul,
                "selected": c.selected,
            })).collect::<Vec<_>>(),
            "jev": {
                "enabled": !self.cli.no_jev,
                "threshold": self.cli.threshold,
                "group_threshold": self.cli.group_threshold,
                "groups": self.groups,
                "kept_groups": self.kept_groups,
                "error": self.jev_error,
                "stages": stages,
            },
            "filter": self.expr,
            "command": if self.command.is_empty() { Value::Null } else { json!(self.command) },
        });
        let text = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
        std::fs::write(path, text + "\n").map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}
