//! Which workspace packages read a changed file that lies outside every package: a string
//! literal in their Rust source (build.rs included) names its path, its basename or a parent
//! directory (`include_str!("../../docs/openapi.json")`, `sqlx::migrate!("../../migrations")`).
//! Doc comments do not count.

use std::str::FromStr;

use proc_macro2::{Delimiter, TokenStream, TokenTree};

/// One package reading an outside file.
pub struct Reference {
    /// Index into the outside files given to [`Finder::new`].
    pub file: usize,
    /// Where the literal is, `path:line`, and its text.
    pub at: String,
    pub literal: String,
}

pub struct Finder<'a> {
    /// Each outside file split into path components.
    files: Vec<Vec<&'a str>>,
}

impl<'a> Finder<'a> {
    pub fn new(files: &'a [String]) -> Self {
        Finder { files: files.iter().map(|f| f.split('/').filter(|c| !c.is_empty()).collect()).collect() }
    }

    /// Every outside file `src` (at repo path `path`) names, once each.
    pub fn scan(&self, path: &str, src: &str) -> Vec<Reference> {
        let mut out: Vec<Reference> = Vec::new();
        let Ok(tokens) = TokenStream::from_str(src) else { return out };
        self.walk(tokens, path, &mut out);
        proc_macro2::extra::invalidate_current_thread_spans();
        out
    }

    fn walk(&self, tokens: TokenStream, path: &str, out: &mut Vec<Reference>) {
        let mut after_hash = false;
        for t in tokens {
            match t {
                TokenTree::Group(g) => {
                    let is_doc = after_hash
                        && g.delimiter() == Delimiter::Bracket
                        && matches!(g.stream().into_iter().next(), Some(TokenTree::Ident(i)) if i == "doc");
                    if !is_doc {
                        self.walk(g.stream(), path, out);
                    }
                    after_hash = false;
                }
                TokenTree::Punct(p) => after_hash = p.as_char() == '#' || (after_hash && p.as_char() == '!'),
                TokenTree::Literal(l) => {
                    after_hash = false;
                    let repr = l.to_string();
                    let (Some(a), Some(b)) = (repr.find('"'), repr.rfind('"')) else { continue };
                    if b <= a {
                        continue;
                    }
                    let text = &repr[a + 1..b];
                    for (i, f) in self.files.iter().enumerate() {
                        if out.iter().any(|r| r.file == i) {
                            continue;
                        }
                        if names(text, f) {
                            out.push(Reference { file: i, at: format!("{path}:{}", l.span().start().line), literal: text.to_owned() });
                        }
                    }
                }
                TokenTree::Ident(_) => after_hash = false,
            }
        }
    }
}

/// Whether literal `text` names file `f`: some path-like word in it ends with the file's path or
/// its basename, ends with two trailing components of a parent directory, or is exactly the
/// file's top-level parent directory (`"migrations"` for `migrations/0001_init.sql`).
fn names(text: &str, f: &[&str]) -> bool {
    let n = f.len();
    if n == 0 {
        return false;
    }
    let path_char = |c: char| c.is_alphanumeric() || "._-/+@~".contains(c);
    text.split(|c: char| !path_char(c)).any(|word| {
        let w: Vec<&str> = word.split('/').filter(|c| !c.is_empty() && *c != "." && *c != "..").collect();
        let Some(&last) = w.last() else { return false };
        last == f[n - 1] || (n == 2 && w == [f[0]]) || (2..n).any(|k| w.ends_with(&f[k - 2..k]))
    })
}
