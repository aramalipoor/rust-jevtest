//! Workspace packages from `cargo metadata` and the reverse-dependency graph.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

pub struct Package {
    pub name: String,
    /// Repo-relative manifest dir; `""` for a package at the repo root.
    pub dir: String,
}

pub struct Workspace {
    pub packages: Vec<Package>,
    /// Package index → indices of packages that directly depend on it.
    rdeps: Vec<Vec<usize>>,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetaPackage>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct MetaPackage {
    id: String,
    name: String,
    manifest_path: String,
    dependencies: Vec<MetaDep>,
}

#[derive(Deserialize)]
struct MetaDep {
    name: String,
    path: Option<String>,
}

impl Workspace {
    pub fn load(root: &Path) -> Result<Self, String> {
        let out = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["metadata", "--format-version", "1", "--no-deps"])
            .current_dir(root)
            .stderr(std::process::Stdio::inherit())
            .output()
            .map_err(|e| format!("cannot run cargo metadata: {e}"))?;
        if !out.status.success() {
            return Err("cargo metadata failed".into());
        }
        let meta: Metadata =
            serde_json::from_slice(&out.stdout).map_err(|e| format!("cannot parse cargo metadata: {e}"))?;
        let members: BTreeSet<&str> = meta.workspace_members.iter().map(String::as_str).collect();
        let metas: Vec<&MetaPackage> = meta.packages.iter().filter(|p| members.contains(p.id.as_str())).collect();

        let mut packages = Vec::with_capacity(metas.len());
        for p in &metas {
            let manifest = Path::new(&p.manifest_path);
            let dir = manifest.parent().unwrap_or(manifest);
            let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
            let rel = dir.strip_prefix(root).map_err(|_| {
                format!("package {} at {} is outside the repo {}", p.name, dir.display(), root.display())
            })?;
            let rel = rel.to_str().ok_or_else(|| format!("non-UTF-8 path for package {}", p.name))?;
            packages.push(Package { name: p.name.clone(), dir: rel.to_owned() });
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
        Ok(Workspace { packages, rdeps })
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
