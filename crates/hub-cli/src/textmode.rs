//! Which saves are text (Text-CRDT 7.1, D-053, Issue #56) and
//! `.hubattributes`.
//!
//! A content value is **text** iff it is valid UTF-8, has no NUL byte, is at
//! most [`TEXT_MAX`] bytes and its longest line (bytes up to and including
//! `\n`; a leading BOM is its own pseudo-line) is at most [`LINE_MAX`]
//! bytes. Classification is made by the author, per save, on both the base
//! and the new content; receivers never re-classify.
//!
//! `.hubattributes` (E-T3) at the working-directory root: one rule per line,
//! `<pattern> text` or `<pattern> binary`, with the `.hubignore` pattern
//! syntax ([`crate::ignore`]) matched relative to the root; the last
//! matching line wins. `text` forces text (still UTF-8 and no NUL; the size
//! and line caps are lifted up to [`FORCED_MAX`]); `binary` forces bytes.
//! Blank lines and `#` comments are skipped; any other line is ignored with
//! a warning.

use std::fs;
use std::io;
use std::path::Path;

use crate::ignore::Ignore;

pub const FILE: &str = ".hubattributes";
/// 7.1 (c): size cap of an automatically classified text file.
pub const TEXT_MAX: usize = 8 << 20;
/// 7.1 (d): longest line of an automatically classified text file.
pub const LINE_MAX: usize = 64 << 10;
/// Hard cap of a file forced to text by `.hubattributes`.
pub const FORCED_MAX: usize = 64 << 20;

/// What `.hubattributes` says about one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Text,
    Binary,
}

/// Whether `bytes` are text under `mode` (7.1).
pub fn is_text(bytes: &[u8], mode: Mode) -> bool {
    let (max, line_max) = match mode {
        Mode::Binary => return false,
        Mode::Auto => (TEXT_MAX, LINE_MAX),
        Mode::Text => (FORCED_MAX, FORCED_MAX),
    };
    if bytes.len() > max || bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        return false;
    }
    hub_core::text::split_lines(bytes)
        .iter()
        .all(|l| l.len() <= line_max)
}

/// Parsed `.hubattributes`: per rule, a single-pattern matcher.
#[derive(Default)]
pub struct Attributes {
    rules: Vec<(Ignore, Mode)>,
}

impl Attributes {
    /// Parse the file's text; returns the rules and one warning per line
    /// that is neither blank, a comment, nor `<pattern> text|binary`.
    pub fn parse(text: &str) -> (Attributes, Vec<String>) {
        let mut rules = Vec::new();
        let mut warns = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let l = line.trim_end_matches([' ', '\t', '\r']);
            if l.trim_start().is_empty() || l.starts_with('#') {
                continue;
            }
            let parsed = l
                .rsplit_once([' ', '\t'])
                .map(|(p, a)| (p.trim_end_matches([' ', '\t']), a))
                .and_then(|(p, a)| match a {
                    "text" => Some((p, Mode::Text)),
                    "binary" => Some((p, Mode::Binary)),
                    _ => None,
                })
                .filter(|(p, _)| !p.is_empty());
            match parsed {
                Some((p, m)) => {
                    let ig = Ignore::parse(p);
                    if !ig.is_empty() {
                        rules.push((ig, m));
                    }
                }
                None => warns.push(format!(
                    "{FILE} line {}: expected `<pattern> text` or `<pattern> binary`; line ignored",
                    i + 1
                )),
            }
        }
        (Attributes { rules }, warns)
    }

    /// Load `<root>/.hubattributes` (absent or not a regular file: no
    /// rules).
    pub fn load(root: &Path) -> io::Result<(Attributes, Vec<String>)> {
        let p = root.join(FILE);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.is_file() => {}
            Ok(_) => return Ok((Attributes::default(), Vec::new())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok((Attributes::default(), Vec::new()))
            }
            Err(e) => return Err(e),
        }
        Ok(Attributes::parse(&String::from_utf8_lossy(&fs::read(p)?)))
    }

    /// The mode of a file at `path` (root-relative, `/`-separated): the
    /// last rule whose pattern matches the path (or one of its ancestor
    /// directories, as in `.hubignore`).
    pub fn mode(&self, path: &str) -> Mode {
        self.rules
            .iter()
            .rev()
            .find(|(ig, _)| ig.is_ignored(path, false))
            .map_or(Mode::Auto, |(_, m)| *m)
    }
}

/// `.hub/remote-text` (Issue #56, Text-CRDT 7.9): what the last sync with
/// each configured remote saw of its Hello feature `text`, one line per
/// remote: `text <name>` or `no-text <name>`. Local cache, not
/// replicated; a remote never synced has no line. `record` reads it
/// offline: while any configured remote is `no-text`, saves are authored
/// as `SetContent` (bytes mode) so that remote is never wedged with
/// `upgrade-required`.
pub const REMOTE_TEXT: &str = "remote-text";

fn read_remote_text(hub: &Path) -> io::Result<Vec<(bool, String)>> {
    let t = match fs::read_to_string(hub.join(REMOTE_TEXT)) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(t.lines()
        .filter_map(|l| match l.split_once(' ') {
            Some(("text", n)) => Some((true, n.to_string())),
            Some(("no-text", n)) => Some((false, n.to_string())),
            _ => None,
        })
        .collect())
}

/// Record what a sync with configured remote `name` saw (atomic replace).
pub fn remember_remote(hub: &Path, name: &str, text: bool) -> io::Result<()> {
    let mut v = read_remote_text(hub)?;
    if v.iter().any(|(t, n)| n == name && *t == text) {
        return Ok(());
    }
    v.retain(|(_, n)| n != name);
    v.push((text, name.to_string()));
    let body: String = v
        .iter()
        .map(|(t, n)| format!("{} {n}\n", if *t { "text" } else { "no-text" }))
        .collect();
    let tmp = hub.join(format!("{REMOTE_TEXT}.tmp"));
    fs::write(&tmp, body)?;
    fs::rename(tmp, hub.join(REMOTE_TEXT))
}

/// Why this repository must author bytes (`SetContent`) only, or `None`
/// when `TextEdit`s may be authored: this binary lacks wire feature
/// `text` (test hook), or a configured remote was last seen without it.
pub fn bytes_mode_reason(hub: &Path) -> Result<Option<String>, crate::Error> {
    if !crate::wire::Hello::new(hub_core::tree::ReplicaId(1), false).has(crate::wire::F_TEXT) {
        return Ok(Some(
            "this barocss-hub has wire feature `text` disabled".into(),
        ));
    }
    let cache = read_remote_text(hub)?;
    if cache.iter().all(|(t, _)| *t) {
        return Ok(None);
    }
    let names: Vec<String> = crate::remote::read_remotes(hub)?
        .into_iter()
        .map(|r| r.name)
        .collect();
    Ok(cache
        .iter()
        .find(|(t, n)| !*t && names.contains(n))
        .map(|(_, n)| {
            format!(
                "remote {n} lacks wire feature `text` (last sync); text files are recorded \
                 whole (SetContent) until it is upgraded or removed"
            )
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert!(is_text(b"", Mode::Auto));
        assert!(is_text(b"a\r\nb\n\xEF\xBB\xBF", Mode::Auto));
        assert!(!is_text(b"a\0b", Mode::Auto));
        assert!(!is_text(b"\xff", Mode::Auto));
        assert!(!is_text(b"a\0b", Mode::Text));
        assert!(!is_text(b"abc", Mode::Binary));
        let long = vec![b'x'; LINE_MAX + 1];
        assert!(!is_text(&long, Mode::Auto));
        assert!(is_text(&long, Mode::Text));
        let mut ok = vec![b'x'; LINE_MAX - 1];
        ok.push(b'\n');
        assert!(is_text(&ok, Mode::Auto));
        // The BOM is its own pseudo-line: a line of exactly LINE_MAX
        // after it still fits.
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend(vec![b'y'; LINE_MAX]);
        assert!(is_text(&bom, Mode::Auto));
        let big = "a\n".repeat(TEXT_MAX / 2 + 1);
        assert!(!is_text(big.as_bytes(), Mode::Auto));
        assert!(is_text(big.as_bytes(), Mode::Text));
    }

    #[test]
    fn attributes_last_match_wins() {
        let (a, w) = Attributes::parse(
            "# c\n*.min.js binary\n\n/gen/ binary\ngen/keep.js text\nbogus\n*.dat text\n",
        );
        assert_eq!(w.len(), 1);
        assert_eq!(a.mode("x/y.min.js"), Mode::Binary);
        assert_eq!(a.mode("a.js"), Mode::Auto);
        assert_eq!(a.mode("gen/a.c"), Mode::Binary);
        assert_eq!(a.mode("gen/keep.js"), Mode::Text);
        assert_eq!(a.mode("d/e.dat"), Mode::Text);
    }

    /// The pattern part is matched exactly as a `.hubignore` line would
    /// be (the matcher is reused; differential check on a grid).
    #[test]
    fn attributes_match_like_hubignore() {
        let pats = ["*.c", "/a", "a/", "a/**/b", "**/x", "d/*.t", "?.z", "a/**"];
        let paths = [
            "a", "a/b", "a/x/b", "x", "q/x", "d/e.t", "d/f/e.t", "k.z", "kk.z", "q.c",
        ];
        for p in pats {
            let (a, _) = Attributes::parse(&format!("{p} text"));
            let ig = Ignore::parse(p);
            for path in paths {
                assert_eq!(
                    a.mode(path) == Mode::Text,
                    ig.is_ignored(path, false),
                    "{p} {path}"
                );
            }
        }
    }
}
