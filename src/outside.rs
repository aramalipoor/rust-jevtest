//! Which workspace packages read a changed file that lies outside every package: a string
//! literal in their Rust source (build.rs included) names its path, its last two components, a
//! distinctive basename or a parent directory (`include_str!("../../docs/openapi.json")`,
//! `sqlx::migrate!("../../migrations")`). Comments, doc comments included, do not count.
//!
//! Each file's path-like literals are cached per blob under the cache dir, so a run reads and
//! tokenizes only the files that changed since the last one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// One package reading an outside file.
pub struct Reference {
    /// Index into the outside files given to [`Finder::new`].
    pub file: usize,
    /// Where the literal is, `path:line`, and its text.
    pub at: String,
    pub literal: String,
}

/// A string literal that may name a path: its 1-based line and raw text.
pub type Literal = (u32, String);

/// Basenames too common to say which file a literal means.
const COMMON: &[&str] = &["mod.rs", "lib.rs", "main.rs", "README.md", "AGENTS.md", "Cargo.toml", "index.html"];

/// Whether a basename alone names its file: it has an extension, is at least 5 characters and is
/// not a [`COMMON`] name (so not `0`, `core`, `lib.rs`).
fn distinctive(base: &str) -> bool {
    base.len() >= 5 && base.rfind('.').is_some_and(|i| i > 0 && i + 1 < base.len()) && !COMMON.contains(&base)
}

pub struct Finder<'a> {
    /// Distinctive basename → files.
    basenames: HashMap<&'a str, Vec<usize>>,
    /// Two consecutive components → files a path-like word ending in them names: a file's own
    /// last two (`manifesto/evm.proto`), or a parent directory's.
    pairs: HashMap<(&'a str, &'a str), Vec<usize>>,
    /// Top-level directory → files directly in it.
    tops: HashMap<&'a str, Vec<usize>>,
    /// Submodules: their last two components (or their one) followed by more components name
    /// them too (`"../proto/manifesto/evm"` for the gitlink `proto/manifesto`).
    inner: HashMap<(&'a str, &'a str), Vec<usize>>,
    inner_one: HashMap<&'a str, Vec<usize>>,
    files: usize,
}

impl<'a> Finder<'a> {
    /// `gitlinks[i]`: `files[i]` is a submodule.
    pub fn new(files: &'a [String], gitlinks: &[bool]) -> Self {
        let mut f = Finder {
            basenames: HashMap::new(),
            pairs: HashMap::new(),
            tops: HashMap::new(),
            inner: HashMap::new(),
            inner_one: HashMap::new(),
            files: files.len(),
        };
        for (i, file) in files.iter().enumerate() {
            let c: Vec<&str> = file.split('/').filter(|c| !c.is_empty()).collect();
            let n = c.len();
            if n == 0 {
                continue;
            }
            if distinctive(c[n - 1]) {
                f.basenames.entry(c[n - 1]).or_default().push(i);
            }
            if n == 2 {
                f.tops.entry(c[0]).or_default().push(i);
            }
            for k in 2..=n {
                f.pairs.entry((c[k - 2], c[k - 1])).or_default().push(i);
            }
            if gitlinks.get(i).copied().unwrap_or(false) {
                match n {
                    1 => f.inner_one.entry(c[0]).or_default().push(i),
                    _ => f.inner.entry((c[n - 2], c[n - 1])).or_default().push(i),
                }
            }
        }
        f
    }

    /// Every outside file the literals of `path` name, once each, first literal wins.
    pub fn scan(&self, path: &str, literals: &[Literal]) -> Vec<Reference> {
        let mut out: Vec<Reference> = Vec::new();
        for (line, text) in literals {
            for i in self.named(text) {
                if !out.iter().any(|r| r.file == i) {
                    out.push(Reference { file: i, at: format!("{path}:{line}"), literal: text.clone() });
                }
            }
            if out.len() == self.files {
                break;
            }
        }
        out
    }

    /// Files literal `text` names: some path-like word in it ends with a file's distinctive
    /// basename, with its last two components or with two consecutive components of a parent
    /// directory, or runs through a submodule's path into it; or the whole literal is the
    /// top-level directory a file sits in (`"migrations"`, `"../../migrations"` for
    /// `migrations/0001.sql`; not the word in `"Write docs"`).
    fn named(&self, text: &str) -> Vec<usize> {
        let mut hits = Vec::new();
        fn components(s: &str) -> Vec<&str> {
            s.split('/').filter(|c| !c.is_empty() && *c != "." && *c != "..").collect()
        }
        if let [dir] = components(text.trim()).as_slice() {
            hits.extend(self.tops.get(dir).into_iter().flatten());
        }
        for word in text.split(|c: char| !path_char(c)) {
            let w = components(word);
            let Some(&last) = w.last() else { continue };
            hits.extend(self.basenames.get(last).into_iter().flatten());
            if w.len() >= 2 {
                hits.extend(self.pairs.get(&(w[w.len() - 2], last)).into_iter().flatten());
            }
            if !self.inner_one.is_empty() || !self.inner.is_empty() {
                for j in 0..w.len().saturating_sub(1) {
                    hits.extend(self.inner_one.get(w[j]).into_iter().flatten());
                    if j + 2 < w.len() {
                        hits.extend(self.inner.get(&(w[j], w[j + 1])).into_iter().flatten());
                    }
                }
            }
        }
        hits
    }
}

/// The per-blob literal cache: `<cache dir>/literals-v1/<key[..2]>/<key>`, one `line\ttext` per
/// literal.
pub struct Index {
    dir: PathBuf,
}

impl Index {
    pub fn new(cache_dir: &Path) -> Self {
        Index { dir: cache_dir.join(INDEX_DIR) }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(&key[..2.min(key.len())]).join(key)
    }

    pub fn get(&self, key: &str) -> Option<Vec<Literal>> {
        let text = std::fs::read_to_string(self.path(key)).ok()?;
        text.lines()
            .map(|l| {
                let (line, lit) = l.split_once('\t')?;
                Some((line.parse().ok()?, lit.to_owned()))
            })
            .collect()
    }

    /// Best effort: a cache that cannot be written only costs the next run a re-scan.
    pub fn put(&self, key: &str, literals: &[Literal]) {
        let path = self.path(key);
        let Some(dir) = path.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let body: String = literals.iter().map(|(line, lit)| format!("{line}\t{lit}\n")).collect();
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// The cache subdirectory holding the literal index (`cargo jevtest cache clear` removes it).
pub const INDEX_DIR: &str = "literals-v1";

/// Cache key of working-tree content (no blob id there): its sha256.
pub fn content_key(src: &str) -> String {
    Sha256::digest(src.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The literals of `src` that can name a path: one line, no tab, at most 512 bytes, and holding a
/// `/` or a `.` or being one path-like word (a top-level directory such as `"migrations"`).
pub fn literals(src: &str) -> Vec<Literal> {
    string_literals(src)
        .into_iter()
        .filter(|(_, raw)| {
            let t = raw.trim();
            !t.is_empty()
                && raw.len() <= 512
                && !raw.contains(['\n', '\r', '\t'])
                && (t.contains(['/', '.']) || t.chars().all(path_char))
        })
        .map(|(line, t)| (line as u32, t.to_owned()))
        .collect()
}

/// A character a path-like word holds.
fn path_char(c: char) -> bool {
    c.is_alphanumeric() || "._-/+@~".contains(c)
}

/// The string literals of Rust source `src` with their 1-based line: `"…"`, `b"…"`, `c"…"` and
/// raw `r#"…"#` forms, raw text (escapes left as written). Comments and char literals are skipped.
fn string_literals(src: &str) -> Vec<(usize, &str)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut line = 1;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\n' => line += 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 0;
                while i < b.len() {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        line += usize::from(b[i] == b'\n');
                        i += 1;
                    }
                }
                continue;
            }
            b'\'' => {
                // A char literal ('x', '\n', '\u{1F600}', '"'), or a lifetime ('a).
                if b.get(i + 1) == Some(&b'\\') {
                    i += 3;
                    while i < b.len() && b[i] != b'\'' {
                        i += 1;
                    }
                } else if let Some(c) = src[i + 1..].chars().next()
                    && src[i + 1 + c.len_utf8()..].starts_with('\'')
                {
                    i += 1 + c.len_utf8();
                }
            }
            b'r' if raw_prefix(b, i) && matches!(b.get(i + 1), Some(b'"' | b'#')) => {
                let hashes = b[i + 1..].iter().take_while(|&&c| c == b'#').count();
                if b.get(i + 1 + hashes) == Some(&b'"') {
                    let start = i + 2 + hashes;
                    let close = format!("\"{}", "#".repeat(hashes));
                    let end = src[start..].find(&close).map_or(b.len(), |e| start + e);
                    out.push((line, &src[start..end]));
                    line += src[start..end].matches('\n').count();
                    i = end + close.len();
                    continue;
                }
            }
            b'"' => {
                let start = i + 1;
                let start_line = line;
                i = start;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    line += usize::from(b.get(i) == Some(&b'\n'));
                    i += 1;
                }
                out.push((start_line, &src[start..i.min(b.len())]));
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Whether the `r` at `b[i]` can open a raw string: it does not continue an identifier, unless
/// it follows a lone `b`/`c` prefix (`br"…"`, `cr"…"`).
fn raw_prefix(b: &[u8], i: usize) -> bool {
    let ident = |j: usize| b[j].is_ascii_alphanumeric() || b[j] == b'_';
    match i {
        0 => true,
        _ if !ident(i - 1) => true,
        1 => matches!(b[0], b'b' | b'c'),
        _ => matches!(b[i - 1], b'b' | b'c') && !ident(i - 2),
    }
}
