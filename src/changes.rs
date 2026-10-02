//! Which changes feed impact detection: the `[changes]` mode (or its flags) resolved to one diff,
//! `base` against a [`Target`], with a one-line account of what was chosen and why.

use std::path::Path;

use globset::GlobSet;
use serde_json::{Value, json};

use crate::config::{ChangeMode, Changes};
use crate::git::{self, FileChange, Target};

/// An explicit choice from the command line; `None` fields fall back to `[changes]`.
#[derive(Default)]
pub struct Request {
    pub mode: Option<ChangeMode>,
    pub base: Option<String>,
    pub head: Option<String>,
    /// `--branch BASE`: compare against this instead of the default branch.
    pub branch_base: Option<String>,
    pub last: Option<usize>,
    pub commit: Option<String>,
    pub since: Option<String>,
}

/// The resolved diff.
pub struct Scope {
    /// The mode asked for (a flag or `changes.mode`), e.g. `auto`.
    pub requested: &'static str,
    pub base: String,
    pub target: Target,
    /// The mode actually used.
    pub used: &'static str,
    /// What the diff covers, in words.
    pub what: String,
    /// Why this mode (auto's reasoning, or "requested").
    pub why: String,
    /// The committed scope auto narrowed away from, when the size guard fired.
    pub narrowed_from: Option<String>,
    pub files: usize,
    pub lines: usize,
    /// The diff itself (ignored paths included), so intake does not run it again.
    pub changes: Vec<FileChange>,
}

impl Scope {
    /// The stderr line naming the choice.
    pub fn line(&self) -> String {
        let s = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
        format!("jevtest: changes = {} ({}) · {}, {}", self.what, self.why, s(self.files, "file"), s(self.lines, "line"))
    }

    pub fn report(&self) -> Value {
        json!({
            "requested": self.requested,
            "used": self.used,
            "what": self.what,
            "reason": self.why,
            "base": self.base,
            "head": self.target.label(),
            "files": self.files,
            "lines": self.lines,
            "narrowed_from": self.narrowed_from,
        })
    }
}

/// Resolves the request against `[changes]`. `ignore` keeps ignored paths out of the dirty check
/// and the size guard.
pub fn resolve(root: &Path, cfg: &Changes, req: &Request, ignore: &GlobSet) -> Result<Scope, String> {
    let mode = req.mode.unwrap_or(cfg.mode);
    let mut scope = resolve_mode(root, cfg, req, ignore, mode)?;
    scope.requested = match mode {
        ChangeMode::Auto => "auto",
        ChangeMode::Uncommitted => "uncommitted",
        ChangeMode::Staged => "staged",
        ChangeMode::Unstaged => "unstaged",
        ChangeMode::Branch => "branch",
        ChangeMode::Last => "last",
        ChangeMode::Since => "since",
        ChangeMode::Range => "range",
    };
    Ok(scope)
}

fn resolve_mode(root: &Path, cfg: &Changes, req: &Request, ignore: &GlobSet, mode: ChangeMode) -> Result<Scope, String> {
    // Asked only by the modes that need it (one git run fewer for explicit ranges).
    let head = std::cell::OnceCell::new();
    let has_head = || *head.get_or_init(|| git::rev_exists(root, "HEAD"));
    let head_base = || if has_head() { "HEAD".to_owned() } else { git::EMPTY_TREE.to_owned() };
    let requested = "requested".to_owned();
    let scope = |base: String, target: Target, used, what: String, why: String| {
        size(root, &base, &target, cfg.include_untracked, ignore).map(|d| Scope {
            requested: "",
            base,
            target,
            used,
            what,
            why,
            narrowed_from: None,
            files: d.files,
            lines: d.lines,
            changes: d.changes,
        })
    };
    match mode {
        ChangeMode::Uncommitted => scope(head_base(), Target::Worktree, "uncommitted", "uncommitted work".into(), requested),
        ChangeMode::Staged => scope(head_base(), Target::Staged, "staged", "staged changes".into(), requested),
        ChangeMode::Unstaged => scope(String::new(), Target::Unstaged, "unstaged", "unstaged changes".into(), requested),
        ChangeMode::Branch => {
            let against = match &req.branch_base {
                Some(b) => b.clone(),
                None => git::default_branch(root, &cfg.default_branch)
                    .ok_or("--branch: no default branch found (origin/HEAD, origin/main, origin/master, main, master); pass --branch BASE")?,
            };
            let base = merge_base(root, &against)?;
            scope(base, Target::Worktree, "branch", format!("{} vs {against}, plus uncommitted work", branch_name(root)), requested)
        }
        ChangeMode::Last => {
            let n = req.last.unwrap_or(cfg.last).max(1);
            need_head(has_head())?;
            let base = nth_parent(root, "HEAD", n);
            scope(base, Target::Rev("HEAD".into()), "last", plural(n, "last commit", "last {n} commits"), requested)
        }
        ChangeMode::Since => {
            let when = req.since.clone().unwrap_or_else(|| cfg.since.clone());
            need_head(has_head())?;
            let what = format!("commits since {when}");
            match oldest_since(root, &when, "HEAD")? {
                Some(oldest) => scope(git::parent_or_empty(root, &oldest), Target::Rev("HEAD".into()), "since", what, requested),
                None => scope("HEAD".into(), Target::Rev("HEAD".into()), "since", format!("{what}: none"), requested),
            }
        }
        ChangeMode::Range => {
            let (base, target) = if let Some(rev) = &req.commit {
                (git::parent_or_empty(root, rev), Target::Rev(rev.clone()))
            } else {
                let base = req.base.clone().unwrap_or_else(|| cfg.base.clone());
                if base.trim().is_empty() {
                    return Err("range needs a base: --base A, --range A..B or changes.base".into());
                }
                (base, req.head.clone().map_or(Target::Worktree, Target::Rev))
            };
            let what = match (&req.commit, &target) {
                (Some(rev), _) => format!("commit {rev}"),
                (None, Target::Rev(h)) => format!("{base}..{h}"),
                (None, _) => format!("{base}..working tree"),
            };
            scope(base, target, "range", what, requested)
        }
        ChangeMode::Auto => auto(root, cfg, ignore, has_head()),
    }
}

fn auto(root: &Path, cfg: &Changes, ignore: &GlobSet, has_head: bool) -> Result<Scope, String> {
    let head_base = if has_head { "HEAD".to_owned() } else { git::EMPTY_TREE.to_owned() };
    let dirty = size(root, &head_base, &Target::Worktree, cfg.include_untracked, ignore)?;
    if dirty.files > 0 {
        let why = if dirty.files > cfg.max_files || dirty.lines > cfg.max_lines {
            "auto: working tree is dirty; large, but uncommitted work is never narrowed"
        } else {
            "auto: working tree is dirty"
        };
        return Ok(Scope {
            requested: "",
            base: head_base,
            target: Target::Worktree,
            used: "uncommitted",
            what: "uncommitted work".into(),
            why: why.into(),
            narrowed_from: None,
            files: dirty.files,
            lines: dirty.lines,
            changes: dirty.changes,
        });
    }
    need_head(has_head)?;
    let head = Target::Rev("HEAD".into());
    let last = || -> Result<Scope, String> {
        let base = git::parent_or_empty(root, "HEAD");
        let d = size(root, &base, &head, false, ignore)?;
        Ok(Scope {
            requested: "",
            base,
            target: Target::Rev("HEAD".into()),
            used: "last",
            what: "last commit".into(),
            why: String::new(),
            narrowed_from: None,
            files: d.files,
            lines: d.lines,
            changes: d.changes,
        })
    };

    let Some(default) = git::default_branch(root, &cfg.default_branch) else {
        let mut s = last()?;
        s.why = "auto: clean tree, no default branch found".into();
        return Ok(s);
    };
    let base = merge_base(root, &default)?;
    if base == git::rev_parse(root, "HEAD")? {
        let mut s = last()?;
        s.why = format!("auto: clean tree on {default}");
        return Ok(s);
    }

    let branch = branch_name(root);
    let d = size(root, &base, &head, false, ignore)?;
    let (files, lines) = (d.files, d.lines);
    if files <= cfg.max_files && lines <= cfg.max_lines {
        return Ok(Scope {
            requested: "",
            base,
            target: head,
            used: "branch",
            what: format!("{branch} vs {default}"),
            why: format!("auto: clean tree, not on {default}"),
            narrowed_from: None,
            files,
            lines,
            changes: d.changes,
        });
    }

    // Size guard: the branch diff is too large; narrow to recent commits, then the last one.
    let too_big = format!("branch diff {files} files / {lines} lines > max_files {} / max_lines {}", cfg.max_files, cfg.max_lines);
    let narrowed_from = Some(format!("{branch} vs {default} ({files} files, {lines} lines)"));
    if let Some(oldest) = oldest_since(root, &cfg.recent, &format!("{base}..HEAD"))? {
        let recent_base = git::parent_or_empty(root, &oldest);
        let d = size(root, &recent_base, &head, false, ignore)?;
        if d.files <= cfg.max_files && d.lines <= cfg.max_lines {
            return Ok(Scope {
                requested: "",
                base: recent_base,
                target: head,
                used: "since",
                what: format!("commits since {}", cfg.recent),
                why: format!("auto: {too_big}"),
                narrowed_from,
                files: d.files,
                lines: d.lines,
                changes: d.changes,
            });
        }
    }
    let mut s = last()?;
    s.why = format!("auto: {too_big}, and commits since {} are too large or none", cfg.recent);
    s.narrowed_from = narrowed_from;
    Ok(s)
}

fn need_head(has_head: bool) -> Result<(), String> {
    if has_head { Ok(()) } else { Err("the repository has no commits yet; use --uncommitted".into()) }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 { one.to_owned() } else { many.replace("{n}", &n.to_string()) }
}

fn branch_name(root: &Path) -> String {
    git::git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "detached HEAD".into())
}

fn merge_base(root: &Path, against: &str) -> Result<String, String> {
    git::git(root, &["merge-base", against, "HEAD"])
        .map(|s| s.trim().to_owned())
        .map_err(|e| format!("no merge-base between {against} and HEAD: {e}"))
}

/// `rev~n`, or the empty tree when history is shorter than `n` commits.
fn nth_parent(root: &Path, rev: &str, n: usize) -> String {
    let base = format!("{rev}~{n}");
    if git::rev_exists(root, &base) { base } else { git::EMPTY_TREE.to_owned() }
}

/// The oldest first-parent commit in `range` committed since `when`.
fn oldest_since(root: &Path, when: &str, range: &str) -> Result<Option<String>, String> {
    let list = git::git(root, &["rev-list", "--first-parent", &format!("--since={when}"), range])?;
    Ok(list.lines().last().map(str::to_owned))
}

struct Diff {
    /// Non-ignored changed files.
    files: usize,
    /// Changed lines of those files, both sides.
    lines: usize,
    changes: Vec<FileChange>,
}

/// The diff of `base` against `target`, sized without ignored paths.
fn size(root: &Path, base: &str, target: &Target, untracked: bool, ignore: &GlobSet) -> Result<Diff, String> {
    let changes = git::changed_files(root, base, target, untracked)?;
    let mut files = 0;
    let mut lines = 0;
    for f in changes.iter().filter(|f| !ignore.is_match(f.path())) {
        files += 1;
        for &(a, b) in f.new_ranges.iter().chain(&f.old_ranges) {
            lines += (b - a + 1) as usize;
        }
    }
    Ok(Diff { files, lines, changes })
}
