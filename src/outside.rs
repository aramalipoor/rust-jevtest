//! Which workspace packages read a changed file that lies outside every package: a string
//! literal in their Rust source (build.rs included) names its path, its basename or a parent
//! directory (`include_str!("../../docs/openapi.json")`, `sqlx::migrate!("../../migrations")`).
//! Comments, doc comments included, do not count.

use std::collections::HashMap;

/// One package reading an outside file.
pub struct Reference {
    /// Index into the outside files given to [`Finder::new`].
    pub file: usize,
    /// Where the literal is, `path:line`, and its text.
    pub at: String,
    pub literal: String,
}

pub struct Finder<'a> {
    /// Basename → files.
    basenames: HashMap<&'a str, Vec<usize>>,
    /// Two consecutive components of a parent directory → files under it.
    pairs: HashMap<(&'a str, &'a str), Vec<usize>>,
    /// Top-level directory → files directly in it.
    tops: HashMap<&'a str, Vec<usize>>,
    /// Every component a naming literal must contain; a source holding none is skipped unscanned.
    needles: Vec<&'a str>,
    files: usize,
}

impl<'a> Finder<'a> {
    pub fn new(files: &'a [String]) -> Self {
        let mut f = Finder { basenames: HashMap::new(), pairs: HashMap::new(), tops: HashMap::new(), needles: Vec::new(), files: files.len() };
        for (i, file) in files.iter().enumerate() {
            let c: Vec<&str> = file.split('/').filter(|c| !c.is_empty()).collect();
            let n = c.len();
            if n == 0 {
                continue;
            }
            f.basenames.entry(c[n - 1]).or_default().push(i);
            f.needles.push(c[n - 1]);
            if n == 2 {
                f.tops.entry(c[0]).or_default().push(i);
                f.needles.push(c[0]);
            }
            for k in 2..n {
                f.pairs.entry((c[k - 2], c[k - 1])).or_default().push(i);
                f.needles.push(c[k - 1]);
            }
        }
        f.needles.sort_unstable();
        f.needles.dedup();
        f
    }

    /// Every outside file `src` (at repo path `path`) names, once each, first literal wins.
    pub fn scan(&self, path: &str, src: &str) -> Vec<Reference> {
        let mut out: Vec<Reference> = Vec::new();
        if !self.needles.iter().any(|n| src.contains(n)) {
            return out;
        }
        for (line, text) in string_literals(src) {
            for i in self.named(text) {
                if !out.iter().any(|r| r.file == i) {
                    out.push(Reference { file: i, at: format!("{path}:{line}"), literal: text.to_owned() });
                }
            }
            if out.len() == self.files {
                break;
            }
        }
        out
    }

    /// Files literal `text` names: some path-like word in it ends with a file's basename (so also
    /// with its whole path) or with two consecutive components of a parent directory, or the
    /// whole literal is the top-level directory a file sits in (`"migrations"`,
    /// `"../../migrations"` for `migrations/0001.sql`; not the word in `"Write docs"`).
    fn named(&self, text: &str) -> Vec<usize> {
        let mut hits = Vec::new();
        fn components(s: &str) -> Vec<&str> {
            s.split('/').filter(|c| !c.is_empty() && *c != "." && *c != "..").collect()
        }
        if let [dir] = components(text.trim()).as_slice() {
            hits.extend(self.tops.get(dir).into_iter().flatten());
        }
        let path_char = |c: char| c.is_alphanumeric() || "._-/+@~".contains(c);
        for word in text.split(|c: char| !path_char(c)) {
            let w = components(word);
            let Some(&last) = w.last() else { continue };
            hits.extend(self.basenames.get(last).into_iter().flatten());
            if w.len() >= 2 {
                hits.extend(self.pairs.get(&(w[w.len() - 2], last)).into_iter().flatten());
            }
        }
        hits
    }
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
