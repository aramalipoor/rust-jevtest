// `cargo jevtest`: pick the Rust tests worth running for a git diff.
// Also built as the standalone `jevtest` binary (src/bin/jevtest.rs includes this file).

mod changes;
mod config;
mod coverage;
mod evidence;
mod git;
mod jev;
mod manifest;
mod outside;
mod output;
mod scan;
mod select;
mod workspace;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use config::{ChangeMode, Config, Runner, WithoutJev};

/// Run only the Rust tests your change can break: crate reach, static evidence, a per-test
/// coverage map and TypeSafe Jev judgments, as a cargo-nextest (or cargo test) command.
#[derive(Parser)]
#[command(name = "cargo-jevtest", bin_name = "cargo jevtest", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    g: Global,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the command that runs the selected tests (the default). Summary on stderr.
    Select,
    /// Select, then run the tests; exits with the runner's code.
    Run {
        /// Extra arguments for the runner.
        #[arg(last = true, value_name = "ARGS")]
        args: Vec<String>,
    },
    /// Show every layer's verdict for each candidate test whose name or path contains PATTERN.
    Explain { pattern: String },
    /// Write jevtest.toml (commented defaults) and print the AGENTS.md block.
    Init {
        /// Overwrite an existing jevtest.toml.
        #[arg(long)]
        force: bool,
    },
    /// Check git, cargo metadata, the runner, the API key and one tiny Jev call.
    Doctor,
    /// Manage the cache (Jev answers, the literal index of `paths.outside = "referenced"`).
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Build or inspect the per-test coverage map.
    Coverage {
        #[command(subcommand)]
        action: CoverageAction,
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Delete every cached Jev answer and the literal index.
    Clear,
}

#[derive(Subcommand)]
enum CoverageAction {
    /// Run every test (one process each) under an instrumented build and write the map of which
    /// workspace functions each test executes. Needs cargo-nextest and LLVM tools matching rustc.
    Build {
        /// Where to write the map (default: config `coverage.map`, `.jevtest/coverage.json.gz`).
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
        /// Extra `cargo nextest run` arguments (filters, features, `--run-ignored all`).
        #[arg(last = true, value_name = "NEXTEST_ARGS")]
        args: Vec<String>,
    },
    /// Show the map: path, commit, age, tests, functions, size; warns when stale.
    Info,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    /// Command on stdout, summary on stderr.
    Human,
    /// The full report on stdout.
    Json,
    /// Only the command on stdout.
    Command,
    /// Only the nextest filterset on stdout.
    Filter,
    /// Selected tests on stdout: package, test path, file:line (tab-separated).
    List,
}

#[derive(Args)]
struct Global {
    /// What counts as the change (config `changes.mode`, default auto: uncommitted work if dirty,
    /// else the branch vs the default branch, else the last commit; big diffs narrow to today).
    #[arg(long, global = true, value_enum, value_name = "MODE")]
    changes: Option<ChangeMode>,
    /// Uncommitted work: staged + unstaged + untracked vs HEAD.
    #[arg(long, global = true, group = "scope")]
    uncommitted: bool,
    /// Staged changes only (the index vs HEAD).
    #[arg(long, global = true, group = "scope")]
    staged: bool,
    /// Unstaged changes only (the working tree vs the index).
    #[arg(long, global = true, group = "scope")]
    unstaged: bool,
    /// The branch since it left BASE (default: the default branch), plus uncommitted work.
    #[arg(long, global = true, group = "scope", value_name = "BASE", num_args = 0..=1, default_missing_value = "")]
    branch: Option<String>,
    /// The last N commits.
    #[arg(long, global = true, group = "scope", value_name = "N")]
    last: Option<usize>,
    /// Exactly this commit.
    #[arg(long, global = true, group = "scope", value_name = "REV")]
    commit: Option<String>,
    /// Commits since WHEN (git date: today, midnight, "6 hours ago", 2026-10-01).
    #[arg(long, global = true, group = "scope", value_name = "WHEN")]
    since: Option<String>,
    /// An explicit range A..B (or A.. for A against the working tree).
    #[arg(long, global = true, group = "scope", value_name = "A..B")]
    range: Option<String>,
    /// Range base (config `changes.base`); with --head, the range is BASE..HEAD.
    #[arg(long, global = true, value_name = "REV")]
    base: Option<String>,
    /// Range head (default: the working tree, including untracked .rs files).
    #[arg(long, global = true, value_name = "REV", requires = "base")]
    head: Option<String>,
    /// Only these paths or globs (repeatable); a listed file with no diff counts as wholly changed.
    #[arg(long, global = true, value_name = "PATH", num_args = 1..)]
    files: Vec<String>,
    /// Config profile ([profile.NAME]); also env JEVTEST_PROFILE.
    #[arg(long, global = true, value_name = "NAME")]
    profile: Option<String>,
    /// Config file (default: jevtest.toml, then .config/jevtest.toml at the repo root).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Output format.
    #[arg(long, global = true, value_enum, default_value_t = Format::Human)]
    format: Format,
    /// Also write the full JSON report here.
    #[arg(long, global = true, value_name = "PATH")]
    json: Option<PathBuf>,
    /// Skip Jev: select by static layers (see `select.without_jev`).
    #[arg(long, global = true)]
    no_jev: bool,
    /// Use cached Jev answers only; uncached tests are selected unjudged.
    #[arg(long, global = true)]
    offline: bool,
    /// Tests taken from the top of each Jev view.
    #[arg(long, global = true, value_name = "N")]
    top_n: Option<usize>,
    /// Cap each view's top picks at this share (0..1) of the tests it ranked.
    #[arg(long, global = true, value_name = "F")]
    top_fraction: Option<f64>,
    /// Score at or above which a judged test is always selected.
    #[arg(long, global = true, value_name = "F")]
    threshold: Option<f64>,
    /// Stage-1 screening cutoff per module group.
    #[arg(long, global = true, value_name = "F")]
    group_threshold: Option<f64>,
    /// Cap on selected tests (0 = none); drops the lowest-scored non-must picks.
    #[arg(long, global = true, value_name = "N")]
    max_tests: Option<usize>,
    /// Test runner for the printed command.
    #[arg(long, global = true, value_enum)]
    runner: Option<Runner>,
    /// Selection when Jev is off: reach (every reached test) or evidence (must + static evidence).
    #[arg(long, global = true, value_name = "reach|evidence", value_parser = parse_without_jev)]
    without_jev: Option<WithoutJev>,
    /// More detail on stderr (changed files and items, selected tests).
    #[arg(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,
}

fn parse_without_jev(s: &str) -> Result<WithoutJev, String> {
    match s {
        "reach" => Ok(WithoutJev::Reach),
        "evidence" => Ok(WithoutJev::Evidence),
        _ => Err("expected `reach` or `evidence`".into()),
    }
}

/// Printed by `init`; docs/agents.md shows the same block.
const AGENTS_BLOCK: &str = "<!-- jevtest -->
## Tests: use jevtest

- Before you finish a change, run `cargo jevtest run` from the repo root. It runs only the tests your diff can break (cargo-nextest, else cargo test) and exits with the runner's code.
- Red: fix the code (or the spec, and say which) and rerun `cargo jevtest run` until it is green. Never loosen or skip a failing test.
- Why was a test picked or skipped? `cargo jevtest explain <PATTERN>`.
- Machine-readable selection: `cargo jevtest --format json`.
- Toolchain and `.config/nextest.toml` changes, and `Cargo.toml`/`Cargo.lock` changes beyond version stamps and dependency bumps, escalate to the full suite on their own; let it run.
- The full suite still runs for releases and in CI.
<!-- /jevtest -->";

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().collect();
    // The hidden target runner `coverage build` installs: `cargo-jevtest __cov-runner <bin> <args…>`.
    if args.get(1).is_some_and(|a| a == coverage::RUNNER) {
        return coverage::runner(&args[2..]);
    }
    if args.get(1).is_some_and(|a| a == "jevtest") {
        args.remove(1);
    }
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() { ExitCode::from(1) } else { ExitCode::SUCCESS };
        }
    };
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("jevtest: {e}");
            ExitCode::from(1)
        }
    }
}

fn profile(g: &Global) -> Option<String> {
    g.profile.clone().or_else(|| std::env::var("JEVTEST_PROFILE").ok().filter(|p| !p.is_empty()))
}

/// The change scope the flags ask for; flags without one leave `[changes]` in charge.
fn change_request(g: &Global) -> Result<changes::Request, String> {
    let mut r = changes::Request { mode: g.changes, ..Default::default() };
    let mut set = |mode| r.mode = Some(mode);
    if g.uncommitted {
        set(ChangeMode::Uncommitted);
    } else if g.staged {
        set(ChangeMode::Staged);
    } else if g.unstaged {
        set(ChangeMode::Unstaged);
    } else if g.branch.is_some() {
        set(ChangeMode::Branch);
    } else if g.last.is_some() {
        set(ChangeMode::Last);
    } else if g.commit.is_some() {
        set(ChangeMode::Range);
    } else if g.since.is_some() {
        set(ChangeMode::Since);
    } else if g.range.is_some() || g.base.is_some() {
        set(ChangeMode::Range);
    }
    r.branch_base = g.branch.clone().filter(|b| !b.is_empty());
    r.last = g.last;
    r.commit = g.commit.clone();
    r.since = g.since.clone();
    r.base = g.base.clone();
    r.head = g.head.clone();
    if let Some(range) = &g.range {
        if g.base.is_some() {
            return Err("--range and --base exclude each other".into());
        }
        let (a, b) = range.split_once("..").ok_or("--range takes A..B (or A.. for the working tree)")?;
        let b = b.trim_start_matches('.');
        r.base = Some(a.to_owned());
        r.head = (!b.is_empty()).then(|| b.to_owned());
    }
    if r.mode.is_some() && r.mode != Some(ChangeMode::Range) && (g.base.is_some() || g.head.is_some()) {
        return Err("--base/--head only go with --range-style scopes, not with --uncommitted, --staged, --unstaged, --branch, --last, --commit or --since".into());
    }
    Ok(r)
}

/// Config with CLI overrides applied and validated.
fn load_config(root: &Path, g: &Global) -> Result<Config, String> {
    let mut cfg = config::load(root, g.config.as_deref(), profile(g).as_deref())?;
    if !g.files.is_empty() {
        cfg.changes.files.clone_from(&g.files);
    }
    let s = &mut cfg.select;
    if let Some(v) = g.top_n {
        s.top_n = v;
    }
    if let Some(v) = g.top_fraction {
        s.top_fraction = v;
    }
    if let Some(v) = g.threshold {
        s.threshold = v;
    }
    if let Some(v) = g.group_threshold {
        s.group_threshold = v;
    }
    if let Some(v) = g.max_tests {
        s.max_tests = v;
    }
    if let Some(v) = g.runner {
        s.runner = v;
    }
    if let Some(v) = g.without_jev {
        s.without_jev = v;
    }
    // Env overrides for the endpoint (handy for proxies and for testing failover).
    for (var, field) in [("TYPESAFE_BASE_URL", &mut cfg.jev.base_url), ("TYPESAFE_MODEL", &mut cfg.jev.model)] {
        if let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            *field = v.trim().to_owned();
        }
    }
    cfg.validate().map_err(|e| format!("command-line override: {e}"))?;
    Ok(cfg)
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    let g = &cli.g;
    match &cli.cmd {
        Some(Cmd::Init { force }) => return init(*force),
        Some(Cmd::Doctor) => return doctor(g),
        Some(Cmd::Cache { action: CacheAction::Clear }) => return cache_clear(g),
        Some(Cmd::Coverage { action }) => {
            let root = git::repo_root(Path::new("."))?;
            let cfg = config::load(&root, g.config.as_deref(), profile(g).as_deref())?;
            match action {
                CoverageAction::Build { out, args } => coverage::build(&root, &cfg.coverage, out.as_deref(), args)?,
                CoverageAction::Info => coverage::info(&root, &cfg.coverage)?,
            }
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }

    let root = git::repo_root(Path::new("."))?;
    let cfg = load_config(&root, g)?;
    let runner = cfg.select.runner.resolve();
    if cfg.select.runner == Runner::Auto && runner == Runner::Cargo {
        eprintln!("jevtest: cargo-nextest not found; running cargo test (install: cargo install cargo-nextest --locked)");
    }
    let req = change_request(g)?;
    let ignore = select::globset(&cfg.paths.ignore)?;
    let resolving = Instant::now();
    let scope = changes::resolve(&root, &cfg.changes, &req, &ignore)?;
    let scope_ms = resolving.elapsed().as_secs_f64() * 1000.0;
    eprintln!("{}", scope.line());
    let sw = select::Switches { no_jev: g.no_jev, offline: g.offline };
    let mut s = select::run(&root, &cfg, scope, &sw)?;
    s.timings.insert(0, ("changes", scope_ms));
    let extra: &[String] = match &cli.cmd {
        Some(Cmd::Run { args }) => args,
        _ => &[],
    };
    let plan = output::plan(&s, &cfg, runner, extra);
    let profile = profile(g);
    let profile = profile.as_deref();
    let verbose = g.verbose > 0;
    let report = || output::report(&s, &cfg, profile, &plan);
    if let Some(path) = &g.json {
        let text = serde_json::to_string_pretty(&report()).map_err(|e| e.to_string())?;
        std::fs::write(path, text + "\n").map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    let summary = || eprintln!("{}", output::summary(&s, &cfg, profile, &plan, verbose));

    match &cli.cmd {
        Some(Cmd::Explain { pattern }) => {
            summary();
            print!("{}", output::explain(&s, &cfg, pattern));
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Run { .. }) => {
            summary();
            if plan.commands.is_empty() {
                eprintln!("no tests worth running");
                return Ok(ExitCode::SUCCESS);
            }
            exec(&root, &plan.commands)
        }
        _ => {
            if g.format == Format::Human || verbose {
                summary();
            } else {
                if (s.jev_error.is_some() || matches!(&s.jev, select::JevState::Off(why) if why == "no API key"))
                    && let Some(n) = output::jev_notice(&s, &cfg)
                {
                    eprintln!("{n}");
                }
                for n in &plan.notes {
                    eprintln!("jevtest: {n}");
                }
            }
            match g.format {
                Format::Human | Format::Command => {
                    if plan.commands.is_empty() {
                        println!("# no tests worth running");
                    }
                    for c in &plan.commands {
                        println!("{}", output::command_line(c));
                    }
                }
                Format::Filter => {
                    if let Some(f) = &plan.filter {
                        println!("{f}");
                    }
                }
                Format::List => print!("{}", output::list(&s)),
                Format::Json => {
                    println!("{}", serde_json::to_string_pretty(&report()).map_err(|e| e.to_string())?);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Runs each command in turn (all of them, so every failure shows); exits with the first failing code.
fn exec(root: &Path, commands: &[Vec<String>]) -> Result<ExitCode, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut code = 0u8;
    for c in commands {
        eprintln!("+ {}", output::command_line(c));
        let status = Command::new(&cargo)
            .args(&c[1..])
            .current_dir(root)
            .status()
            .map_err(|e| format!("cannot run {}: {e}", c.join(" ")))?;
        if code == 0 && !status.success() {
            code = status.code().map_or(1, |c| c.clamp(1, 255) as u8);
        }
    }
    Ok(ExitCode::from(code))
}

fn init(force: bool) -> Result<ExitCode, String> {
    let root = git::repo_root(Path::new(".")).or_else(|_| std::env::current_dir().map_err(|e| e.to_string()))?;
    let path = root.join("jevtest.toml");
    let existing = [path.clone(), root.join(".config/jevtest.toml")].into_iter().find(|p| p.exists());
    if let Some(p) = existing
        && !force
    {
        return Err(format!("{} already exists (use --force to overwrite jevtest.toml)", p.display()));
    }
    std::fs::write(&path, config::TEMPLATE).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    eprintln!("wrote {}", path.display());
    eprintln!("add this block to AGENTS.md (or CLAUDE.md):\n");
    println!("{AGENTS_BLOCK}");
    Ok(ExitCode::SUCCESS)
}

fn cache_clear(g: &Global) -> Result<ExitCode, String> {
    let root = git::repo_root(Path::new(".")).or_else(|_| std::env::current_dir().map_err(|e| e.to_string()))?;
    let cfg = config::load(&root, g.config.as_deref(), profile(g).as_deref())?;
    let dir = &cfg.jev.cache_dir;
    let mut removed = 0usize;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            let ours = p.extension().is_some_and(|x| x == "answers" || x == "json" || x == "too-large" || x == "tmp");
            if ours && std::fs::remove_file(&p).is_ok() {
                removed += 1;
            }
        }
    }
    let literals = std::fs::remove_dir_all(dir.join(outside::INDEX_DIR)).is_ok();
    eprintln!(
        "removed {removed} cached answers{} from {}",
        if literals { " and the literal index" } else { "" },
        dir.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn doctor(g: &Global) -> Result<ExitCode, String> {
    let mut failed = false;
    let mut line = |ok: Option<bool>, what: &str, detail: String| {
        let tag = match ok {
            Some(true) => "ok  ",
            Some(false) => {
                failed = true;
                "FAIL"
            }
            None => "warn",
        };
        println!("{tag} {what:<14} {detail}");
    };

    let root = match git::repo_root(Path::new(".")) {
        Ok(root) => {
            line(Some(true), "git", format!("repo {}", root.display()));
            root
        }
        Err(e) => {
            line(Some(false), "git", e);
            return Ok(ExitCode::from(1));
        }
    };
    match load_config(&root, g).and_then(|cfg| {
        let ignore = select::globset(&cfg.paths.ignore)?;
        changes::resolve(&root, &cfg.changes, &change_request(g)?, &ignore)
    }) {
        Ok(scope) => line(Some(true), "changes", scope.line().trim_start_matches("jevtest: changes = ").to_owned()),
        Err(e) => line(None, "changes", format!("{e}; pick one with --uncommitted, --branch, --last N or --range A..B")),
    }
    let started = Instant::now();
    match workspace::Workspace::load(&root) {
        Ok(ws) => line(
            Some(true),
            "cargo metadata",
            format!("{} workspace packages ({} ms)", ws.packages.len(), started.elapsed().as_millis()),
        ),
        Err(e) => line(Some(false), "cargo metadata", e),
    }
    let cfg = match load_config(&root, g) {
        Ok(cfg) => {
            let from = cfg.source.as_ref().map_or_else(|| "built-in defaults (run `cargo jevtest init`)".to_owned(), |p| p.display().to_string());
            line(Some(true), "config", from);
            cfg
        }
        Err(e) => {
            line(Some(false), "config", e);
            return Ok(ExitCode::from(1));
        }
    };

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let version = |args: &[&str]| -> Option<String> {
        let out = Command::new(&cargo).args(args).output().ok().filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or_default().to_owned())
    };
    const NEXTEST_HINT: &str = "install: cargo install cargo-nextest --locked (or cargo binstall cargo-nextest)";
    match cfg.select.runner {
        Runner::Auto => match version(&["nextest", "--version"]) {
            Some(v) => line(Some(true), "runner", format!("auto → nextest ({v})")),
            None => line(None, "runner", format!("auto → cargo test (cargo-nextest not found; {NEXTEST_HINT})")),
        },
        Runner::Nextest => match version(&["nextest", "--version"]) {
            Some(v) => line(Some(true), "runner", format!("nextest ({v})")),
            None => line(Some(false), "runner", format!("cargo-nextest not found; {NEXTEST_HINT}, or use --runner auto|cargo")),
        },
        Runner::Cargo => match version(&["--version"]) {
            Some(v) => line(Some(true), "runner", format!("cargo test ({v})")),
            None => line(Some(false), "runner", "cargo not found; install Rust from https://rustup.rs".into()),
        },
    }

    match cfg.jev.resolve_key() {
        None => {
            line(
                None,
                "api key",
                format!(
                    "none: set {} or write the key to {}; without it selection uses static layers only",
                    cfg.jev.key_env.join(" or "),
                    cfg.jev.key_file.display()
                ),
            );
        }
        Some((key, source)) => {
            let (url, model) = cfg.jev.endpoint(source);
            line(Some(true), "api key", format!("from {source}; {url} model {model}"));
            let client = jev::Client::new(url, model, Some(key), cfg.jev.timeout_secs, &cfg.jev.cache_dir);
            match client.ping() {
                Ok(p) => line(
                    Some(true),
                    "jev call",
                    format!("{} ms, noul {:.3}, {} in + {} out tokens", p.ms, p.noul, p.input_tokens, p.output_tokens),
                ),
                Err(e) => line(Some(false), "jev call", e),
            }
        }
    }
    Ok(if failed { ExitCode::from(1) } else { ExitCode::SUCCESS })
}
