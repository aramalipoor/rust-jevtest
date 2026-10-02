//! `jevtest.toml`: schema, discovery, `[profile.X]` overlays and API-key resolution.
//!
//! Precedence: built-in defaults < file top level < `[profile.X]` < CLI flags (main.rs assigns
//! fields after [`load`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_MODEL: &str = "jev-latest";
pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai/v1";
/// Env var whose key routes requests through the Vercel AI Gateway.
pub const GATEWAY_KEY_ENV: &str = "AI_GATEWAY_API_KEY";
pub const GATEWAY_MODEL: &str = "typesafe-ai/jev";
pub const GATEWAY_BASE_URL: &str = "https://ai-gateway.vercel.sh/typesafe/v1";
/// Source name returned by [`Jev::resolve_key`] when the key came from `key_file`.
pub const KEY_FILE_SOURCE: &str = "key_file";
/// Source name returned by [`Jev::resolve_key`] for an env var in `key_env` other than the two
/// defaults (which are returned by name).
pub const KEY_ENV_SOURCE: &str = "key_env";
/// Valid entries of `jev.views`.
pub const VIEWS: [&str; 2] = ["names", "body"];

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub select: Select,
    pub changes: Changes,
    pub paths: Paths,
    pub rules: Vec<Rule>,
    pub tests: Tests,
    pub jev: Jev,
    pub coverage: Coverage,
    /// The file the settings came from; `None` = built-in defaults.
    pub source: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Select {
    pub top_n: usize,
    pub top_fraction: f64,
    pub threshold: f64,
    pub group_threshold: f64,
    pub max_tests: usize,
    pub min_tests: usize,
    pub static_evidence: StaticEvidence,
    pub call_graph_depth: u8,
    pub reach_depth: usize,
    pub non_rust: NonRust,
    pub on_jev_error: OnJevError,
    pub without_jev: WithoutJev,
    pub runner: Runner,
}

impl Default for Select {
    fn default() -> Self {
        Self {
            top_n: 30,
            top_fraction: 0.25,
            threshold: 0.5,
            group_threshold: 0.1,
            max_tests: 0,
            min_tests: 0,
            static_evidence: StaticEvidence::Must,
            call_graph_depth: 2,
            reach_depth: 0,
            non_rust: NonRust::WholePackage,
            on_jev_error: OnJevError::Reach,
            without_jev: WithoutJev::Reach,
            runner: Runner::Auto,
        }
    }
}

/// Which changes feed impact detection (`[changes]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Changes {
    pub mode: ChangeMode,
    pub default_branch: String,
    pub base: String,
    pub last: usize,
    pub since: String,
    pub recent: String,
    pub max_files: usize,
    pub max_lines: usize,
    pub include_untracked: bool,
    pub files: Vec<String>,
}

impl Default for Changes {
    fn default() -> Self {
        Self {
            mode: ChangeMode::Auto,
            default_branch: "auto".into(),
            base: String::new(),
            last: 1,
            since: "midnight".into(),
            recent: "midnight".into(),
            max_files: 60,
            max_lines: 3000,
            include_untracked: true,
            files: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeMode {
    /// Uncommitted work if dirty; else the branch vs the default branch; else the last commit.
    Auto,
    /// Staged + unstaged + untracked vs HEAD.
    Uncommitted,
    /// The index vs HEAD.
    Staged,
    /// The working tree vs the index.
    Unstaged,
    /// merge-base(default branch, HEAD) .. working tree.
    Branch,
    /// The last `changes.last` commits.
    Last,
    /// Commits since `changes.since`.
    Since,
    /// `changes.base` .. HEAD or the working tree.
    Range,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StaticEvidence {
    Must,
    Boost,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NonRust {
    WholePackage,
    Jev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnJevError {
    Reach,
    Full,
    Fail,
}

/// What to select when Jev is off (no key, `--no-jev`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WithoutJev {
    /// Every test in reached crates.
    Reach,
    /// Only must-runs, tests with static evidence, rules and `tests.always`.
    Evidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Runner {
    /// `nextest` when `cargo nextest --version` succeeds, else `cargo`.
    Auto,
    Nextest,
    Cargo,
}

impl Runner {
    /// Resolves [`Runner::Auto`] to `Nextest` when `$CARGO nextest --version` succeeds, else
    /// `Cargo`; other values are returned unchanged.
    pub fn resolve(self) -> Runner {
        if self != Runner::Auto {
            return self;
        }
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let nextest = std::process::Command::new(cargo)
            .args(["nextest", "--version"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if nextest { Runner::Nextest } else { Runner::Cargo }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Paths {
    pub ignore: Vec<String>,
    pub full_run: Vec<String>,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            ignore: strings(&["**/*.md", "docs/**", ".github/**"]),
            full_run: strings(&[
                "Cargo.toml",
                "Cargo.lock",
                "rust-toolchain",
                "rust-toolchain.toml",
                ".cargo/**",
                ".config/nextest.toml",
            ]),
        }
    }
}

/// `[[rule]]`: when any `when` glob matches a changed file, run the `run` filtersets, the whole
/// `packages`, or everything (`full`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Rule {
    pub when: Vec<String>,
    pub run: Vec<String>,
    pub packages: Vec<String>,
    pub full: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tests {
    pub always: Vec<String>,
    pub never: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Jev {
    pub model: String,
    pub base_url: String,
    pub key_env: Vec<String>,
    /// `~` is expanded by [`load`].
    pub key_file: PathBuf,
    /// Each entry is one of [`VIEWS`]; [`load`] rejects anything else.
    pub views: Vec<String>,
    pub batch: usize,
    pub concurrency: usize,
    pub max_questions: usize,
    pub max_state_chars: usize,
    pub max_test_chars: usize,
    pub max_group_chars: usize,
    pub timeout_secs: u64,
    /// `~` is expanded by [`load`].
    pub cache_dir: PathBuf,
}

impl Default for Jev {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.into(),
            base_url: DEFAULT_BASE_URL.into(),
            key_env: strings(&["TYPESAFE_API_KEY", GATEWAY_KEY_ENV]),
            key_file: "~/.config/jevtest/typesafe.key".into(),
            views: strings(&VIEWS),
            batch: 100,
            concurrency: 4,
            max_questions: 6000,
            max_state_chars: 24000,
            max_test_chars: 800,
            max_group_chars: 600,
            timeout_secs: 30,
            cache_dir: "~/.cache/jevtest".into(),
        }
    }
}

impl Jev {
    /// The API key: the first non-empty env var in `key_env` order, then the contents of
    /// `key_file`. Returns the key (trimmed) and where it came from — the env var name or
    /// [`KEY_FILE_SOURCE`]. Never log the key itself.
    pub fn resolve_key(&self) -> Option<(String, &'static str)> {
        for name in &self.key_env {
            if let Ok(value) = std::env::var(name)
                && let Some(key) = trimmed(value)
            {
                return Some((key, static_name(name)));
            }
        }
        let key = trimmed(std::fs::read_to_string(&self.key_file).ok()?)?;
        Some((key, KEY_FILE_SOURCE))
    }

    /// Effective `(base_url, model)` for a key from `source`: a key from [`GATEWAY_KEY_ENV`]
    /// moves a default `base_url`/`model` to the Vercel AI Gateway; explicit values stay.
    pub fn endpoint(&self, source: &str) -> (&str, &str) {
        let gateway = source == GATEWAY_KEY_ENV;
        let base_url = if gateway && self.base_url == DEFAULT_BASE_URL {
            GATEWAY_BASE_URL
        } else {
            &self.base_url
        };
        let model = if gateway && self.model == DEFAULT_MODEL { GATEWAY_MODEL } else { &self.model };
        (base_url, model)
    }
}

/// The per-test coverage map (`[coverage]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Coverage {
    /// Relative paths are from the repo root; `~` is expanded by [`load`].
    pub map: PathBuf,
    pub policy: CoveragePolicy,
    pub max_age_commits: usize,
    /// Directory holding `llvm-profdata` and `llvm-cov`; empty = search. `~` is expanded by [`load`].
    pub llvm_bin: PathBuf,
    /// Parallel mapping jobs in `coverage build`; 0 = one per core.
    pub jobs: usize,
}

impl Default for Coverage {
    fn default() -> Self {
        Self {
            map: ".jevtest/coverage.json.gz".into(),
            policy: CoveragePolicy::Gate,
            max_age_commits: 200,
            llvm_bin: PathBuf::new(),
            jobs: 0,
        }
    }
}

/// How tests the coverage map knows are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoveragePolicy {
    /// Drop tests that execute no changed function (and have no other evidence) before Jev.
    Gate,
    /// Tests that execute a changed function always run.
    Must,
    /// Tests that execute a changed function skip screening and score +0.3.
    Boost,
    Off,
}

impl CoveragePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            CoveragePolicy::Gate => "gate",
            CoveragePolicy::Must => "must",
            CoveragePolicy::Boost => "boost",
            CoveragePolicy::Off => "off",
        }
    }
}

/// On-disk shape of `jevtest.toml`.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct File {
    select: Select,
    paths: Paths,
    #[serde(rename = "rule")]
    rules: Vec<Rule>,
    changes: Changes,
    tests: Tests,
    jev: Jev,
    coverage: Coverage,
    profile: BTreeMap<String, toml::Table>,
}

/// Loads the config: `explicit` if given (must exist), else `jevtest.toml` at `repo_root`, else
/// `.config/jevtest.toml`, else built-in defaults; then overlays `[profile.<profile>]`, validates
/// and expands `~` in `key_file`/`cache_dir`/`coverage.map`/`coverage.llvm_bin`. Errors name the
/// file and the offending key.
pub fn load(repo_root: &Path, explicit: Option<&Path>, profile: Option<&str>) -> Result<Config, String> {
    let source = match explicit {
        Some(path) if path.is_file() => Some(path.to_path_buf()),
        Some(path) => return Err(format!("config file {} not found", path.display())),
        None => [repo_root.join("jevtest.toml"), repo_root.join(".config/jevtest.toml")]
            .into_iter()
            .find(|path| path.is_file()),
    };
    let origin = source.as_deref().map_or_else(|| "built-in defaults".to_owned(), |p| p.display().to_string());

    let mut file = match &source {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {origin}: {e}"))?;
            toml::from_str::<File>(&text).map_err(|e| format!("{origin}: {}", e.to_string().trim_end()))?
        }
        None => File::default(),
    };

    if let Some(name) = profile {
        let Some(overlay) = file.profile.remove(name) else {
            let known: Vec<&str> = file.profile.keys().map(String::as_str).collect();
            return Err(if known.is_empty() {
                format!("profile `{name}` not found: {origin} defines no [profile.*]")
            } else {
                format!("profile `{name}` not found in {origin}; defined: {}", known.join(", "))
            });
        };
        apply_profile(&mut file, overlay).map_err(|e| format!("{origin}: [profile.{name}]: {e}"))?;
    }

    let mut config = Config {
        select: file.select,
        changes: file.changes,
        paths: file.paths,
        rules: file.rules,
        tests: file.tests,
        jev: file.jev,
        coverage: file.coverage,
        source,
    };
    config.validate().map_err(|e| format!("{origin}: {e}"))?;
    expand_home(&mut config.jev.key_file);
    expand_home(&mut config.jev.cache_dir);
    expand_home(&mut config.coverage.map);
    expand_home(&mut config.coverage.llvm_bin);
    Ok(config)
}

impl Config {
    /// Checks value ranges, enum-like strings and glob syntax. [`load`] calls it; call again after
    /// applying CLI overrides to reject bad flag values.
    pub fn validate(&self) -> Result<(), String> {
        let s = &self.select;
        for (key, value) in [("threshold", s.threshold), ("group_threshold", s.group_threshold), ("top_fraction", s.top_fraction)] {
            if !(0.0..=1.0).contains(&value) {
                return Err(format!("select.{key} = {value} is outside 0..=1"));
            }
        }
        let c = &self.changes;
        if c.last == 0 {
            return Err("changes.last must be at least 1".into());
        }
        if c.mode == ChangeMode::Range && c.base.trim().is_empty() {
            return Err("changes.mode = \"range\" needs changes.base (or --base / --range)".into());
        }
        for (key, value) in [("since", &c.since), ("recent", &c.recent), ("default_branch", &c.default_branch)] {
            if value.trim().is_empty() {
                return Err(format!("changes.{key} is empty"));
            }
        }
        if s.max_tests != 0 && s.min_tests > s.max_tests {
            return Err(format!("select.min_tests ({}) exceeds select.max_tests ({})", s.min_tests, s.max_tests));
        }
        let j = &self.jev;
        if j.views.is_empty() {
            return Err(format!("jev.views is empty; use any of {VIEWS:?}"));
        }
        for (i, view) in j.views.iter().enumerate() {
            if !VIEWS.contains(&view.as_str()) {
                return Err(format!("jev.views: unknown view `{view}`; expected one of {VIEWS:?}"));
            }
            if j.views[..i].contains(view) {
                return Err(format!("jev.views: `{view}` listed twice"));
            }
        }
        for (key, value) in [("batch", j.batch), ("concurrency", j.concurrency), ("timeout_secs", j.timeout_secs as usize)] {
            if value == 0 {
                return Err(format!("jev.{key} must be at least 1"));
            }
        }
        check_globs("paths.ignore", &self.paths.ignore)?;
        check_globs("paths.full_run", &self.paths.full_run)?;
        for (i, rule) in self.rules.iter().enumerate() {
            let at = format!("[[rule]] #{}", i + 1);
            if rule.when.is_empty() {
                return Err(format!("{at}: `when` is empty"));
            }
            if rule.run.is_empty() && rule.packages.is_empty() && !rule.full {
                return Err(format!("{at}: set at least one of `run`, `packages`, `full = true`"));
            }
            check_globs(&format!("{at} when"), &rule.when)?;
        }
        if self.coverage.map.as_os_str().is_empty() {
            return Err("coverage.map is empty".into());
        }
        Ok(())
    }
}

/// Overlays a `[profile.X]` table: each key replaces the same-named key of `[select]`,
/// `[changes]`, `[tests]`, `[jev]` or `[coverage]` (key sets are disjoint).
fn apply_profile(file: &mut File, overlay: toml::Table) -> Result<(), String> {
    let mut select = toml::Table::try_from(&file.select).map_err(|e| e.to_string())?;
    let mut changes = toml::Table::try_from(&file.changes).map_err(|e| e.to_string())?;
    let mut tests = toml::Table::try_from(&file.tests).map_err(|e| e.to_string())?;
    let mut jev = toml::Table::try_from(&file.jev).map_err(|e| e.to_string())?;
    let mut coverage = toml::Table::try_from(&file.coverage).map_err(|e| e.to_string())?;
    for (key, value) in overlay {
        let section = if select.contains_key(&key) {
            &mut select
        } else if changes.contains_key(&key) {
            &mut changes
        } else if tests.contains_key(&key) {
            &mut tests
        } else if jev.contains_key(&key) {
            &mut jev
        } else if coverage.contains_key(&key) {
            &mut coverage
        } else {
            return Err(format!("unknown key `{key}` (profiles take [select], [changes], [tests], [jev] and [coverage] keys)"));
        };
        section.insert(key, value);
    }
    let fix = |e: toml::de::Error| e.to_string().trim_end().replace('\n', " ");
    file.select = select.try_into().map_err(fix)?;
    file.changes = changes.try_into().map_err(fix)?;
    file.tests = tests.try_into().map_err(fix)?;
    file.jev = jev.try_into().map_err(fix)?;
    file.coverage = coverage.try_into().map_err(fix)?;
    Ok(())
}

fn check_globs(key: &str, globs: &[String]) -> Result<(), String> {
    for glob in globs {
        globset::Glob::new(glob).map_err(|e| format!("{key}: bad glob `{glob}`: {e}"))?;
    }
    Ok(())
}

/// `~` or `~/…` → `$HOME/…`; other paths (and an unset `HOME`) are left alone.
fn expand_home(path: &mut PathBuf) {
    let Ok(rest) = path.strip_prefix("~") else { return };
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else { return };
    let mut expanded = PathBuf::from(home);
    if !rest.as_os_str().is_empty() {
        expanded.push(rest);
    }
    *path = expanded;
}

fn trimmed(mut value: String) -> Option<String> {
    let end = value.trim_end().len();
    value.truncate(end);
    let start = value.len() - value.trim_start().len();
    value.drain(..start);
    (!value.is_empty()).then_some(value)
}

/// `'static` source name for an env var: the two default names are returned as themselves; any
/// other configured name is reported as [`KEY_ENV_SOURCE`].
fn static_name(name: &str) -> &'static str {
    match name {
        "TYPESAFE_API_KEY" => "TYPESAFE_API_KEY",
        GATEWAY_KEY_ENV => GATEWAY_KEY_ENV,
        _ => KEY_ENV_SOURCE,
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// The commented `jevtest.toml` written by `cargo jevtest init`: every key with its default.
pub const TEMPLATE: &str = r#"# jevtest.toml — settings for `cargo jevtest` (https://github.com/aramalipoor/rust-jevtest).
# Every key is shown commented out with its default; uncomment a line to change it.
# Precedence: built-in defaults < this file < [profile.NAME] < command-line flags.
# Unknown keys are an error.

[changes]
# What counts as "the change" whose impact is tested. Flags override: --uncommitted, --staged,
# --unstaged, --branch [BASE], --last N, --commit REV, --since WHEN, --range A..B / --base A [--head B],
# and --files PATH (narrows any of them).
# mode = "auto"               # auto | uncommitted | staged | unstaged | branch | last | since | range
#                             # auto: uncommitted work if the tree is dirty; else the branch vs the default
#                             # branch when HEAD is off it; else the last commit. A committed diff larger than
#                             # max_files/max_lines narrows to commits since `recent`, then to the last commit.
# default_branch = "auto"     # auto = origin/HEAD, origin/main, origin/master, main, master
# base = ""                   # mode = "range": the base revision
# last = 1                    # mode = "last": how many commits
# since = "midnight"          # mode = "since": git date ("midnight", "6 hours ago", "2026-10-01")
# recent = "midnight"         # auto's fallback window when the committed diff is too large
# max_files = 60              # auto size guard: changed files (ignored paths not counted)
# max_lines = 3000            # auto size guard: changed lines, both sides
# include_untracked = true    # untracked .rs files count as added (modes that include the working tree)
# files = []                  # only these paths/globs; a listed file with no diff counts as wholly changed

[select]
# top_n = 30                  # tests taken from the top of each Jev view
# top_fraction = 0.25         # ...but at most this share (0..1) of the tests that view ranked
# threshold = 0.5             # a test scoring at or above this (0..1) is always selected
# group_threshold = 0.1       # stage-1 screening cutoff per module group; groups with static evidence are kept
# max_tests = 0               # cap on selected tests (0 = no cap); drops the lowest-scored non-must picks
# min_tests = 0               # pad the selection with the next-best scores up to this many tests
# static_evidence = "must"    # must | boost | off: how tests that name a changed item are treated
# call_graph_depth = 2        # caller hops of name-based call-graph expansion inside affected crates
# reach_depth = 0             # reverse-dependency hops from changed packages (0 = all transitive)
# non_rust = "whole-package"  # whole-package | jev: how a changed non-.rs file inside a package is handled
# on_jev_error = "reach"      # reach (select every reached test) | full (run everything) | fail (exit 1)
# without_jev = "reach"       # when Jev is off (no key, --no-jev): reach (every test in reached crates) | evidence (only must-runs, static evidence, rules, tests.always)
# runner = "auto"             # auto | nextest | cargo: auto = nextest when `cargo nextest --version` succeeds, else `cargo test`

[paths]
# ignore = ["**/*.md", "docs/**", ".github/**"]   # changed files matching these globs never affect selection
# full_run = ["Cargo.toml", "Cargo.lock", "rust-toolchain", "rust-toolchain.toml", ".cargo/**", ".config/nextest.toml"]   # any change here runs the full suite

[tests]
# always = []                 # nextest filtersets always run (smoke tests)
# never = []                  # quarantined nextest filtersets; dropped unless the test itself changed

[jev]
# model = "jev-latest"                                  # Jev model id
# base_url = "https://api.typesafe.ai/v1"               # API base URL; with an AI_GATEWAY_API_KEY key the defaults become the Vercel AI Gateway
# key_env = ["TYPESAFE_API_KEY", "AI_GATEWAY_API_KEY"]  # env vars searched for the API key, in order
# key_file = "~/.config/jevtest/typesafe.key"           # file read for the key when no env var is set
# views = ["names", "body"]                             # judging views: names (test identity) and/or body (test source)
# batch = 100                                           # questions per API request
# concurrency = 4                                       # API requests in flight at once
# max_questions = 6000                                  # cap on Jev questions per run
# max_state_chars = 24000                               # diff context sent with each request, in chars
# max_test_chars = 800                                  # test source per body question, in chars
# max_group_chars = 600                                 # group description per screening question, in chars
# timeout_secs = 30                                     # per-request timeout, in seconds
# cache_dir = "~/.cache/jevtest"                        # verdict cache; `cargo jevtest cache clear` empties it

[coverage]
# Per-test coverage map from `cargo jevtest coverage build` (build it nightly on the default branch).
# map = ".jevtest/coverage.json.gz"   # gzip JSON; relative paths are from the repo root
# policy = "gate"             # gate | must | boost | off: what a test that executes a changed function gets
#                             # gate: tests the map knows that execute no changed function (and have no static
#                             #   evidence) are dropped before Jev; never when a changed item is coverage-blind
#                             # must: tests that execute a changed function always run
#                             # boost: they skip screening and score +0.3
# max_age_commits = 200       # an older map (or one not in HEAD's history) downgrades to boost
# llvm_bin = ""               # dir with llvm-profdata + llvm-cov matching rustc's LLVM; empty = search
# jobs = 0                    # parallel mapping jobs in `coverage build` (0 = one per core)

# Path-triggered must-runs: when any `when` glob matches a changed file, run the `run` filtersets,
# the whole `packages`, or everything (`full = true`). Repeat the block for more rules.
# [[rule]]
# when = ["crates/core/migrations/**"]
# run = ["package(=core) & test(/store::/)"]   # nextest filtersets
# packages = []                                # packages to run whole
# full = false                                 # true = run the full suite

# Profiles override any [select], [changes], [tests], [jev] or [coverage] key; pick one with
# --profile ci or JEVTEST_PROFILE=ci.
# [profile.ci]
# top_n = 60
# on_jev_error = "full"
"#;
