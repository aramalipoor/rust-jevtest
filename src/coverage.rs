//! The per-test coverage map: `coverage build` runs every test in its own process under an
//! instrumented build and records which workspace functions each one executed; `coverage info`
//! describes the map; `__cov-runner` is the target runner that gives each test its own profile
//! directory; [`apply`] is the selection layer that reads the map.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{BufWriter, Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::de::{IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{Coverage, CoveragePolicy};
use crate::evidence::{ChangedItem, EvidenceKind};
use crate::git::{self, FileChange};
use crate::scan::{self, Kind};
use crate::select::{Candidate, Escalation, Must};
use crate::workspace::Workspace;

pub const MAP_VERSION: u32 = 1;
/// `argv[1]` that turns this binary into the target runner `coverage build` installs.
pub const RUNNER: &str = "__cov-runner";
/// Where the runner puts each test's profile directory.
const DIR_ENV: &str = "JEVTEST_COV_DIR";
/// Score bonus for covered tests under `policy = "boost"`.
pub const COVER_BOOST: f64 = 0.3;
/// Source files outside the workspace, skipped by `llvm-cov export` to keep its output small
/// (everything else outside the workspace is dropped when mapping).
const FOREIGN_FILES: &str = r"/rustc/|/\.cargo/registry/|/\.cargo/git/|/\.rustup/|/lib/rustlib/";

#[derive(Serialize, Deserialize)]
pub struct Map {
    pub version: u32,
    pub commit: String,
    pub built_at: String,
    pub rustc: String,
    pub functions: Vec<Function>,
    pub tests: Vec<MapTest>,
}

/// A workspace function (closures and generic instances fold into their enclosing item).
#[derive(Serialize, Deserialize)]
pub struct Function {
    /// Repo-relative.
    pub file: String,
    /// `Type::method` or `name`.
    pub name: String,
    pub start: u32,
    pub end: u32,
}

#[derive(Serialize, Deserialize)]
pub struct MapTest {
    pub package: String,
    pub binary_id: String,
    /// The nextest test name, e.g. `store::tests::opens`.
    pub name: String,
    /// Indices into [`Map::functions`] of every function it executed.
    pub functions: Vec<u32>,
}

impl Map {
    pub fn load(path: &Path) -> Result<Map, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let mut text = Vec::new();
        GzDecoder::new(std::io::BufReader::new(file))
            .read_to_end(&mut text)
            .map_err(|e| format!("cannot decompress {}: {e}", path.display()))?;
        let map: Map = serde_json::from_slice(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
        if map.version != MAP_VERSION {
            return Err(format!("{} is map version {}; this jevtest reads {MAP_VERSION} (rebuild it)", path.display(), map.version));
        }
        Ok(map)
    }

    /// Writes the gzip JSON atomically; returns its size in bytes.
    fn save(&self, path: &Path) -> Result<u64, String> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let tmp = path.with_extension("tmp");
        let fail = |e: &dyn std::fmt::Display| format!("cannot write {}: {e}", tmp.display());
        let file = std::fs::File::create(&tmp).map_err(|e| fail(&e))?;
        let mut gz = GzEncoder::new(BufWriter::new(file), Compression::default());
        serde_json::to_writer(&mut gz, self).map_err(|e| fail(&e))?;
        gz.finish().map_err(|e| fail(&e))?.flush().map_err(|e| fail(&e))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("cannot move {} to {}: {e}", tmp.display(), path.display()))?;
        std::fs::metadata(path).map(|m| m.len()).map_err(|e| e.to_string())
    }
}

/// Lookups the selection layer needs.
struct Index<'a> {
    map: &'a Map,
    /// (file, name) → function.
    by_key: HashMap<(&'a str, &'a str), u32>,
    /// package → its tests.
    by_package: HashMap<&'a str, Vec<u32>>,
}

impl<'a> Index<'a> {
    fn new(map: &'a Map) -> Self {
        let by_key = map.functions.iter().enumerate().map(|(i, f)| ((f.file.as_str(), f.name.as_str()), i as u32)).collect();
        let mut by_package: HashMap<&str, Vec<u32>> = HashMap::new();
        for (i, t) in map.tests.iter().enumerate() {
            by_package.entry(t.package.as_str()).or_default().push(i as u32);
        }
        Index { map, by_key, by_package }
    }

    fn function(&self, paths: &[&str], key: &str) -> Option<u32> {
        paths.iter().find_map(|p| self.by_key.get(&(*p, key)).copied())
    }

    /// Functions in `file` or in any file under `dir` (a `/`-terminated prefix).
    fn functions_under(&self, file: &str, dir: &str) -> Vec<u32> {
        let f = &self.map.functions;
        (0..f.len() as u32).filter(|&i| f[i as usize].file == file || f[i as usize].file.starts_with(dir)).collect()
    }

    /// Map tests named `name` in `package`; failing that, its `name::…` cases (rstest, test_case).
    fn tests(&self, package: &str, name: &str) -> Vec<u32> {
        let Some(all) = self.by_package.get(package) else { return Vec::new() };
        let t = &self.map.tests;
        let exact: Vec<u32> = all.iter().copied().filter(|&i| t[i as usize].name == name).collect();
        if !exact.is_empty() {
            return exact;
        }
        all.iter()
            .copied()
            .filter(|&i| t[i as usize].name.strip_prefix(name).is_some_and(|rest| rest.starts_with("::")))
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Selection layer.

/// A coverage-blind changed item: coverage cannot tell which tests it affects.
pub struct Blind {
    pub package: usize,
    pub item: String,
    pub why: String,
}

/// Changed lines outside every item in one side of a changed `.rs` file.
pub struct LooseFile {
    pub pkg: usize,
    pub path: String,
    pub loose: scan::Loose,
}

/// What the coverage layer did in this run.
pub struct State {
    /// The map path, when one was read.
    pub map: Option<String>,
    pub commit: Option<String>,
    /// Commits from the map's commit to the change base.
    pub age_commits: Option<u64>,
    pub requested: CoveragePolicy,
    /// `Off` when the layer was skipped.
    pub used: CoveragePolicy,
    pub covered: usize,
    pub gated_out: usize,
    /// Candidates the map does not know (never gated).
    pub unknown: usize,
    pub blind: Vec<Blind>,
    /// Packages exempt from gating: those with blind items and their reverse dependencies.
    pub no_gate: Vec<usize>,
    /// Why the layer was skipped or downgraded.
    pub reason: Option<String>,
}

/// Reads the map (unless the policy is off) and checks its age against `base`. A missing or
/// unreadable map skips the layer; a stale one downgrades `gate`/`must` to `boost`.
pub fn open(root: &Path, cfg: &Coverage, base: &str) -> (Option<Map>, State) {
    let mut st = State {
        map: None,
        commit: None,
        age_commits: None,
        requested: cfg.policy,
        used: CoveragePolicy::Off,
        covered: 0,
        gated_out: 0,
        unknown: 0,
        blind: Vec::new(),
        no_gate: Vec::new(),
        reason: None,
    };
    if cfg.policy == CoveragePolicy::Off {
        st.reason = Some("coverage.policy = \"off\"".into());
        return (None, st);
    }
    let path = root.join(&cfg.map);
    if !path.is_file() {
        st.reason = Some("no map (cargo jevtest coverage build)".into());
        return (None, st);
    }
    let map = match Map::load(&path) {
        Ok(map) => map,
        Err(e) => {
            st.reason = Some(format!("map unreadable: {e}"));
            return (None, st);
        }
    };
    let shown = path.strip_prefix(root).unwrap_or(&path).display().to_string();
    st.map = Some(shown);
    st.commit = Some(map.commit.clone());
    let age = age(root, &map.commit, base, cfg.max_age_commits);
    st.age_commits = age.commits;
    st.used = cfg.policy;
    if let Some(why) = age.stale
        && matches!(cfg.policy, CoveragePolicy::Gate | CoveragePolicy::Must)
    {
        st.used = CoveragePolicy::Boost;
        st.reason = Some(format!("stale map ({why}): {} → boost", cfg.policy.as_str()));
    }
    (Some(map), st)
}

/// Marks covered and gated candidates per `st.used`.
#[allow(clippy::too_many_arguments)]
pub fn apply(
    st: &mut State,
    map: &Map,
    ws: &Workspace,
    files: &[FileChange],
    items: &[ChangedItem],
    loose: &[LooseFile],
    escalations: &[Escalation],
    candidates: &mut [Candidate],
) {
    let idx = Index::new(map);
    let old_of: HashMap<&str, &str> = files
        .iter()
        .filter_map(|f| match (&f.new_path, &f.old_path) {
            (Some(n), Some(o)) if n != o => Some((n.as_str(), o.as_str())),
            _ => None,
        })
        .collect();
    let mut changed: HashSet<u32> = HashSet::new();
    let mut blind: Vec<Blind> = Vec::new();
    for item in items {
        let Some(pkg) = ws.owner(&item.path) else { continue };
        let key = scan::item_key(item.owner.as_deref(), &item.name);
        let paths: Vec<&str> = std::iter::once(item.path.as_str()).chain(old_of.get(item.path.as_str()).copied()).collect();
        let mut mark = |why: String| blind.push(Blind { package: pkg, item: format!("{}: {key}", item.path), why });
        if ws.packages[pkg].proc_macro {
            mark("proc-macro crate (its code runs inside rustc)".into());
            continue;
        }
        match &item.kind {
            Kind::Fn { konst: true } => mark("const fn (may run at compile time)".into()),
            Kind::Fn { .. } => match idx.function(&paths, &key) {
                Some(f) => {
                    changed.insert(f);
                }
                None => mark("fn not in the map (new, or no test ran it)".into()),
            },
            Kind::Impl { methods } if methods.is_empty() => mark("impl without methods".into()),
            Kind::Impl { methods } => {
                let found: Vec<Option<u32>> = methods.iter().map(|m| idx.function(&paths, &format!("{}::{m}", item.name))).collect();
                match methods.iter().zip(&found).find(|(_, f)| f.is_none()) {
                    Some((m, _)) => mark(format!("impl header changed and its method {}::{m} is not in the map", item.name)),
                    None => changed.extend(found.into_iter().flatten()),
                }
            }
            Kind::Other(what) => mark(format!("{what}: no body for coverage to see")),
            Kind::Test | Kind::Mod => {}
        }
    }
    for lf in loose {
        let mut mark = |item: String, why: &str| blind.push(Blind { package: lf.pkg, item, why: why.into() });
        if ws.packages[lf.pkg].proc_macro {
            mark(lf.path.clone(), "proc-macro crate (its code runs inside rustc)");
        } else if let Some(line) = lf.loose.code {
            mark(format!("{}:{line}", lf.path), "top-level change (pub use, mod, attribute or item macro)");
        } else if lf.loose.private_use {
            let fns = idx.functions_under(&lf.path, &scope_dir(&lf.path, &ws.packages[lf.pkg].dir));
            if fns.is_empty() {
                mark(lf.path.clone(), "`use` changed in a file the map has never seen");
            } else {
                changed.extend(fns);
            }
        }
    }
    for e in escalations.iter().filter(|e| e.kind == "whole") {
        if let Some(pkg) = e.package {
            blind.push(Blind { package: pkg, item: e.file.clone(), why: e.reason.clone() });
        }
    }
    let mut roots: Vec<usize> = blind.iter().map(|b| b.package).collect();
    roots.sort_unstable();
    roots.dedup();
    st.no_gate = ws.affected(&roots, 0).into_iter().map(|(p, _)| p).collect();
    st.blind = blind;
    if !st.no_gate.is_empty() && st.used == CoveragePolicy::Gate {
        let names: Vec<&str> = st.no_gate.iter().map(|&p| ws.packages[p].name.as_str()).collect();
        let why = format!("no gating in [{}]: coverage-blind changes", names.join(", "));
        st.reason = Some(match st.reason.take() {
            Some(r) => format!("{r}; {why}"),
            None => why,
        });
    }

    for c in candidates.iter_mut() {
        let full = if c.test.module.is_empty() { c.test.name.clone() } else { format!("{}::{}", c.test.module, c.test.name) };
        let tests = idx.tests(&ws.packages[c.pkg].name, &full);
        if tests.is_empty() {
            st.unknown += 1;
            continue;
        }
        let hit: BTreeSet<u32> =
            tests.iter().flat_map(|&t| &map.tests[t as usize].functions).copied().filter(|f| changed.contains(f)).collect();
        let mut names: Vec<String> = hit.iter().map(|&f| map.functions[f as usize].name.clone()).collect();
        names.dedup();
        if names.is_empty() {
            if st.used == CoveragePolicy::Gate && c.must.is_none() && c.evidence.is_none() && !st.no_gate.contains(&c.pkg) {
                c.gated = true;
                st.gated_out += 1;
            }
        } else {
            st.covered += 1;
            if c.must.is_none() {
                if st.used == CoveragePolicy::Must {
                    c.must = Some(Must::Covered);
                }
                if c.evidence.is_none() {
                    c.evidence = Some((EvidenceKind::Covered(names.len() as u32), names.clone()));
                }
            }
        }
        c.coverage = Some(names);
    }
}

/// The directory whose files a private `use` in `path` can reach through `use super::*`: the
/// crate or module directory for a crate root or `mod.rs`, else `foo/` beside `foo.rs`.
fn scope_dir(path: &str, pkg_dir: &str) -> String {
    let rel = if pkg_dir.is_empty() { path } else { path.strip_prefix(pkg_dir).map_or(path, |r| r.trim_start_matches('/')) };
    let file = rel.rsplit('/').next().unwrap_or(rel);
    let parts: Vec<&str> = rel.split('/').collect();
    let root = matches!(file, "lib.rs" | "main.rs" | "mod.rs" | "build.rs")
        || matches!(parts.as_slice(), ["tests" | "benches" | "examples", _] | ["src", "bin", _]);
    if root {
        path.rsplit_once('/').map_or_else(String::new, |(dir, _)| format!("{dir}/"))
    } else {
        format!("{}/", path.trim_end_matches(".rs"))
    }
}

pub struct Age {
    pub commits: Option<u64>,
    pub stale: Option<String>,
}

/// How far `commit` is behind `base` (HEAD when `base` is not a commit), and whether that makes
/// the map stale.
pub fn age(root: &Path, commit: &str, base: &str, max: usize) -> Age {
    let short = &commit[..commit.len().min(12)];
    if git::git(root, &["merge-base", "--is-ancestor", commit, "HEAD"]).is_err() {
        return Age { commits: None, stale: Some(format!("built at {short}, which is not in HEAD's history")) };
    }
    let base = if git::rev_exists(root, base) { base } else { "HEAD" };
    let commits = git::git(root, &["rev-list", "--count", &format!("{commit}..{base}")]).ok().and_then(|s| s.trim().parse::<u64>().ok());
    let stale = commits.filter(|&n| n > max as u64).map(|n| format!("{} behind, max_age_commits = {max}", n_commits(n)));
    Age { commits, stale }
}

/// `1 commit`, `3 commits`.
pub fn n_commits(n: u64) -> String {
    format!("{n} commit{}", if n == 1 { "" } else { "s" })
}

// ---------------------------------------------------------------------------------------------
// The target runner.

/// `cargo-jevtest __cov-runner <test binary> <args…>`: under `coverage build`, points the test's
/// `LLVM_PROFILE_FILE` at its own directory (with a `meta.json` naming the test), then `exec`s the
/// binary. Listing invocations run untouched.
pub fn runner(args: &[OsString]) -> ExitCode {
    let Some((bin, rest)) = args.split_first() else {
        eprintln!("jevtest {RUNNER}: missing the test binary");
        return ExitCode::from(2);
    };
    let mut cmd = Command::new(bin);
    cmd.args(rest);
    if let Some(dir) = std::env::var_os(DIR_ENV)
        && let Some((binary_id, name)) = test_identity(bin, rest)
    {
        let package = std::env::var("CARGO_PKG_NAME").unwrap_or_default();
        let tdir = Path::new(&dir).join(test_key(&binary_id, &name));
        let meta = serde_json::json!({"package": package, "binary_id": binary_id, "name": name, "binary": bin.to_string_lossy()});
        if let Err(e) = std::fs::create_dir_all(&tdir).and_then(|()| std::fs::write(tdir.join("meta.json"), meta.to_string())) {
            eprintln!("jevtest {RUNNER}: cannot record {name}: {e}");
        }
        cmd.env("LLVM_PROFILE_FILE", tdir.join("%p.profraw"));
    }
    exec(cmd)
}

/// (binary id, test name) when nextest runs one test; `None` when it lists tests.
fn test_identity(bin: &OsString, rest: &[OsString]) -> Option<(String, String)> {
    if rest.iter().any(|a| a == "--list") {
        return None;
    }
    let name = std::env::var("NEXTEST_TEST_NAME").ok().filter(|n| !n.is_empty()).or_else(|| {
        rest.iter().map(|a| a.to_string_lossy()).find(|a| !a.starts_with('-')).map(|a| a.into_owned())
    })?;
    let binary_id = std::env::var("NEXTEST_BINARY_ID")
        .ok()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| Path::new(bin).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
    Some((binary_id, name))
}

#[cfg(unix)]
fn exec(mut cmd: Command) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let e = cmd.exec();
    eprintln!("jevtest {RUNNER}: cannot run the test binary: {e}");
    ExitCode::from(126)
}

#[cfg(not(unix))]
fn exec(mut cmd: Command) -> ExitCode {
    match cmd.status() {
        Ok(s) => ExitCode::from(s.code().map_or(1, |c| c.clamp(0, 255) as u8)),
        Err(e) => {
            eprintln!("jevtest {RUNNER}: cannot run the test binary: {e}");
            ExitCode::from(126)
        }
    }
}

/// A file-name-safe, injective key for one test: `[A-Za-z0-9_]` kept, other bytes `~hh`; long
/// keys keep a prefix plus a hash.
fn test_key(binary_id: &str, name: &str) -> String {
    let raw = format!("{binary_id}${name}");
    let mut key = String::with_capacity(raw.len() * 2);
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || b == b'_' {
            key.push(b as char);
        } else {
            let _ = write!(key, "~{b:02x}");
        }
    }
    if key.len() > 180 {
        key.truncate(100);
        key.push('~');
        for b in Sha256::digest(raw.as_bytes()).iter().take(10) {
            let _ = write!(key, "{b:02x}");
        }
    }
    key
}

// ---------------------------------------------------------------------------------------------
// Toolchain.

struct Rustc {
    host: String,
    release: String,
    /// `LLVM version:` of `rustc -vV`, e.g. `22.1.8`.
    llvm: String,
    sysroot: PathBuf,
}

impl Rustc {
    fn query(root: &Path) -> Result<Self, String> {
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let run = |args: &[&str]| -> Result<String, String> {
            let out = Command::new(&rustc)
                .args(args)
                .current_dir(root)
                .output()
                .map_err(|e| format!("cannot run rustc: {e}"))?;
            if !out.status.success() {
                return Err(format!("rustc {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let vv = run(&["-vV"])?;
        let field = |name: &str| vv.lines().find_map(|l| l.strip_prefix(name)).map(|v| v.trim().to_owned());
        Ok(Rustc {
            host: field("host:").ok_or("rustc -vV printed no host")?,
            release: field("release:").unwrap_or_default(),
            llvm: field("LLVM version:").ok_or("rustc -vV printed no LLVM version")?,
            sysroot: PathBuf::from(run(&["--print", "sysroot"])?.trim()),
        })
    }
}

fn major(version: &str) -> &str {
    version.split('.').next().unwrap_or(version)
}

/// The LLVM version a tool prints (`LLVM version 22.1.8…`), if it runs.
fn tool_version(tool: &Path) -> Option<String> {
    let out = Command::new(tool).arg("--version").stderr(Stdio::null()).output().ok().filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&out.stdout);
    let at = text.find("LLVM version ")? + "LLVM version ".len();
    Some(text[at..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect())
}

struct Llvm {
    profdata: PathBuf,
    cov: PathBuf,
    version: String,
}

impl Llvm {
    /// `coverage.llvm_bin` if set; else the rustc sysroot's `llvm-tools` component, `PATH`, then
    /// Homebrew's keg-only llvm. Both tools must match rustc's LLVM major version.
    fn find(configured: &Path, rustc: &Rustc) -> Result<Self, String> {
        let want = major(&rustc.llvm);
        let mut dirs: Vec<Option<PathBuf>> = Vec::new();
        if configured.as_os_str().is_empty() {
            dirs.push(Some(rustc.sysroot.join("lib/rustlib").join(&rustc.host).join("bin")));
            dirs.push(None);
            dirs.extend(["/opt/homebrew/opt/llvm/bin", "/usr/local/opt/llvm/bin"].map(|d| Some(PathBuf::from(d))));
        } else {
            dirs.push(Some(configured.to_path_buf()));
        }
        let mut tried = Vec::new();
        for dir in dirs {
            let tool = |name: &str| {
                let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
                dir.as_ref().map_or_else(|| PathBuf::from(&file), |d| d.join(&file))
            };
            let (profdata, cov) = (tool("llvm-profdata"), tool("llvm-cov"));
            let at = dir.as_ref().map_or_else(|| "PATH".to_owned(), |d| d.display().to_string());
            match (tool_version(&profdata), tool_version(&cov)) {
                (Some(a), Some(b)) if major(&a) == want && major(&b) == want => return Ok(Llvm { profdata, cov, version: a }),
                (Some(a), Some(_)) => tried.push(format!("{at} (LLVM {a})")),
                _ => tried.push(format!("{at} (not found)")),
            }
        }
        Err(format!(
            "no llvm-profdata + llvm-cov for LLVM {want} (rustc {} uses LLVM {}); tried {}. Fix: `rustup component add llvm-tools`, or set coverage.llvm_bin = \"/path/to/llvm/bin\" (LLVM {want}) in jevtest.toml",
            rustc.release,
            rustc.llvm,
            tried.join(", ")
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// `coverage build`.

#[derive(Deserialize)]
struct Meta {
    package: String,
    binary_id: String,
    name: String,
    binary: PathBuf,
}

/// One test that ran under the runner.
struct Ran {
    meta: Meta,
    profiles: Vec<PathBuf>,
}

/// `llvm-cov export` JSON, reduced to what mapping needs.
#[derive(Deserialize)]
struct Export {
    data: Vec<ExportData>,
}

#[derive(Deserialize)]
struct ExportData {
    functions: Vec<ExportFn>,
}

#[derive(Deserialize)]
struct ExportFn {
    name: String,
    regions: Vec<Region>,
    filenames: Vec<String>,
}

/// `[line_start, col_start, line_end, col_end, count, file_id, …]`.
struct Region {
    line_start: u32,
    line_end: u32,
    file_id: usize,
}

impl<'de> Deserialize<'de> for Region {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Region;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an llvm-cov region array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Region, A::Error> {
                let mut v = [0u64; 6];
                for (i, slot) in v.iter_mut().enumerate() {
                    *slot = seq.next_element()?.ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                }
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Region { line_start: v[0] as u32, line_end: v[2] as u32, file_id: v[5] as usize })
            }
        }
        d.deserialize_seq(V)
    }
}

/// Runs `f` over `items` on `jobs` threads; results in input order.
fn parallel<T: Sync, R: Send>(items: &[T], jobs: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let mut out: Vec<Option<R>> = (0..items.len()).map(|_| None).collect();
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..jobs.clamp(1, items.len().max(1)))
            .map(|_| {
                s.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else { break };
                        done.push((i, f(item)));
                    }
                    done
                })
            })
            .collect();
        for w in workers {
            for (i, r) in w.join().unwrap_or_else(|p| std::panic::resume_unwind(p)) {
                out[i] = Some(r);
            }
        }
    });
    out.into_iter().map(|r| r.expect("every item was processed")).collect()
}

fn run_tool(cmd: &mut Command) -> Result<Vec<u8>, String> {
    let out = cmd.stdin(Stdio::null()).output().map_err(|e| format!("cannot run {:?}: {e}", cmd.get_program()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{} failed: {}", Path::new(cmd.get_program()).display(), err.trim()));
    }
    Ok(out.stdout)
}

/// Functions a test executed: PGO names whose entry counter is non-zero, from
/// `llvm-profdata merge -sparse -text` over the test's profiles.
fn executed(llvm: &Llvm, profiles: &[PathBuf]) -> Result<Vec<String>, String> {
    let text = run_tool(Command::new(&llvm.profdata).args(["merge", "-sparse", "-text", "-o", "-"]).args(profiles))?;
    let text = String::from_utf8_lossy(&text);
    let mut names = Vec::new();
    for record in text.split("\n\n") {
        let mut lines = record.lines().filter(|l| !l.is_empty() && !l.starts_with(':'));
        let Some(name) = lines.next().filter(|l| !l.starts_with('#')) else { continue };
        let mut lines = lines.skip_while(|l| *l != "# Counter Values:");
        if lines.next().is_some() && lines.next().and_then(|c| c.trim().parse::<u64>().ok()).is_some_and(|c| c > 0) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// Every function of `object` in a workspace file: (PGO name, repo-relative file, first and last line).
fn exported(llvm: &Llvm, profile: &Path, object: &Path, root: &Path) -> Result<Vec<(String, String, u32, u32)>, String> {
    let json = run_tool(
        Command::new(&llvm.cov)
            .args(["export", "-format=text", "-skip-expansions", "-skip-branches"])
            .arg(format!("-ignore-filename-regex={FOREIGN_FILES}"))
            .arg(format!("-instr-profile={}", profile.display()))
            .arg(object),
    )?;
    let export: Export = serde_json::from_slice(&json).map_err(|e| format!("cannot parse llvm-cov export of {}: {e}", object.display()))?;
    let mut out = Vec::new();
    for f in export.data.into_iter().flat_map(|d| d.functions) {
        let Some(first) = f.regions.first() else { continue };
        let Some(file) = f.filenames.get(first.file_id) else { continue };
        let path = Path::new(file);
        let rel = if path.is_absolute() { path.strip_prefix(root).ok() } else { Some(path) };
        let Some(rel) = rel.and_then(Path::to_str) else { continue };
        let own = f.regions.iter().filter(|r| r.file_id == first.file_id);
        let start = own.clone().map(|r| r.line_start).min().unwrap_or(0);
        let end = own.map(|r| r.line_end).max().unwrap_or(start);
        out.push((f.name, rel.to_owned(), start, end));
    }
    Ok(out)
}

/// A symbol in the jevtest item form, for functions syn cannot place: `rustc-demangle`, then
/// `Type::method` or `name` (generics, hashes and closures dropped).
fn reduce(symbol: &str) -> String {
    // PGO names of local symbols carry a `file;` prefix.
    let symbol = symbol.rsplit_once(';').map_or(symbol, |(_, s)| s);
    let demangled = format!("{:#}", rustc_demangle::demangle(symbol));
    let mut segs: Vec<&str> = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    let bytes = demangled.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => depth += 1,
            b'>' if i == 0 || bytes[i - 1] != b'-' => depth -= 1,
            b':' if depth == 0 && bytes.get(i + 1) == Some(&b':') => {
                segs.push(&demangled[start..i]);
                start = i + 2;
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    segs.push(&demangled[start..]);
    let mut owner: Option<&str> = None;
    let mut path: Vec<&str> = Vec::new();
    for (i, seg) in segs.iter().enumerate() {
        if seg.starts_with('{') {
            break; // `{closure#0}`, `{shim:…}`: the enclosing fn
        }
        if let Some(inner) = seg.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
            if i == 0 {
                let ty = inner.split(" as ").next().unwrap_or(inner);
                let ty = ty.trim_start_matches('&').trim_start_matches("mut ").trim_start_matches("dyn ");
                let ty = ty.split('<').next().unwrap_or(ty);
                owner = ty.rsplit("::").next();
            }
            continue; // generic arguments
        }
        path.push(seg);
    }
    let Some(&name) = path.last() else { return demangled };
    match owner {
        Some(o) if path.len() == 1 => format!("{o}::{name}"),
        _ => match path.len().checked_sub(2).map(|i| path[i]) {
            Some(t) if t.starts_with(|c: char| c.is_ascii_uppercase()) => format!("{t}::{name}"),
            _ => name.to_owned(),
        },
    }
}

/// Builds the map: instrumented `cargo nextest run` (one process per test) under the runner, then
/// per test the executed functions, resolved to workspace items through each binary's
/// `llvm-cov export`.
pub fn build(root: &Path, cfg: &Coverage, out: Option<&Path>, args: &[String]) -> Result<(), String> {
    let started = Instant::now();
    let ws = Workspace::load(root)?;
    let rustc = Rustc::query(root)?;
    let llvm = Llvm::find(&cfg.llvm_bin, &rustc)?;
    let commit = git::rev_parse(root, "HEAD")?;
    let short = &commit[..12];
    if !git::git(root, &["status", "--porcelain", "--untracked-files=no"])?.trim().is_empty() {
        eprintln!("jevtest: warning: uncommitted changes; the map is stamped with HEAD {short} but built from the working tree");
    }
    let path = out.map_or_else(|| root.join(&cfg.map), Path::to_path_buf);
    let target = ws.target_dir.join("jevtest-cov");
    let profiles = target.join("profiles");
    let tests_dir = profiles.join("tests");
    let _ = std::fs::remove_dir_all(&profiles);
    std::fs::create_dir_all(&tests_dir).map_err(|e| format!("cannot create {}: {e}", tests_dir.display()))?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate jevtest itself: {e}"))?;
    let exe = exe.to_str().filter(|s| !s.contains(char::is_whitespace)).ok_or_else(|| {
        format!("the jevtest binary path {} contains whitespace or non-UTF-8; cargo cannot use it as a target runner", exe.display())
    })?;
    let runner_var = format!("CARGO_TARGET_{}_RUNNER", rustc.host.to_uppercase().replace(['-', '.'], "_"));

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(&cargo);
    cmd.args(["nextest", "run"]);
    let has = |flags: &[&str]| args.iter().any(|a| flags.iter().any(|f| a == f || a.strip_prefix(f).is_some_and(|r| r.starts_with('='))));
    if !has(&["-p", "--package", "--workspace", "--all"]) {
        cmd.arg("--workspace");
    }
    if !has(&["--fail-fast", "--no-fail-fast"]) {
        cmd.arg("--no-fail-fast");
    }
    // Precedence as cargo has it: CARGO_ENCODED_RUSTFLAGS, then RUSTFLAGS, then config files
    // (which a `--config` array joins).
    if let Some(enc) = std::env::var_os("CARGO_ENCODED_RUSTFLAGS") {
        let mut enc = enc;
        if !enc.is_empty() {
            enc.push("\x1f");
        }
        enc.push("-C\x1finstrument-coverage");
        cmd.env("CARGO_ENCODED_RUSTFLAGS", enc);
    } else if let Some(mut flags) = std::env::var_os("RUSTFLAGS") {
        flags.push(" -C instrument-coverage");
        cmd.env("RUSTFLAGS", flags);
    } else {
        cmd.args(["--config", r#"build.rustflags=["-C","instrument-coverage"]"#]);
    }
    cmd.args(args)
        .current_dir(root)
        .env("CARGO_TARGET_DIR", &target)
        .env("LLVM_PROFILE_FILE", profiles.join("other").join("%p-%m.profraw"))
        .env(DIR_ENV, &tests_dir)
        .env(&runner_var, format!("{exe} {RUNNER}"))
        .stdin(Stdio::null());
    eprintln!(
        "coverage: rustc {} (LLVM {}), llvm tools {} in {}; target dir {}",
        rustc.release,
        rustc.llvm,
        llvm.version,
        llvm.profdata.parent().map_or_else(|| "PATH".to_owned(), |p| p.display().to_string()),
        target.display()
    );
    let status = cmd.status().map_err(|e| format!("cannot run cargo nextest: {e}"))?;
    let ran_secs = started.elapsed().as_secs_f64();

    // The tests the runner saw.
    let mut ran: Vec<Ran> = Vec::new();
    for entry in std::fs::read_dir(&tests_dir).map_err(|e| e.to_string())?.flatten() {
        let dir = entry.path();
        let Ok(text) = std::fs::read(dir.join("meta.json")) else { continue };
        let Ok(meta) = serde_json::from_slice::<Meta>(&text) else { continue };
        let mut profiles: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "profraw"))
            .collect();
        profiles.sort();
        ran.push(Ran { meta, profiles });
    }
    if ran.iter().all(|r| r.profiles.is_empty()) {
        return Err(format!(
            "no test wrote coverage data (cargo nextest: {status}). If the build failed, see above; a `target.<triple>.rustflags` in .cargo/config overrides build.rustflags, so set RUSTFLAGS=\"…\" for those flags instead"
        ));
    }
    ran.sort_by(|a, b| (&a.meta.package, &a.meta.binary_id, &a.meta.name).cmp(&(&b.meta.package, &b.meta.binary_id, &b.meta.name)));
    let without = ran.iter().filter(|r| r.profiles.is_empty()).count();
    ran.retain(|r| !r.profiles.is_empty());

    let jobs = if cfg.jobs == 0 { std::thread::available_parallelism().map_or(1, |n| n.get()) } else { cfg.jobs };
    eprintln!("coverage: mapping {} tests on {jobs} jobs", ran.len());
    let mapping = Instant::now();

    // Any profile serves `llvm-cov export`: functions it lacks export with zero counts.
    let any = profiles.join("any.profdata");
    run_tool(Command::new(&llvm.profdata).args(["merge", "-sparse", "-o"]).arg(&any).args(&ran[0].profiles))?;

    let names: Vec<Result<Vec<String>, String>> = parallel(&ran, jobs, |r| {
        let names = executed(&llvm, &r.profiles);
        for p in &r.profiles {
            let _ = std::fs::remove_file(p);
        }
        names
    });

    // Objects: every test binary, plus the workspace's own executables, which tests may spawn
    // (they inherit the test's profile path).
    let mut objects: BTreeSet<PathBuf> = ran.iter().map(|r| r.meta.binary.clone()).collect();
    let profile_dirs: BTreeSet<PathBuf> = objects
        .iter()
        .filter_map(|b| b.parent().filter(|d| d.file_name().is_some_and(|n| n == "deps")).and_then(Path::parent).map(Path::to_path_buf))
        .collect();
    for dir in &profile_dirs {
        for p in &ws.packages {
            for bin in &p.bins {
                let exe = dir.join(format!("{bin}{}", std::env::consts::EXE_SUFFIX));
                if exe.is_file() {
                    objects.insert(exe);
                }
            }
        }
    }
    let objects: Vec<PathBuf> = objects.into_iter().collect();
    let exports = parallel(&objects, jobs, |o| exported(&llvm, &any, o, root));

    // Resolve every exported function to a workspace item.
    let target_rel = ws.target_dir.strip_prefix(root).ok().map(|p| format!("{}/", p.display()));
    let mut parsed: HashMap<String, Option<Vec<scan::Named>>> = HashMap::new();
    let mut functions: Vec<Function> = Vec::new();
    let mut by_key: HashMap<(String, String), u32> = HashMap::new();
    let mut table: HashMap<String, u32> = HashMap::new();
    for (object, export) in objects.iter().zip(exports) {
        let export = match export {
            Ok(e) => e,
            Err(e) => {
                eprintln!("jevtest: warning: skipping {}: {e}", object.display());
                continue;
            }
        };
        for (pgo, file, start, end) in export {
            if table.contains_key(&pgo)
                || ws.owner(&file).is_none()
                || target_rel.as_deref().is_some_and(|t| file.starts_with(t))
            {
                continue;
            }
            let named = parsed.entry(file.clone()).or_insert_with(|| {
                let src = std::fs::read_to_string(root.join(&file)).ok()?;
                let named = syn::parse_file(&src).ok().map(|f| scan::named(&f, &[]));
                proc_macro2::extra::invalidate_current_thread_spans();
                named
            });
            let item = named.as_deref().and_then(|n| scan::innermost(n, start)).filter(|n| n.kind != Kind::Mod);
            let (key, s, e) = item.map_or_else(|| (reduce(&pgo), start, end), |n| (n.key(), n.start, n.end));
            let id = *by_key.entry((file.clone(), key.clone())).or_insert_with(|| {
                functions.push(Function { file, name: key, start: s, end: e });
                functions.len() as u32 - 1
            });
            table.insert(pgo, id);
        }
    }

    // Stable order: functions by file and line; tests already sorted.
    let mut order: Vec<u32> = (0..functions.len() as u32).collect();
    order.sort_by(|&a, &b| {
        let (fa, fb) = (&functions[a as usize], &functions[b as usize]);
        (&fa.file, fa.start, &fa.name).cmp(&(&fb.file, fb.start, &fb.name))
    });
    let mut remap = vec![0u32; functions.len()];
    for (new, &old) in order.iter().enumerate() {
        remap[old as usize] = new as u32;
    }
    let mut slots: Vec<Option<Function>> = functions.into_iter().map(Some).collect();
    let functions: Vec<Function> = order.iter().map(|&i| slots[i as usize].take().expect("each function once")).collect();

    let mut tests = Vec::with_capacity(ran.len());
    let mut failed = 0usize;
    for (r, names) in ran.into_iter().zip(names) {
        let names = match names {
            Ok(n) => n,
            Err(e) => {
                failed += 1;
                eprintln!("jevtest: warning: {} {}: {e}", r.meta.binary_id, r.meta.name);
                continue;
            }
        };
        let mut ids: Vec<u32> = names.iter().filter_map(|n| table.get(n)).map(|&i| remap[i as usize]).collect();
        ids.sort_unstable();
        ids.dedup();
        tests.push(MapTest { package: r.meta.package, binary_id: r.meta.binary_id, name: r.meta.name, functions: ids });
    }
    let files: HashSet<&str> = functions.iter().map(|f| f.file.as_str()).collect();
    let (nfiles, nfunctions) = (files.len(), functions.len());
    let map = Map {
        version: MAP_VERSION,
        commit,
        built_at: rfc3339(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())),
        rustc: format!("rustc {} (LLVM {})", rustc.release, rustc.llvm),
        functions,
        tests,
    };
    let size = map.save(&path)?;
    let _ = std::fs::remove_dir_all(&profiles);
    if !status.success() {
        eprintln!("coverage: cargo nextest {status}; failing tests are mapped too");
    }
    let mut skipped = String::new();
    if without + failed > 0 {
        let _ = write!(skipped, " ({} without profile data)", without + failed);
    }
    eprintln!(
        "coverage: mapped {} tests{skipped} → {nfunctions} functions in {nfiles} files; {:.1}s (build + tests {ran_secs:.1}s, mapping {:.1}s); {:.1} KiB → {}",
        map.tests.len(),
        started.elapsed().as_secs_f64(),
        mapping.elapsed().as_secs_f64(),
        size as f64 / 1024.0,
        path.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// `coverage info`.

pub fn info(root: &Path, cfg: &Coverage) -> Result<(), String> {
    let path = root.join(&cfg.map);
    if !path.is_file() {
        return Err(format!("no coverage map at {}; build one with `cargo jevtest coverage build`", path.display()));
    }
    let map = Map::load(&path)?;
    let size = std::fs::metadata(&path).map_or(0, |m| m.len());
    let age = age(root, &map.commit, "HEAD", cfg.max_age_commits);
    let days = parse_rfc3339(&map.built_at)
        .zip(SystemTime::now().duration_since(UNIX_EPOCH).ok())
        .map(|(built, now)| (now.as_secs() as f64 - built as f64) / 86400.0);
    let files: HashSet<&str> = map.functions.iter().map(|f| f.file.as_str()).collect();
    let packages: HashSet<&str> = map.tests.iter().map(|t| t.package.as_str()).collect();
    println!("map:        {}", path.display());
    println!("commit:     {} (built {}, {})", map.commit, map.built_at, map.rustc);
    println!(
        "age:        {}, {}",
        age.commits.map_or_else(|| "not in HEAD's history".to_owned(), |n| format!("{} behind HEAD", n_commits(n))),
        days.map_or_else(|| "built at an unknown time".to_owned(), |d| format!("{d:.1} days"))
    );
    println!("tests:      {} in {} packages", map.tests.len(), packages.len());
    println!("functions:  {} in {} files", map.functions.len(), files.len());
    println!("size:       {:.1} KiB", size as f64 / 1024.0);
    if let Some(why) = age.stale {
        eprintln!("jevtest: warning: stale map ({why}); policy gate/must runs as boost until you rebuild it (cargo jevtest coverage build)");
    }
    Ok(())
}

/// `YYYY-MM-DDTHH:MM:SSZ` for Unix seconds.
fn rfc3339(secs: u64) -> String {
    let (days, rem) = (secs / 86400, secs % 86400);
    // Howard Hinnant's civil_from_days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Unix seconds of a `YYYY-MM-DDTHH:MM:SSZ` timestamp.
fn parse_rfc3339(s: &str) -> Option<i64> {
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86400 + hh * 3600 + mm * 60 + ss)
}
