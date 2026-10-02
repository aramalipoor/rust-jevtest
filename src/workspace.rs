//! Workspace packages from `cargo metadata` and the reverse-dependency graph.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

pub struct Package {
    pub name: String,
    /// Repo-relative manifest dir; `""` for a package at the repo root.
    pub dir: String,
    /// A proc-macro crate: its code runs inside rustc, where no test coverage sees it.
    pub proc_macro: bool,
    /// Names of its binary targets.
    pub bins: Vec<String>,
}

pub struct Workspace {
    pub packages: Vec<Package>,
    /// Cargo's target directory.
    pub target_dir: PathBuf,
    /// Package index → indices of packages that directly depend on it.
    rdeps: Vec<Vec<usize>>,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetaPackage>,
    workspace_members: Vec<String>,
    target_directory: PathBuf,
    #[serde(default)]
    resolve: Option<Resolve>,
}

#[derive(Deserialize)]
struct MetaPackage {
    id: String,
    name: String,
    manifest_path: String,
    #[serde(default)]
    source: Option<String>,
    dependencies: Vec<MetaDep>,
    targets: Vec<MetaTarget>,
}

#[derive(Deserialize)]
struct MetaTarget {
    name: String,
    kind: Vec<String>,
}

#[derive(Deserialize)]
struct MetaDep {
    name: String,
    path: Option<String>,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    dependencies: Vec<String>,
}

/// A path dependency that is not a workspace member (`[patch]` paths, excluded crates).
pub struct PathDep {
    pub name: String,
    /// Repo-relative manifest dir.
    pub dir: String,
    /// Members that depend on it, directly or through other packages.
    pub dependents: Vec<usize>,
}

fn metadata(root: &Path, deps: bool) -> Result<Metadata, String> {
    let mut args = vec!["metadata", "--format-version", "1"];
    if !deps {
        args.push("--no-deps");
    }
    let out = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(&args)
        .current_dir(root)
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot run cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err("cargo metadata failed".into());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("cannot parse cargo metadata: {e}"))
}

/// `dir` relative to `root`, canonicalized; `None` outside the repo or not UTF-8.
fn relative(root: &Path, manifest: &str) -> Option<String> {
    let manifest = Path::new(manifest);
    let dir = manifest.parent().unwrap_or(manifest);
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
    dir.strip_prefix(root).ok()?.to_str().map(str::to_owned)
}

impl Workspace {
    pub fn load(root: &Path) -> Result<Self, String> {
        let meta = metadata(root, false)?;
        let members: BTreeSet<&str> = meta.workspace_members.iter().map(String::as_str).collect();
        let metas: Vec<&MetaPackage> = meta.packages.iter().filter(|p| members.contains(p.id.as_str())).collect();

        let mut packages = Vec::with_capacity(metas.len());
        for p in &metas {
            let rel = relative(root, &p.manifest_path)
                .ok_or_else(|| format!("package {} at {} is outside the repo {}", p.name, p.manifest_path, root.display()))?;
            packages.push(Package {
                name: p.name.clone(),
                dir: rel,
                proc_macro: p.targets.iter().any(|t| t.kind.iter().any(|k| k == "proc-macro")),
                bins: p.targets.iter().filter(|t| t.kind.iter().any(|k| k == "bin")).map(|t| t.name.clone()).collect(),
            });
        }
        let index: HashMap<&str, usize> = metas.iter().enumerate().map(|(i, p)| (p.name.as_str(), i)).collect();
        let mut rdeps = vec![Vec::new(); packages.len()];
        for (i, p) in metas.iter().enumerate() {
            let mut deps: Vec<usize> = p
                .dependencies
                .iter()
                .filter(|d| d.path.is_some())
                .filter_map(|d| index.get(d.name.as_str()).copied())
                .filter(|&j| j != i)
                .collect();
            deps.sort_unstable();
            deps.dedup();
            for &j in &deps {
                rdeps[j].push(i);
            }
        }
        Ok(Workspace { packages, target_dir: meta.target_directory, rdeps })
    }

    /// The package whose manifest dir is the longest prefix of `file` (repo-relative).
    pub fn owner(&self, file: &str) -> Option<usize> {
        self.packages
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.dir.is_empty()
                    || file.strip_prefix(p.dir.as_str()).is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|(_, p)| p.dir.len())
            .map(|(i, _)| i)
    }

    /// Path dependencies inside the repo that are not members, with the members depending on
    /// each. Runs `cargo metadata` with dependencies (slower than [`Workspace::load`]), so call
    /// it only when a changed file lies outside every member.
    pub fn path_deps(&self, root: &Path) -> Result<Vec<PathDep>, String> {
        let meta = metadata(root, true)?;
        let members: HashSet<&str> = meta.workspace_members.iter().map(String::as_str).collect();
        let names: HashMap<&str, &str> = meta.packages.iter().map(|p| (p.id.as_str(), p.name.as_str())).collect();
        let mut rev: HashMap<&str, Vec<&str>> = HashMap::new();
        for n in meta.resolve.iter().flat_map(|r| &r.nodes) {
            for d in &n.dependencies {
                rev.entry(d.as_str()).or_default().push(n.id.as_str());
            }
        }
        let mut out = Vec::new();
        for p in meta.packages.iter().filter(|p| p.source.is_none() && !members.contains(p.id.as_str())) {
            let Some(dir) = relative(root, &p.manifest_path) else { continue };
            let mut seen: HashSet<&str> = HashSet::from([p.id.as_str()]);
            let mut queue: VecDeque<&str> = VecDeque::from([p.id.as_str()]);
            let mut dependents = Vec::new();
            while let Some(at) = queue.pop_front() {
                for &up in rev.get(at).into_iter().flatten() {
                    if seen.insert(up) {
                        if members.contains(up)
                            && let Some(i) = names.get(up).and_then(|n| self.by_name(n))
                        {
                            dependents.push(i);
                        }
                        queue.push_back(up);
                    }
                }
            }
            dependents.sort_unstable_by(|&a, &b| self.packages[a].name.cmp(&self.packages[b].name));
            out.push(PathDep { name: p.name.clone(), dir, dependents });
        }
        Ok(out)
    }

    pub fn by_name(&self, name: &str) -> Option<usize> {
        self.packages.iter().position(|p| p.name == name)
    }

    /// Changed packages (in the given order) followed by their reverse deps (normal, dev and build),
    /// nearest first, at most `max_depth` hops away (0 = no limit).
    /// Returns (package, distance) with distance 0 for changed packages.
    pub fn affected(&self, changed: &[usize], max_depth: usize) -> Vec<(usize, u32)> {
        let mut dist: Vec<Option<u32>> = vec![None; self.packages.len()];
        let mut queue = VecDeque::new();
        let mut order = Vec::new();
        for &c in changed {
            if dist[c].is_none() {
                dist[c] = Some(0);
                queue.push_back(c);
                order.push((c, 0));
            }
        }
        while let Some(p) = queue.pop_front() {
            let d = dist[p].unwrap_or(0) + 1;
            if max_depth != 0 && d as usize > max_depth {
                continue;
            }
            let mut next: Vec<usize> = self.rdeps[p].iter().copied().filter(|&r| dist[r].is_none()).collect();
            next.sort_unstable_by(|&a, &b| self.packages[a].name.cmp(&self.packages[b].name));
            for r in next {
                dist[r] = Some(d);
                queue.push_back(r);
                order.push((r, d));
            }
        }
        // BFS already yields nondecreasing distance; within a level keep name order.
        order.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| self.packages[a.0].name.cmp(&self.packages[b.0].name)));
        order
    }
}
