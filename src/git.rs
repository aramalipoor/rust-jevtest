//! Git plumbing: running git, parsing the `-U0` diff, reading files at a revision.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(match String::from_utf8(out.stdout) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    })
}

pub fn repo_root(dir: &Path) -> Result<PathBuf, String> {
    let top = git(dir, &["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(top.trim_end());
    top.canonicalize()
        .map_err(|e| format!("cannot resolve repo root {}: {e}", top.display()))
}

/// The tree object of an empty repository: the base when a range starts at the root commit.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Whether `rev` resolves to a commit.
pub fn rev_exists(root: &Path, rev: &str) -> bool {
    git(root, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")]).is_ok()
}

/// Full hash of `rev`.
pub fn rev_parse(root: &Path, rev: &str) -> Result<String, String> {
    Ok(git(root, &["rev-parse", "--verify", &format!("{rev}^{{commit}}")])?.trim().to_owned())
}

/// `rev^`, or the empty tree when `rev` is a root commit.
pub fn parent_or_empty(root: &Path, rev: &str) -> String {
    let parent = format!("{rev}^");
    if rev_exists(root, &parent) { parent } else { EMPTY_TREE.to_owned() }
}

/// The repository's default branch ref: `name` itself unless it is `"auto"`, which tries
/// origin/HEAD, origin/main, origin/master, main, master. `None` when none exists.
pub fn default_branch(root: &Path, name: &str) -> Option<String> {
    if name != "auto" {
        return rev_exists(root, name).then(|| name.to_owned());
    }
    ["origin/HEAD", "origin/main", "origin/master", "main", "master"]
        .into_iter()
        .find(|r| rev_exists(root, r))
        .map(str::to_owned)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Added,
    Modified,
    Deleted,
    Renamed,
    /// Not known to git; counts as added (working-tree mode, `include_untracked`).
    Untracked,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Added => "added",
            Status::Modified => "modified",
            Status::Deleted => "deleted",
            Status::Renamed => "renamed",
            Status::Untracked => "untracked",
        }
    }
}

/// What the base is compared against.
#[derive(Clone, Debug)]
pub enum Target {
    /// The working tree (tracked changes, plus untracked `.rs` files when enabled).
    Worktree,
    /// The index (`--staged`).
    Staged,
    /// The working tree against the index (`--unstaged`); the base is `""`, the index.
    Unstaged,
    /// A revision (`--head`).
    Rev(String),
}

impl Target {
    pub fn label(&self) -> &str {
        match self {
            Target::Worktree | Target::Unstaged => "working tree",
            Target::Staged => "index",
            Target::Rev(r) => r,
        }
    }
}

/// Inclusive 1-based line range.
pub type Range = (u32, u32);

#[derive(Debug)]
pub struct FileChange {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: Status,
    pub new_ranges: Vec<Range>,
    pub old_ranges: Vec<Range>,
}

impl FileChange {
    /// The path that exists after the change (old path for deletions).
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or_default()
    }
}

/// Common diff flags that make output independent of user git config.
const DIFF_FLAGS: [&str; 6] = [
    "--no-color",
    "--no-ext-diff",
    "--find-renames",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--no-relative",
];

fn diff_args<'a>(extra: &[&'a str], base: &'a str, target: &'a Target) -> Vec<&'a str> {
    let mut args = vec!["-c", "core.quotePath=false", "diff"];
    args.extend_from_slice(&DIFF_FLAGS);
    args.extend_from_slice(extra);
    match target {
        Target::Worktree => args.push(base),
        Target::Staged => args.extend(["--cached", base]),
        Target::Unstaged => {}
        Target::Rev(head) => args.extend([base, head.as_str()]),
    }
    args.push("--");
    args
}

/// The `-U5` diff text handed to Jev; untracked files are appended as whole-file additions.
pub fn context_diff(root: &Path, base: &str, target: &Target, files: &[FileChange], source: &mut Source) -> Result<String, String> {
    let mut diff = git(root, &diff_args(&["-U5"], base, target))?;
    for f in files.iter().filter(|f| f.status == Status::Untracked) {
        let path = f.path();
        let Some(text) = source.read(path) else { continue };
        let n = text.lines().count();
        diff.push_str(&format!("diff --git a/{path} b/{path}\nnew file (untracked)\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{n} @@\n"));
        for line in text.lines() {
            diff.push('+');
            diff.push_str(line);
            diff.push('\n');
        }
    }
    Ok(diff)
}

/// Changed files between `base` and `target`; with `untracked`, untracked `.rs` files of the
/// working tree are added as [`Status::Untracked`] covering every line.
pub fn changed_files(root: &Path, base: &str, target: &Target, untracked: bool) -> Result<Vec<FileChange>, String> {
    let names = git(root, &diff_args(&["--name-status", "-z"], base, target))?;
    let mut files = Vec::new();
    let mut fields = names.split('\0').filter(|s| !s.is_empty());
    while let Some(code) = fields.next() {
        let one = |f: &mut dyn Iterator<Item = &str>| {
            f.next()
                .map(str::to_owned)
                .ok_or_else(|| format!("truncated name-status after {code}"))
        };
        let (status, old_path, new_path) = match code.as_bytes()[0] {
            b'A' => (Status::Added, None, Some(one(&mut fields)?)),
            b'D' => (Status::Deleted, Some(one(&mut fields)?), None),
            b'R' => {
                let old = one(&mut fields)?;
                (Status::Renamed, Some(old), Some(one(&mut fields)?))
            }
            b'C' => {
                one(&mut fields)?;
                (Status::Added, None, Some(one(&mut fields)?))
            }
            _ => {
                let p = one(&mut fields)?;
                (Status::Modified, Some(p.clone()), Some(p))
            }
        };
        files.push(FileChange { old_path, new_path, status, new_ranges: Vec::new(), old_ranges: Vec::new() });
    }

    let hunks = parse_hunks(&git(root, &diff_args(&["-U0"], base, target))?);
    for f in &mut files {
        if let Some((old, new)) = hunks.get(f.path()) {
            f.old_ranges.clone_from(old);
            f.new_ranges.clone_from(new);
        }
    }

    if untracked && matches!(target, Target::Worktree | Target::Unstaged) {
        let list = git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
        for path in list.split('\0').filter(|p| p.ends_with(".rs")) {
            let lines = std::fs::read_to_string(root.join(path)).map(|t| t.lines().count()).unwrap_or(0).max(1) as u32;
            files.push(FileChange {
                old_path: None,
                new_path: Some(path.to_owned()),
                status: Status::Untracked,
                new_ranges: vec![(1, lines)],
                old_ranges: Vec::new(),
            });
        }
    }
    Ok(files)
}

/// Keeps the changes under `patterns` (paths or globs). A listed path that exists on the target
/// side but has no change in scope is added as wholly changed, so `--files src/x.rs` means "the
/// impact of this file".
pub fn restrict(root: &Path, files: Vec<FileChange>, patterns: &[String], target: &Target) -> Result<Vec<FileChange>, String> {
    let mut b = globset::GlobSetBuilder::new();
    for p in patterns {
        let p = p.trim_start_matches("./").trim_end_matches('/');
        b.add(globset::Glob::new(p).map_err(|e| format!("--files: bad glob {p}: {e}"))?);
        b.add(globset::Glob::new(&format!("{p}/**")).map_err(|e| format!("--files: bad glob {p}: {e}"))?);
    }
    let set = b.build().map_err(|e| e.to_string())?;
    let hit = |f: &FileChange| f.new_path.iter().chain(&f.old_path).any(|p| set.is_match(p));
    let mut kept: Vec<FileChange> = files.into_iter().filter(hit).collect();

    let mut source = Source::new(root, target)?;
    let all = source.list(root, "")?;
    for path in all.iter().filter(|p| set.is_match(p.as_str())) {
        if kept.iter().any(|f| f.new_path.as_deref() == Some(path.as_str())) {
            continue;
        }
        let Some(text) = source.read(path) else { continue };
        let lines = text.lines().count().max(1) as u32;
        kept.push(FileChange {
            old_path: Some(path.clone()),
            new_path: Some(path.clone()),
            status: Status::Modified,
            new_ranges: vec![(1, lines)],
            old_ranges: vec![(1, lines)],
        });
    }
    Ok(kept)
}

/// Parses a `-U0` diff into path → (old ranges, new ranges); keyed by new path, or old path for deletions.
fn parse_hunks(diff: &str) -> HashMap<String, (Vec<Range>, Vec<Range>)> {
    let mut map: HashMap<String, (Vec<Range>, Vec<Range>)> = HashMap::new();
    let mut in_header = false;
    let mut old: Option<String> = None;
    let mut new: Option<String> = None;
    let mut current: Option<String> = None;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_header = true;
            old = None;
            new = None;
            current = None;
        } else if in_header && let Some(p) = line.strip_prefix("--- ") {
            old = strip_side(p, "a/");
        } else if in_header && let Some(p) = line.strip_prefix("+++ ") {
            new = strip_side(p, "b/");
        } else if line.starts_with("@@ ") {
            if in_header {
                in_header = false;
                current = new.take().or_else(|| old.take());
            }
            let (Some(path), Some((o, n))) = (&current, parse_hunk_header(line)) else { continue };
            let entry = map.entry(path.clone()).or_default();
            entry.0.extend(o);
            entry.1.push(n);
        }
    }
    map
}

fn strip_side(p: &str, prefix: &str) -> Option<String> {
    let p = p.trim_end_matches('\t');
    if p == "/dev/null" {
        return None;
    }
    let p = if p.starts_with('"') { unquote(p) } else { p.to_owned() };
    Some(p.strip_prefix(prefix).map(str::to_owned).unwrap_or(p))
}

/// Undoes git's C-style path quoting.
fn unquote(s: &str) -> String {
    let inner = s.trim_matches('"').as_bytes();
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        if inner[i] == b'\\' && i + 1 < inner.len() {
            i += 1;
            match inner[i] {
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'0'..=b'3' if i + 2 < inner.len() => {
                    let oct = std::str::from_utf8(&inner[i..i + 3]).ok().and_then(|o| u8::from_str_radix(o, 8).ok());
                    out.push(oct.unwrap_or(inner[i]));
                    i += 2;
                }
                c => out.push(c),
            }
        } else {
            out.push(inner[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `@@ -a[,b] +c[,d] @@` → (old range if b > 0, new range). A pure deletion (`d == 0`) marks line c.
fn parse_hunk_header(line: &str) -> Option<(Option<Range>, Range)> {
    let mut parts = line.split(' ');
    parts.next();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let span = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (a, b) = span(old)?;
    let (c, d) = span(new)?;
    let old_range = (b > 0).then(|| (a, a + b - 1));
    let new_range = if d == 0 { (c.max(1), c.max(1)) } else { (c, c + d - 1) };
    Some((old_range, new_range))
}

/// Reads files either from the working tree or at a revision (via one `git cat-file --batch`).
pub enum Source {
    Disk(PathBuf),
    Rev { rev: String, child: Child, stdin: ChildStdin, stdout: BufReader<ChildStdout> },
}

impl Source {
    /// Reads the side `target` names: disk, the index, or a revision.
    pub fn new(root: &Path, target: &Target) -> Result<Self, String> {
        match target {
            Target::Worktree | Target::Unstaged => Ok(Source::Disk(root.to_owned())),
            // `:path` names the staged blob.
            Target::Staged => Self::at(root, ""),
            Target::Rev(rev) => Self::at(root, rev),
        }
    }

    /// Reads files at revision `rev` (`""` = the index).
    pub fn at(root: &Path, rev: &str) -> Result<Self, String> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run git cat-file: {e}"))?;
        let stdin = child.stdin.take().ok_or("git cat-file: no stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("git cat-file: no stdout")?);
        Ok(Source::Rev { rev: rev.to_owned(), child, stdin, stdout })
    }

    /// Repo-relative path → contents (None when absent or not UTF-8).
    pub fn read(&mut self, path: &str) -> Option<String> {
        match self {
            Source::Disk(root) => std::fs::read_to_string(root.join(path)).ok(),
            Source::Rev { rev, stdin, stdout, .. } => {
                writeln!(stdin, "{rev}:{path}").ok()?;
                stdin.flush().ok()?;
                let mut header = String::new();
                stdout.read_line(&mut header).ok()?;
                let mut fields = header.split_ascii_whitespace();
                let (_, kind, size) = (fields.next()?, fields.next()?, fields.next());
                let size: usize = size?.parse().ok()?;
                let mut buf = vec![0; size + 1];
                stdout.read_exact(&mut buf).ok()?;
                buf.pop();
                if kind != "blob" {
                    return None;
                }
                String::from_utf8(buf).ok()
            }
        }
    }

    /// Every file under `dir` (repo-relative, `""` = root), repo-relative paths.
    pub fn list(&self, root: &Path, dir: &str) -> Result<Vec<String>, String> {
        match self {
            Source::Disk(_) => {
                let mut out = Vec::new();
                walk_disk(root, dir, &mut out);
                out.sort_unstable();
                Ok(out)
            }
            Source::Rev { rev, .. } => {
                let mut args = if rev.is_empty() {
                    vec!["ls-files", "-z"]
                } else {
                    vec!["ls-tree", "-r", "--name-only", "-z", rev.as_str()]
                };
                if !dir.is_empty() {
                    args.extend(["--", dir]);
                }
                Ok(git(root, &args)?.split('\0').filter(|s| !s.is_empty()).map(str::to_owned).collect())
            }
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        if let Source::Rev { child, .. } = self {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn walk_disk(root: &Path, dir: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else { return };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else { continue };
        let rel = if dir.is_empty() { name } else { format!("{dir}/{name}") };
        match entry.file_type() {
            Ok(t) if t.is_dir() => {
                if !(rel == "target" || rel == ".git" || rel.ends_with("/target") || rel.ends_with("/.git")) {
                    walk_disk(root, &rel, out);
                }
            }
            Ok(t) if t.is_file() => out.push(rel),
            _ => {}
        }
    }
}
