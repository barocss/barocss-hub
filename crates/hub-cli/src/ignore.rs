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
//! # Built-in default ignores (Issue #37)
//! [`DEFAULTS`] (editor and OS temp files) are parsed as if they were the
//! first lines of every `.hubignore`, so a later `!pattern` in the file
//! re-includes any of them (last match wins). They apply even with no
//! `.hubignore`. As with any rule, a path that is already tracked stays
//! tracked (#24).
//!
//! The top-level `.hub/` directory is always excluded by the scanner and
//! never reaches this matcher.

use std::fs;
use std::io;
use std::path::Path;

pub const FILE: &str = ".hubignore";

/// Built-in default ignore patterns (Issue #37), in `.hubignore` syntax,
/// applied before the file's own lines. Same list in the README and the
/// Wiki (CLI-JSON / Repository-Semantics).
pub const DEFAULTS: &[&str] = &[
    // vim swap files (.x.swp, then .swo, .swn, ... when taken).
    "*.swp",
    "*.swo",
    "*.swn",
    "*.swx",
    // JetBrains "safe write".
    "*___jb_tmp___",
    "*___jb_old___",
    // LibreOffice / OpenOffice lock files.
    ".~lock.*#",
    // GNOME GIO atomic-replace temp files (gedit and other GTK editors).
    ".goutputstream-*",
    // OS metadata.
    ".DS_Store",
    "Thumbs.db",
    "desktop.ini",
];

/// When a [`CONDITIONAL`] default matches (Issue #37 repair: an editor
/// temp name that is also a plausible real file name is ignored only while
/// the file looks like the temp; otherwise it is recorded like any file).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cond {
    /// Always (an ordinary rule).
    Always,
    /// An empty regular file (vim's `4913` write probe).
    Empty,
    /// `x~` while `x` exists (vim / emacs backup).
    SiblingOfTilde,
    /// `#x#` while `x` exists (emacs auto-save).
    SiblingOfHashes,
    /// `.#x` that is a symlink, or a regular file of at most
    /// [`LOCK_MAX`] bytes, one line, containing `@` (emacs lock file:
    /// `user@host.pid:boot`).
    EmacsLock,
}

/// Largest regular file taken for an emacs `.#x` lock.
pub const LOCK_MAX: u64 = 256;

/// Built-in defaults that apply only under a condition (Issue #37 repair),
/// after [`DEFAULTS`] and before the `.hubignore` lines (so `!` still
/// re-includes them). Directories never match these.
pub const CONDITIONAL: &[(&str, Cond)] = &[
    ("4913", Cond::Empty),
    ("*~", Cond::SiblingOfTilde),
    ("\\#*#", Cond::SiblingOfHashes),
    (".#*", Cond::EmacsLock),
];

#[derive(Clone, Debug)]
struct Rule {
    cond: Cond,
    negate: bool,
    dir_only: bool,
    /// Pattern segments; unanchored patterns start with `**`.
    segs: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Ignore {
    rules: Vec<Rule>,
    /// Working-directory root, for [`Cond`] checks; without it conditional
    /// rules never match.
    root: Option<std::path::PathBuf>,
}

impl Ignore {
    /// Load `<root>/.hubignore`; absent (or not a regular file) = no rules.
    pub fn load(root: &Path) -> io::Result<Ignore> {
        let p = root.join(FILE);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.is_file() => {}
            Ok(_) => return Ok(Ignore::defaults_at(root)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Ignore::defaults_at(root)),
            Err(e) => return Err(e),
        }
        let bytes = fs::read(&p)?;
        let mut ig = Ignore::defaults_at(root);
        ig.rules
            .extend(Ignore::parse(&String::from_utf8_lossy(&bytes)).rules);
        Ok(ig)
    }

    /// Only the unconditional built-in [`DEFAULTS`].
    pub fn defaults() -> Ignore {
        Ignore::parse(&DEFAULTS.join("\n"))
    }

    /// All built-in defaults, [`CONDITIONAL`] ones checked under `root`.
    pub fn defaults_at(root: &Path) -> Ignore {
        let mut ig = Ignore::defaults();
        for (pat, cond) in CONDITIONAL {
            let mut r = Ignore::parse(pat).rules;
            for x in &mut r {
                x.cond = *cond;
            }
            ig.rules.extend(r);
        }
        ig.root = Some(root.to_path_buf());
        ig
    }

    /// Whether `path` is ignored by the built-in defaults alone (for the
    /// `status` hint).
    pub fn is_default_ignored(root: &Path, path: &str, is_dir: bool) -> bool {
        Ignore::defaults_at(root).is_ignored(path, is_dir)
    }

    fn cond_holds(&self, c: Cond, path: &str, is_dir: bool) -> bool {
        if c == Cond::Always {
            return true;
        }
        let Some(root) = &self.root else { return false };
        if is_dir {
            return false;
        }
        let exists = |p: &str| fs::symlink_metadata(root.join(p)).is_ok();
        let (dir, name) = match path.rsplit_once('/') {
            Some((d, n)) => (format!("{d}/"), n),
            None => (String::new(), path),
        };
        let md = fs::symlink_metadata(root.join(path));
        match c {
            Cond::Always => true,
            Cond::Empty => md.is_ok_and(|m| m.is_file() && m.len() == 0),
            Cond::SiblingOfTilde => {
                let base = &name[..name.len() - 1];
                !base.is_empty() && exists(&format!("{dir}{base}"))
            }
            Cond::SiblingOfHashes => {
                name.len() > 2 && exists(&format!("{dir}{}", &name[1..name.len() - 1]))
            }
            Cond::EmacsLock => match md {
                Ok(m) if m.file_type().is_symlink() => true,
                Ok(m) if m.is_file() && m.len() <= LOCK_MAX => fs::read(root.join(path))
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok())
                    .is_some_and(|t| t.contains('@') && !t.trim_end().contains('\n')),
                _ => false,
            },
        }
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
                cond: Cond::Always,
                negate,
                dir_only,
                segs,
            });
        }
        Ignore { rules, root: None }
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
            if match_segs(&r.segs, &parts) && self.cond_holds(r.cond, path, is_dir) {
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

    /// Issue #37: every default pattern, with a sample it must match (at
    /// the root and nested) and near misses it must not.
    #[test]
    fn defaults_match_editor_and_os_temp_files() {
        let d = Ignore::defaults();
        let hits = [
            ".x.rs.swp",
            ".x.rs.swo",
            ".x.rs.swn",
            ".x.rs.swx",
            "x.rs___jb_tmp___",
            "x.rs___jb_old___",
            ".~lock.report.odt#",
            ".goutputstream-AB12CD",
            ".DS_Store",
            "Thumbs.db",
            "desktop.ini",
        ];
        assert_eq!(hits.len(), DEFAULTS.len(), "one sample per default");
        for h in hits {
            assert!(d.is_ignored(h, false), "{h}");
            assert!(d.is_ignored(&format!("src/deep/{h}"), false), "nested {h}");
        }
        for miss in [
            "x.rs",
            "swp",
            "x.swp.rs",
            "49130",
            "~x",
            "x#",
            "x___jb_tmp",
            "DS_Store",
            "thumbs.db.txt",
            "#readme",
        ] {
            assert!(!d.is_ignored(miss, false), "{miss}");
        }
        // Each default is also its own rule: dropping any one of them makes
        // its sample tracked (mutation check of the list itself).
        for (i, h) in hits.iter().enumerate() {
            let mut rest: Vec<&str> = DEFAULTS.to_vec();
            rest.remove(i);
            assert!(!Ignore::parse(&rest.join("\n")).is_ignored(h, false), "{h}");
        }
    }

    /// Defaults come first: a `.hubignore` `!` line re-includes them.
    #[test]
    fn defaults_re_includable_from_hubignore() {
        let root = std::env::temp_dir().join(format!("hub-ig-defaults-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let i = Ignore::load(&root).unwrap();
        assert!(i.is_ignored("a.swp", false) && i.is_ignored(".DS_Store", false));
        fs::write(root.join(FILE), "!*.swp\n!*~\n*.log\n").unwrap();
        fs::write(root.join("x"), "").unwrap();
        fs::write(root.join("x~"), "").unwrap();
        let i = Ignore::load(&root).unwrap();
        assert!(!i.is_ignored("a.swp", false));
        assert!(!i.is_ignored("x~", false), "conditional re-included");
        assert!(
            i.is_ignored(".DS_Store", false),
            "other defaults still apply"
        );
        assert!(i.is_ignored("a.log", false));
        fs::remove_dir_all(&root).unwrap();
    }

    /// Issue #37 repair: the conditional defaults match only files that
    /// look like the editor's temp; the same names with real content (or
    /// without the sibling) are not ignored.
    #[test]
    fn conditional_defaults() {
        let root = std::env::temp_dir().join(format!("hub-ig-cond-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("d")).unwrap();
        let w = |p: &str, b: &[u8]| fs::write(root.join(p), b).unwrap();
        w("d/4913", b"");
        w("4913", b"data");
        w("a", b"a");
        w("a~", b"old a");
        w("notes~", b"notes");
        w("#a#", b"autosave");
        w("#todo#", b"real");
        w(".#a", b"me@host.123:456");
        w(".#draft", b"a real draft\nwith two lines @ home");
        w(".#big", &[b'@'; 300]);
        #[cfg(unix)]
        std::os::unix::fs::symlink("me@host.1:2", root.join(".#link")).unwrap();
        let i = Ignore::load(&root).unwrap();
        for p in ["d/4913", "a~", "#a#", ".#a"] {
            assert!(i.is_ignored(p, false), "{p}");
        }
        #[cfg(unix)]
        assert!(i.is_ignored(".#link", false));
        for p in ["4913", "notes~", "#todo#", ".#draft", ".#big"] {
            assert!(!i.is_ignored(p, false), "{p}");
        }
        // Directories never match a conditional default.
        fs::create_dir_all(root.join("b~")).unwrap();
        w("b", b"");
        assert!(!i.is_ignored("b~", true));
        // Without a root (plain parse) conditional rules never match.
        assert!(!Ignore::defaults().is_ignored("a~", false));
        assert_eq!(CONDITIONAL.len(), 4);
        fs::remove_dir_all(&root).unwrap();
    }
}
