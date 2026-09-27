//! `.hubignore` (Issue #24): a small, dependency-free gitignore subset.
//!
//! One file, `.hubignore` at the working-directory root, one pattern per
//! line. Supported:
//! - blank lines and lines starting with `#` are skipped; trailing spaces,
//!   tabs and `\r` are trimmed; a leading `\` escapes a literal `#` or `!`;
//! - `!pat` negates (re-includes); the **last** matching line wins;
//! - a trailing `/` matches directories only;
//! - a pattern with a `/` at the start or in the middle is anchored to the
//!   root (a leading `/` is dropped); otherwise it matches the name at any
//!   depth (as if prefixed by `**/`);
//! - `*` matches any run of characters except `/`, `?` one character
//!   except `/`; `**` as a whole segment matches zero or more segments
//!   (`**/x`, `a/**/b`), and a trailing `/**` matches everything inside
//!   (but not the directory itself); `**` inside a segment acts as `*`.
//!
//! As in git, a path whose parent directory is ignored is ignored, and a
//! negation cannot re-include a path inside an ignored directory.
//!
//! Not supported (treated literally): character classes `[...]`, backslash
//! escapes other than a leading `\#` / `\!`, escaped trailing spaces.
//! Not supported at all: nested `.hubignore` files, `.git/info/exclude`
//! style per-repo or global excludes, case-insensitive matching.
//!
//! The top-level `.hub/` directory is always excluded by the scanner and
//! never reaches this matcher.

use std::fs;
use std::io;
use std::path::Path;

pub const FILE: &str = ".hubignore";

#[derive(Clone, Debug)]
struct Rule {
    negate: bool,
    dir_only: bool,
    /// Pattern segments; unanchored patterns start with `**`.
    segs: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Ignore {
    rules: Vec<Rule>,
}

impl Ignore {
    /// Load `<root>/.hubignore`; absent (or not a regular file) = no rules.
    pub fn load(root: &Path) -> io::Result<Ignore> {
        let p = root.join(FILE);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.is_file() => {}
            Ok(_) => return Ok(Ignore::default()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Ignore::default()),
            Err(e) => return Err(e),
        }
        let bytes = fs::read(&p)?;
        Ok(Ignore::parse(&String::from_utf8_lossy(&bytes)))
    }

    pub fn parse(text: &str) -> Ignore {
        let mut rules = Vec::new();
        for line in text.lines() {
            let mut l = line.trim_end_matches([' ', '\t', '\r']);
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let mut negate = false;
            if let Some(r) = l.strip_prefix('!') {
                negate = true;
                l = r;
            } else if let Some(r) = l.strip_prefix('\\') {
                if r.starts_with('#') || r.starts_with('!') {
                    l = r;
                }
            }
            let dir_only = l.ends_with('/');
            let l = l.trim_end_matches('/');
            if l.is_empty() {
                continue;
            }
            let anchored = l.contains('/');
            let l = l.strip_prefix('/').unwrap_or(l);
            let mut segs: Vec<String> = Vec::new();
            if !anchored {
                segs.push("**".into());
            }
            segs.extend(l.split('/').filter(|s| !s.is_empty()).map(str::to_string));
            // Collapse runs of `**`.
            segs.dedup_by(|a, b| a == "**" && b == "**");
            if segs.is_empty() {
                continue;
            }
            rules.push(Rule {
                negate,
                dir_only,
                segs,
            });
        }
        Ignore { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether `path` (root-relative, `/`-separated) matches on its own,
    /// ignoring its ancestors: the last matching rule decides.
    fn direct(&self, path: &str, is_dir: bool) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        let mut ignored = false;
        for r in &self.rules {
            if r.dir_only && !is_dir {
                continue;
            }
            if match_segs(&r.segs, &parts) {
                ignored = !r.negate;
            }
        }
        ignored
    }

    /// Whether `path` is ignored: it matches, or any ancestor directory does.
    pub fn is_ignored(&self, path: &str, is_dir: bool) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        path.match_indices('/')
            .any(|(i, _)| self.direct(&path[..i], true))
            || self.direct(path, is_dir)
    }

    /// Like [`Ignore::is_ignored`] when the caller already knows no
    /// ancestor is ignored (a scan that does not descend ignored dirs).
    pub fn is_ignored_here(&self, path: &str, is_dir: bool) -> bool {
        !self.rules.is_empty() && self.direct(path, is_dir)
    }
}

fn match_segs(pat: &[String], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((p, rest)) if p == "**" => {
            if rest.is_empty() {
                // Trailing `/**`: one or more segments. A lone `**` pattern
                // (from unanchored `**`) matches anything.
                !path.is_empty()
            } else {
                (0..=path.len()).any(|i| match_segs(rest, &path[i..]))
            }
        }
        Some((p, rest)) => match path.split_first() {
            Some((s, prest)) => glob(p, s) && match_segs(rest, prest),
            None => false,
        },
    }
}

/// Single-segment glob: `*` any run, `?` one char (by UTF-8 char).
fn glob(p: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (p.chars().collect(), s.chars().collect());
    // Iterative wildcard match with backtracking to the last `*`.
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ig(t: &str) -> Ignore {
        Ignore::parse(t)
    }

    #[test]
    fn comments_blank_and_escapes() {
        let i = ig("# c\n\n   \n\\#lit\n\\!bang\nfoo   \n");
        assert!(i.is_ignored("#lit", false));
        assert!(i.is_ignored("!bang", false));
        assert!(i.is_ignored("foo", false));
        assert!(!i.is_ignored("# c", false));
    }

    #[test]
    fn star_and_question() {
        let i = ig("*.log\na?c\n");
        assert!(i.is_ignored("x.log", false));
        assert!(i.is_ignored("d/e/x.log", false));
        assert!(i.is_ignored(".log", false));
        assert!(!i.is_ignored("x.logs", false));
        assert!(i.is_ignored("abc", false));
        assert!(!i.is_ignored("ac", false));
        assert!(!i.is_ignored("abbc", false));
        // `*` does not cross `/` in anchored patterns.
        let j = ig("/a*z\n");
        assert!(j.is_ignored("abcz", false));
        assert!(!j.is_ignored("ab/cz", false));
    }

    #[test]
    fn anchoring() {
        let i = ig("/top\nmid/x\nany\n");
        assert!(i.is_ignored("top", false));
        assert!(!i.is_ignored("d/top", false));
        assert!(i.is_ignored("mid/x", false));
        assert!(!i.is_ignored("d/mid/x", false), "middle slash anchors");
        assert!(i.is_ignored("any", false));
        assert!(i.is_ignored("d/e/any", false));
    }

    #[test]
    fn dir_only() {
        let i = ig("build/\n");
        assert!(i.is_ignored("build", true));
        assert!(!i.is_ignored("build", false));
        assert!(i.is_ignored("x/build", true));
        assert!(i.is_ignored("build/a.o", false), "inside an ignored dir");
        assert!(!i.is_ignored("buildx", true));
    }

    #[test]
    fn negation_order() {
        let i = ig("*.log\n!keep.log\n");
        assert!(i.is_ignored("a.log", false));
        assert!(!i.is_ignored("keep.log", false));
        assert!(!i.is_ignored("d/keep.log", false));
        // Last match wins: re-ignoring after the negation.
        let j = ig("*.log\n!keep.log\nkeep.log\n");
        assert!(j.is_ignored("keep.log", false));
        // Negation cannot re-include inside an ignored directory.
        let k = ig("out/\n!out/keep\n");
        assert!(k.is_ignored("out/keep", false));
        // But `out/*` + `!out/keep` works, as in git.
        let m = ig("out/*\n!out/keep\n");
        assert!(!m.is_ignored("out/keep", false));
        assert!(m.is_ignored("out/other", false));
        assert!(!m.is_ignored("out", true));
    }

    #[test]
    fn double_star() {
        let i = ig("**/node_modules/\n");
        assert!(i.is_ignored("node_modules", true));
        assert!(i.is_ignored("a/b/node_modules", true));
        assert!(i.is_ignored("a/node_modules/x/y", false));
        assert!(!i.is_ignored("a/node_modules", false));
        let j = ig("a/**/b\n");
        assert!(j.is_ignored("a/b", false));
        assert!(j.is_ignored("a/x/y/b", false));
        assert!(!j.is_ignored("x/a/b", false));
        let k = ig("lib/**\n");
        assert!(
            !k.is_ignored("lib", true),
            "trailing /** excludes the dir itself"
        );
        assert!(k.is_ignored("lib/x", false));
        assert!(k.is_ignored("lib/x/y", false));
        let m = ig("a**b\n");
        assert!(m.is_ignored("axxb", false));
        assert!(!m.is_ignored("a/b", false));
    }

    #[test]
    fn empty_ignores_nothing() {
        assert!(!ig("").is_ignored("x", false));
        assert!(!ig("/\n!\n").is_ignored("x", false));
    }
}
