//! Shared helpers for the Issue #37 tests (real binary, temp dirs), with a
//! small strict JSON parser so every `--json` document is checked for
//! well-formedness without a JSON dependency.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

pub const DEADLINE: Duration = Duration::from_secs(60);

pub fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hub-37-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

pub fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_barocss-hub"));
    c.env("XDG_CONFIG_HOME", xdg());
    c
}

pub fn hub(dir: &Path, args: &[&str]) -> Output {
    bin()
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

pub fn ok(dir: &Path, args: &[&str]) -> String {
    let o = hub(dir, args);
    assert!(
        o.status.success(),
        "hub {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

/// Run with `--json`: success, stdout is exactly one JSON line, stderr
/// empty. Returns the parsed document.
pub fn js(dir: &Path, args: &[&str]) -> V {
    let mut a = args.to_vec();
    a.push("--json");
    let o = hub(dir, &a);
    let (out, err) = (
        String::from_utf8(o.stdout).unwrap(),
        String::from_utf8(o.stderr).unwrap(),
    );
    assert!(o.status.success(), "hub {a:?} failed: {err}");
    assert!(err.is_empty(), "stderr under --json: {err}");
    one_doc(&out)
}

/// Run with `--json` expecting failure: exit 1, stdout empty, stderr one
/// JSON error document.
pub fn js_err(dir: &Path, args: &[&str]) -> V {
    let mut a = args.to_vec();
    a.push("--json");
    let o = hub(dir, &a);
    assert_eq!(o.status.code(), Some(1), "hub {a:?}");
    assert!(o.stdout.is_empty(), "stdout on error: {:?}", o.stdout);
    let d = one_doc(&String::from_utf8(o.stderr).unwrap());
    assert_eq!(d.get("schema"), &V::Num(1.0));
    assert_eq!(d.get("program").str(), "barocss-hub");
    d
}

pub fn one_doc(s: &str) -> V {
    assert!(s.ends_with('\n'), "no trailing newline: {s:?}");
    assert_eq!(s.matches('\n').count(), 1, "not one line: {s:?}");
    parse(s.trim_end()).unwrap_or_else(|e| panic!("bad JSON ({e}): {s}"))
}

/// Pin a fresh repository's replica id, so ids and version hashes are
/// deterministic (golden tests). Only before any op is recorded.
pub fn set_replica(dir: &Path, rid: u128) {
    let p = dir.join(".hub/replica");
    let t = fs::read_to_string(&p).unwrap();
    let header = t.lines().next().unwrap().to_string();
    fs::write(&p, format!("{header}\n{rid:032x}\n")).unwrap();
}

// ---------- JSON ----------

#[derive(Clone, Debug, PartialEq)]
pub enum V {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<V>),
    /// Keys in document order (duplicates rejected by the parser).
    Obj(Vec<(String, V)>),
}

impl V {
    pub fn get(&self, k: &str) -> &V {
        match self {
            V::Obj(v) => v
                .iter()
                .find(|(x, _)| x == k)
                .map(|(_, v)| v)
                .unwrap_or_else(|| panic!("no key {k} in {self:?}")),
            _ => panic!("not an object: {self:?}"),
        }
    }
    pub fn has(&self, k: &str) -> bool {
        matches!(self, V::Obj(v) if v.iter().any(|(x, _)| x == k))
    }
    pub fn str(&self) -> &str {
        match self {
            V::Str(s) => s,
            _ => panic!("not a string: {self:?}"),
        }
    }
    pub fn arr(&self) -> &[V] {
        match self {
            V::Arr(v) => v,
            _ => panic!("not an array: {self:?}"),
        }
    }
    pub fn keys(&self) -> Vec<&str> {
        match self {
            V::Obj(v) => v.iter().map(|(k, _)| k.as_str()).collect(),
            _ => panic!("not an object"),
        }
    }
}

pub fn parse(s: &str) -> Result<V, String> {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let v = value(&b, &mut i)?;
    ws(&b, &mut i);
    if i != b.len() {
        return Err(format!("trailing data at {i}"));
    }
    Ok(v)
}

fn ws(b: &[char], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], ' ' | '\t' | '\n' | '\r') {
        *i += 1;
    }
}

fn value(b: &[char], i: &mut usize) -> Result<V, String> {
    ws(b, i);
    let lit = |i: &mut usize, w: &str, v: V| -> Result<V, String> {
        let w: Vec<char> = w.chars().collect();
        if b.len() >= *i + w.len() && b[*i..*i + w.len()] == w[..] {
            *i += w.len();
            Ok(v)
        } else {
            Err(format!("bad literal at {i}"))
        }
    };
    match b.get(*i) {
        Some('n') => lit(i, "null", V::Null),
        Some('t') => lit(i, "true", V::Bool(true)),
        Some('f') => lit(i, "false", V::Bool(false)),
        Some('"') => string(b, i).map(V::Str),
        Some('[') => {
            *i += 1;
            let mut v = Vec::new();
            ws(b, i);
            if b.get(*i) == Some(&']') {
                *i += 1;
                return Ok(V::Arr(v));
            }
            loop {
                v.push(value(b, i)?);
                ws(b, i);
                match b.get(*i) {
                    Some(',') => *i += 1,
                    Some(']') => {
                        *i += 1;
                        return Ok(V::Arr(v));
                    }
                    _ => return Err(format!("bad array at {i}")),
                }
            }
        }
        Some('{') => {
            *i += 1;
            let mut v: Vec<(String, V)> = Vec::new();
            ws(b, i);
            if b.get(*i) == Some(&'}') {
                *i += 1;
                return Ok(V::Obj(v));
            }
            loop {
                ws(b, i);
                let k = string(b, i)?;
                if v.iter().any(|(x, _)| *x == k) {
                    return Err(format!("duplicate key {k}"));
                }
                ws(b, i);
                if b.get(*i) != Some(&':') {
                    return Err(format!("missing : at {i}"));
                }
                *i += 1;
                v.push((k, value(b, i)?));
                ws(b, i);
                match b.get(*i) {
                    Some(',') => *i += 1,
                    Some('}') => {
                        *i += 1;
                        return Ok(V::Obj(v));
                    }
                    _ => return Err(format!("bad object at {i}")),
                }
            }
        }
        Some(c) if *c == '-' || c.is_ascii_digit() => {
            let st = *i;
            while *i < b.len() && (b[*i].is_ascii_digit() || "+-.eE".contains(b[*i])) {
                *i += 1;
            }
            let t: String = b[st..*i].iter().collect();
            t.parse().map(V::Num).map_err(|_| format!("bad number {t}"))
        }
        _ => Err(format!("unexpected at {i}")),
    }
}

fn string(b: &[char], i: &mut usize) -> Result<String, String> {
    if b.get(*i) != Some(&'"') {
        return Err(format!("expected string at {i}"));
    }
    *i += 1;
    let mut s = String::new();
    loop {
        let c = *b.get(*i).ok_or("unterminated string")?;
        *i += 1;
        match c {
            '"' => return Ok(s),
            '\\' => {
                let e = *b.get(*i).ok_or("bad escape")?;
                *i += 1;
                s.push(match e {
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    'u' => {
                        let h: String = b.get(*i..*i + 4).ok_or("bad \\u")?.iter().collect();
                        *i += 4;
                        let n = u32::from_str_radix(&h, 16).map_err(|_| "bad \\u")?;
                        char::from_u32(n).ok_or("surrogate \\u (not produced by hub)")?
                    }
                    _ => return Err("bad escape".into()),
                });
            }
            c if (c as u32) < 0x20 => return Err("raw control char in string".into()),
            c => s.push(c),
        }
    }
}

// ---------- watch ----------

/// A `watch --json` child, killed and reaped on drop.
pub struct Watcher {
    pub child: Option<Child>,
    pub log: PathBuf,
}

impl Watcher {
    pub fn start(dir: &Path, debounce_ms: u64, env: &[(&str, &str)]) -> Watcher {
        let log = dir.with_extension(format!(
            "watch-{}.jsonl",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let f = fs::File::create(&log).unwrap();
        let err = fs::File::create(log.with_extension("err")).unwrap();
        let mut c = bin();
        c.args([
            "watch",
            "--json",
            "--poll",
            "50",
            "--debounce",
            &debounce_ms.to_string(),
            "--sync",
            "500",
        ])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(f)
        .stderr(err);
        for (k, v) in env {
            c.env(k, v);
        }
        let w = Watcher {
            child: Some(c.spawn().unwrap()),
            log,
        };
        wait_for("watch started", || {
            w.events().iter().any(|e| e.get("event").str() == "started")
        });
        w
    }
    /// Every event so far (each line must be one valid JSON document).
    pub fn events(&self) -> Vec<V> {
        let t = fs::read_to_string(&self.log).unwrap_or_default();
        // Only complete lines.
        let done = t.rfind('\n').map_or("", |i| &t[..i + 1]);
        done.lines()
            .map(|l| {
                let v = parse(l).unwrap_or_else(|e| panic!("bad event ({e}): {l}"));
                assert_eq!(v.get("schema"), &V::Num(1.0));
                v
            })
            .collect()
    }
    pub fn stderr(&self) -> String {
        fs::read_to_string(self.log.with_extension("err")).unwrap_or_default()
    }
    pub fn of(&self, name: &str) -> Vec<V> {
        self.events()
            .into_iter()
            .filter(|e| e.get("event").str() == name)
            .collect()
    }
    /// SIGTERM, wait for a clean exit; returns all events.
    pub fn stop(mut self) -> Vec<V> {
        let c = self.child.as_mut().unwrap();
        let st = Command::new("kill")
            .args(["-TERM", &c.id().to_string()])
            .status()
            .unwrap();
        assert!(st.success());
        let t = Instant::now();
        loop {
            if let Some(s) = c.try_wait().unwrap() {
                assert!(s.success(), "watch exit {s:?}: {}", self.stderr());
                break;
            }
            assert!(t.elapsed() < DEADLINE, "watcher did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
        self.child = None;
        assert!(self.stderr().is_empty(), "watch stderr: {}", self.stderr());
        self.events()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

pub fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    loop {
        if f() {
            return;
        }
        assert!(t.elapsed() < DEADLINE, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Path -> node id of the head tree (`ls --json`).
pub fn nodes(dir: &Path) -> BTreeMap<String, String> {
    js(dir, &["ls"])
        .get("entries")
        .arr()
        .iter()
        .map(|e| {
            (
                e.get("path").str().to_string(),
                e.get("node").str().to_string(),
            )
        })
        .collect()
}

/// Issue #45: a private trusted-remotes store per test process.
fn xdg() -> String {
    static D: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    D.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("hub45-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into_owned()
    })
    .clone()
}
