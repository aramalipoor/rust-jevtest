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

/// `git merge-base HEAD origin/HEAD`, falling back to origin/main, then origin/master.
pub fn default_base(root: &Path) -> Result<String, String> {
    for upstream in ["origin/HEAD", "origin/main", "origin/master"] {
        if let Ok(base) = git(root, &["merge-base", "HEAD", upstream]) {
            return Ok(base.trim().to_owned());
        }
    }
    Err("no --base given and no merge-base with origin/HEAD, origin/main or origin/master".into())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Added => "added",
            Status::Modified => "modified",
            Status::Deleted => "deleted",
            Status::Renamed => "renamed",
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

fn diff_args<'a>(extra: &[&'a str], base: &'a str, head: Option<&'a str>) -> Vec<&'a str> {
    let mut args = vec!["-c", "core.quotePath=false", "diff"];
    args.extend_from_slice(&DIFF_FLAGS);
    args.extend_from_slice(extra);
    args.push(base);
    args.extend(head);
    args.push("--");
    args
}

/// The `-U5` diff text handed to Jev.
pub fn context_diff(root: &Path, base: &str, head: Option<&str>) -> Result<String, String> {
    git(root, &diff_args(&["-U5"], base, head))
}

pub fn changed_files(root: &Path, base: &str, head: Option<&str>) -> Result<Vec<FileChange>, String> {
    let names = git(root, &diff_args(&["--name-status", "-z"], base, head))?;
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

    let hunks = parse_hunks(&git(root, &diff_args(&["-U0"], base, head))?);
    for f in &mut files {
        if let Some((old, new)) = hunks.get(f.path()) {
            f.old_ranges.clone_from(old);
            f.new_ranges.clone_from(new);
        }
    }
    Ok(files)
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
    pub fn new(root: &Path, head: Option<&str>) -> Result<Self, String> {
        let Some(rev) = head else { return Ok(Source::Disk(root.to_owned())) };
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
                let mut args = vec!["ls-tree", "-r", "--name-only", "-z", rev.as_str()];
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
