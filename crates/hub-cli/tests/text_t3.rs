//! Issue #56 (T3): text merge in the working directory, end to end with
//! the real binary (Text-CRDT 7.1, 7.5, 7.8, 7.9, D-053, D-056):
//! * two and three replicas through a bare server over `file://` and
//!   SSH/stdio (fake ssh): different lines merge, the same line keeps both
//!   lines with a `concurrent-edit` notice, edit vs delete revives, a
//!   text->binary flip concurrent with text edits is a `content` conflict,
//!   a stale-epoch save is a visible loser, mixed line endings and a BOM
//!   round-trip byte-exact, an ~8 MiB save is a chained save,
//!   `.hubattributes` overrides both ways;
//! * 7.12 #18: authoring on the base after a remote op arrived without a
//!   checkout (merged, or a visible loser; never silently overwritten);
//! * sequential equivalence (7.12 #7) through the real `record` path;
//! * D-056: kill -9 (abort) at every crash point of a checkout-merge and of
//!   a watch-merge, then rerun: no edit lost, the same merged result.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

mod support;
use support::{bin, js, tmp, wait_for, Watcher, V};

fn hub_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = bin();
    c.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .env_remove("HUB_TEST_CRASH_STAGE")
        .env_remove("HUB_TEST_NO_TEXT_FEATURE");
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn ok_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let o = hub_env(dir, args, env);
    assert!(
        o.status.success(),
        "hub {args:?} in {}: {}{}",
        dir.display(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> String {
    ok_env(dir, args, &[])
}

fn read(p: &Path) -> Vec<u8> {
    fs::read(p).unwrap_or_default()
}

fn fake_ssh(dir: &Path) -> String {
    let p = dir.join("fake-ssh");
    fs::write(
        &p,
        "#!/bin/sh\nwhile [ \"$#\" -gt 0 ] && [ \"$1\" != \"--\" ]; do shift; done\nshift\nexec sh -c \"$2\"\n",
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&p).status().unwrap();
    p.to_string_lossy().into_owned()
}

/// A bare server and `n` replicas with remote `origin` over `via`
/// (`file` or `ssh`).
struct Net {
    reps: Vec<PathBuf>,
    env: Vec<(String, String)>,
}

impl Net {
    fn new(tag: &str, via: &str, n: usize) -> Net {
        let t = fs::canonicalize(tmp(&format!("t3-{tag}-{via}"))).unwrap();
        ok(&t, &["init", "--bare", "srv.hub"]);
        let srv = t.join("srv.hub");
        let mut env = Vec::new();
        let url = if via == "ssh" {
            env.push(("HUB_SSH_COMMAND".to_string(), fake_ssh(&t)));
            format!("ssh://localhost{}", srv.display())
        } else {
            format!("file://{}", srv.display())
        };
        let bin = env!("CARGO_BIN_EXE_barocss-hub");
        let mut reps = Vec::new();
        for i in 0..n {
            let d = t.join(format!("r{i}"));
            fs::create_dir_all(&d).unwrap();
            ok(&d, &["init"]);
            ok(&d, &["remote", "add", "--exec", bin, "origin", &url]);
            reps.push(d);
        }
        Net { reps, env }
    }
    fn env(&self) -> Vec<(&str, &str)> {
        self.env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }
    fn sync(&self, i: usize) {
        ok_env(&self.reps[i], &["sync"], &self.env());
    }
    /// Everyone syncs twice (all ops everywhere), then checks out.
    fn converge(&self) {
        for _ in 0..2 {
            for i in 0..self.reps.len() {
                self.sync(i);
            }
        }
        for d in &self.reps {
            ok(d, &["checkout"]);
        }
    }
    fn r(&self, i: usize) -> &Path {
        &self.reps[i]
    }
    /// Every replica has the same head, conflicts and file bytes.
    fn same(&self, file: &str) -> Vec<u8> {
        let b0 = read(&self.r(0).join(file));
        let s0 = js(self.r(0), &["status"]);
        for d in &self.reps[1..] {
            assert_eq!(read(&d.join(file)), b0, "{}", d.display());
            let s = js(d, &["status"]);
            assert_eq!(s.get("head"), s0.get("head"));
            assert_eq!(s.get("conflicts"), s0.get("conflicts"));
            assert_eq!(s.get("notices"), s0.get("notices"));
        }
        b0
    }
    /// Write `f` in replica 0, record, and give it to everyone (checked out).
    fn seed(&self, f: &str, bytes: &[u8]) {
        fs::write(self.r(0).join(f), bytes).unwrap();
        ok(self.r(0), &["record"]);
        self.converge();
    }
}

fn kinds(d: &Path, path: &str) -> Vec<String> {
    js(d, &["log", "--path", path])
        .get("ops")
        .arr()
        .iter()
        .map(|o| o.get("kind").str().to_string())
        .collect()
}

fn conflict_kinds(d: &Path) -> Vec<String> {
    js(d, &["status"])
        .get("conflicts")
        .arr()
        .iter()
        .map(|c| c.get("kind").str().to_string())
        .collect()
}

const VIAS: [&str; 2] = ["file", "ssh"];

#[test]
fn different_lines_merge_without_conflict_two_and_three_replicas() {
    for via in VIAS {
        for n in [2, 3] {
            let net = Net::new(&format!("lines{n}"), via, n);
            net.seed("f.txt", b"one\ntwo\nthree\nfour\n");
            fs::write(net.r(0).join("f.txt"), b"ONE\ntwo\nthree\nfour\n").unwrap();
            fs::write(net.r(1).join("f.txt"), b"one\ntwo\nTHREE\nfour\n").unwrap();
            if n == 3 {
                fs::write(net.r(2).join("f.txt"), b"one\ntwo\nthree\nfour\nfive\n").unwrap();
            }
            for i in 0..n {
                ok(net.r(i), &["record"]);
            }
            net.converge();
            let want: &[u8] = if n == 3 {
                b"ONE\ntwo\nTHREE\nfour\nfive\n"
            } else {
                b"ONE\ntwo\nTHREE\nfour\n"
            };
            assert_eq!(net.same("f.txt"), want, "{via} {n}");
            assert!(conflict_kinds(net.r(0)).is_empty());
            assert!(js(net.r(0), &["status"]).get("notices").arr().is_empty());
            assert_eq!(kinds(net.r(0), "f.txt")[0], "text-edit");
        }
    }
}

#[test]
fn same_line_keeps_both_lines_with_a_notice() {
    for via in VIAS {
        let net = Net::new("sameline", via, 2);
        net.seed("f.txt", b"head\nmid\ntail\n");
        fs::write(net.r(0).join("f.txt"), b"head\nmid A\ntail\n").unwrap();
        fs::write(net.r(1).join("f.txt"), b"head\nmid B\ntail\n").unwrap();
        ok(net.r(0), &["record"]);
        ok(net.r(1), &["record"]);
        net.converge();
        let got = net.same("f.txt");
        assert!(
            got == b"head\nmid A\nmid B\ntail\n" || got == b"head\nmid B\nmid A\ntail\n",
            "{via}: {}",
            String::from_utf8_lossy(&got)
        );
        assert!(conflict_kinds(net.r(0)).is_empty());
        let st = js(net.r(0), &["status"]);
        let ns = st.get("notices").arr();
        assert_eq!(ns.len(), 1, "{via}");
        assert_eq!(ns[0].get("kind").str(), "concurrent-edit");
        assert_eq!(ns[0].get("path").str(), "f.txt");
        assert_eq!(ns[0].get("ops").arr().len(), 2);
        assert_eq!(ns[0].get("authors").arr().len(), 2);
        assert_eq!(ns[0].get("lines"), &V::Arr(vec![V::Num(2.0), V::Num(3.0)]));
        let plain = ok(net.r(0), &["status"]);
        assert!(
            plain.contains("notice: concurrent edits merged in f.txt lines 2-3"),
            "{plain}"
        );
        // A later edit after the merge clears it.
        fs::write(net.r(0).join("f.txt"), b"head\nmerged\ntail\n").unwrap();
        ok(net.r(0), &["record"]);
        net.converge();
        assert!(js(net.r(1), &["status"]).get("notices").arr().is_empty());
        assert_eq!(net.same("f.txt"), b"head\nmerged\ntail\n");
    }
}

#[test]
fn edit_concurrent_with_delete_revives() {
    for via in VIAS {
        let net = Net::new("revive", via, 2);
        net.seed("f.txt", b"keep\nme\n");
        ok(net.r(0), &["rm", "f.txt"]);
        fs::write(net.r(1).join("f.txt"), b"keep\nme\nedited\n").unwrap();
        ok(net.r(1), &["record"]);
        net.converge();
        assert_eq!(net.same("f.txt"), b"keep\nme\nedited\n", "{via}");
        assert!(conflict_kinds(net.r(0)).contains(&"edit-revived".to_string()));
    }
}

#[test]
fn flip_to_binary_concurrent_with_text_edits_is_a_content_conflict() {
    for via in VIAS {
        let net = Net::new("flip", via, 3);
        net.seed("f.txt", b"a\nb\n");
        fs::write(net.r(0).join("f.txt"), b"bin\0ary").unwrap();
        fs::write(net.r(1).join("f.txt"), b"a\nb\nc\n").unwrap();
        fs::write(net.r(2).join("f.txt"), b"z\na\nb\n").unwrap();
        for i in 0..3 {
            ok(net.r(i), &["record"]);
        }
        assert_eq!(kinds(net.r(0), "f.txt")[0], "set-content");
        net.converge();
        let got = net.same("f.txt");
        let cs = js(net.r(0), &["status"]).get("conflicts").arr().to_vec();
        assert!(
            cs.iter().any(|c| c.get("kind").str() == "content"),
            "{via}: {cs:?}"
        );
        // Nothing is lost: the head and every loser blob are readable.
        let mut seen = vec![got];
        for c in &cs {
            if c.get("kind").str() == "content" {
                for k in ["winner_blob", "loser_blob"] {
                    let o = hub_env(net.r(1), &["cat-blob", c.get(k).str()], &[]);
                    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
                    seen.push(o.stdout);
                }
            }
        }
        let merged_text = seen
            .iter()
            .any(|b| b == b"z\na\nb\nc\n" || b == b"a\nb\nc\n" || b == b"z\na\nb\n");
        assert!(seen.iter().any(|b| b == b"bin\0ary"), "{via}");
        assert!(merged_text, "{via}: {seen:?}");
    }
}

/// 7.12 #16: a save on a superseded epoch is a visible loser. The new
/// epoch is a `SetContent` forced by `.hubattributes` (`binary`), here of
/// still-text content.
#[test]
fn stale_epoch_save_is_a_visible_loser_and_attributes_force_bytes() {
    for via in VIAS {
        let net = Net::new("stale", via, 2);
        net.seed("f.txt", b"one\ntwo\n");
        fs::write(net.r(0).join(".hubattributes"), b"f.txt binary\n").unwrap();
        fs::write(net.r(0).join("f.txt"), b"one\ntwo\nnew epoch\n").unwrap();
        ok(net.r(0), &["record"]);
        assert_eq!(kinds(net.r(0), "f.txt")[0], "set-content");
        fs::write(net.r(1).join("f.txt"), b"one\nTWO\n").unwrap();
        ok(net.r(1), &["record"]);
        assert_eq!(kinds(net.r(1), "f.txt")[0], "text-edit");
        net.converge();
        net.same("f.txt");
        let cs = js(net.r(1), &["status"]).get("conflicts").arr().to_vec();
        let c = cs
            .iter()
            .find(|c| c.get("kind").str() == "content")
            .unwrap_or_else(|| panic!("{via}: {cs:?}"));
        let blobs: Vec<Vec<u8>> = ["winner_blob", "loser_blob"]
            .iter()
            .map(|k| ok(net.r(1), &["cat-blob", c.get(k).str()]).into_bytes())
            .collect();
        assert!(blobs.contains(&b"one\nTWO\n".to_vec()), "{via}: {blobs:?}");
        assert!(blobs.contains(&b"one\ntwo\nnew epoch\n".to_vec()));
    }
}

/// `.hubattributes` `text` lifts the line cap (a > 64 KiB line is bytes
/// by default), `binary` forces bytes; default classification otherwise.
#[test]
fn hubattributes_override_both_ways() {
    let net = Net::new("attrs", "file", 2);
    let long = "x".repeat(70 * 1024);
    let body = format!("short\n{long}\n");
    net.seed("min.js", body.as_bytes());
    fs::write(net.r(0).join("min.js"), format!("{body}more\n")).unwrap();
    ok(net.r(0), &["record"]);
    assert_eq!(kinds(net.r(0), "min.js")[0], "set-content");
    fs::write(net.r(0).join(".hubattributes"), b"*.js text\n").unwrap();
    fs::write(net.r(0).join("min.js"), format!("{body}more\nmore\n")).unwrap();
    ok(net.r(0), &["record"]);
    assert_eq!(kinds(net.r(0), "min.js")[0], "text-edit");
    // A later rule wins: `binary` again for this file only.
    fs::write(
        net.r(0).join(".hubattributes"),
        b"*.js text\nmin.js binary\n",
    )
    .unwrap();
    fs::write(net.r(0).join("min.js"), format!("{body}m\n")).unwrap();
    ok(net.r(0), &["record"]);
    assert_eq!(kinds(net.r(0), "min.js")[0], "set-content");
    net.converge();
    assert_eq!(net.same("min.js"), format!("{body}m\n").as_bytes());
    // A NUL byte is never text, even when forced.
    fs::write(net.r(0).join(".hubattributes"), b"* text\n").unwrap();
    fs::write(net.r(0).join("min.js"), b"a\0b\n").unwrap();
    ok(net.r(0), &["record"]);
    assert_eq!(kinds(net.r(0), "min.js")[0], "set-content");
}

#[test]
fn mixed_line_endings_and_bom_round_trip_byte_exact() {
    for via in VIAS {
        let net = Net::new("eol", via, 2);
        let base = b"\xEF\xBB\xBFfirst\r\nsecond\nthird\r\nno newline".to_vec();
        net.seed("w.txt", &base);
        assert_eq!(read(&net.r(1).join("w.txt")), base);
        fs::write(
            net.r(0).join("w.txt"),
            b"\xEF\xBB\xBFtop\r\nfirst\r\nsecond\nthird\r\nno newline",
        )
        .unwrap();
        fs::write(
            net.r(1).join("w.txt"),
            b"\xEF\xBB\xBFfirst\r\nsecond\nthird\r\nno newline\r\n",
        )
        .unwrap();
        ok(net.r(0), &["record"]);
        ok(net.r(1), &["record"]);
        net.converge();
        // The insert at the top goes after the BOM; the last line was
        // replaced by r1 (delete + insert of that line).
        assert_eq!(
            net.same("w.txt"),
            b"\xEF\xBB\xBFtop\r\nfirst\r\nsecond\nthird\r\nno newline\r\n",
            "{via}"
        );
    }
}

#[test]
fn eight_mib_save_is_a_chained_save() {
    let net = Net::new("big", "file", 2);
    // ~7.6 MiB of 64-byte lines, then a 400 KiB insert (total < 8 MiB):
    // over TEXT_OP_MAX (192 KiB), so the save is a chain of TextEdits.
    let line = |i: usize, tag: &str| format!("{tag}{i:>58}\n").into_bytes();
    let mut base = Vec::new();
    let mut i = 0;
    while base.len() < 7_600_000 {
        base.extend(line(i, "base "));
        i += 1;
    }
    net.seed("big.txt", &base);
    let mut saved = base.clone();
    for j in 0..6400 {
        saved.extend(line(j, "new  "));
    }
    assert!(saved.len() <= 8 << 20);
    fs::write(net.r(0).join("big.txt"), &saved).unwrap();
    ok(net.r(0), &["record"]);
    let k = kinds(net.r(0), "big.txt");
    let chain = k.iter().take_while(|x| *x == "text-edit").count();
    assert!(chain >= 2, "{k:?}");
    net.converge();
    assert_eq!(net.same("big.txt"), saved);
}

/// 7.12 #18: a remote `TextEdit` delivered without a checkout, then a
/// save: the save is authored on its base and merged (D-056 checkout).
/// A remote `SetContent` (flip to bytes) delivered without a checkout:
/// checkout refuses (flip exception) and keeps the file; the watcher
/// records the save on its base, a visible loser.
#[test]
fn authoring_on_the_base_never_silently_overwrites() {
    let net = Net::new("base18", "file", 2);
    net.seed("f.txt", b"1\n2\n3\n");
    fs::write(net.r(0).join("f.txt"), b"1\n2\n3\nremote\n").unwrap();
    ok(net.r(0), &["record"]);
    net.sync(0);
    net.sync(1);
    assert_eq!(
        js(net.r(1), &["status"]).get("workdir").get("state").str(),
        "behind"
    );
    fs::write(net.r(1).join("f.txt"), b"local\n1\n2\n3\n").unwrap();
    let o = js(net.r(1), &["checkout"]);
    assert_eq!(o.get("at_head"), &V::Bool(true));
    assert_eq!(read(&net.r(1).join("f.txt")), b"local\n1\n2\n3\nremote\n");
    net.converge();
    assert_eq!(net.same("f.txt"), b"local\n1\n2\n3\nremote\n");

    // Remote flip to bytes, not checked out; local text save.
    fs::write(net.r(0).join("f.txt"), b"bytes\0now").unwrap();
    ok(net.r(0), &["record"]);
    net.sync(0);
    net.sync(1);
    let mine = b"local\n1\n2\n3\nremote\nmine\n";
    fs::write(net.r(1).join("f.txt"), mine).unwrap();
    let o = hub_env(net.r(1), &["checkout"], &[]);
    assert!(!o.status.success(), "flip exception: checkout refuses");
    assert_eq!(read(&net.r(1).join("f.txt")), mine, "kept on disk");
    {
        let w = Watcher::start(net.r(1), 100, &[]);
        wait_for("recorded on its base", || {
            js(net.r(1), &["status"])
                .get("conflicts")
                .arr()
                .iter()
                .any(|c| c.get("kind").str() == "content")
        });
        drop(w);
    }
    let cs = js(net.r(1), &["status"]).get("conflicts").arr().to_vec();
    let c = cs
        .iter()
        .find(|c| c.get("kind").str() == "content")
        .unwrap();
    let blobs: Vec<Vec<u8>> = ["winner_blob", "loser_blob"]
        .iter()
        .map(|k| hub_env(net.r(1), &["cat-blob", c.get(k).str()], &[]).stdout)
        .collect();
    assert!(blobs.contains(&mine.to_vec()), "{blobs:?}");
}

/// 7.12 #7 through the real `record`: any sequence of saves recorded one
/// by one materializes each save byte-exact, every save after the first
/// content is a `TextEdit`, and a fresh replica reaches the same bytes.
#[test]
fn sequential_equivalence_through_record() {
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut rnd = |n: u64| {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s % n
    };
    for round in 0..3 {
        let net = Net::new(&format!("seq{round}"), "file", 2);
        let mut lines: Vec<String> = (0..6).map(|i| format!("l{i}\n")).collect();
        net.seed("s.txt", lines.concat().as_bytes());
        let pool = ["", "x", "é", "\r", "tab\t", "longer line"];
        for step in 0..25 {
            match rnd(4) {
                0 if !lines.is_empty() => {
                    let k = rnd(lines.len() as u64) as usize;
                    lines.remove(k);
                }
                1 if !lines.is_empty() => {
                    let k = rnd(lines.len() as u64) as usize;
                    lines[k] = format!("{}{step}\n", pool[rnd(6) as usize]);
                }
                _ => {
                    let k = rnd(lines.len() as u64 + 1) as usize;
                    lines.insert(k, format!("{}{step}\n", pool[rnd(6) as usize]));
                }
            }
            let mut text = lines.concat();
            if rnd(5) == 0 {
                text.pop();
            }
            fs::write(net.r(0).join("s.txt"), &text).unwrap();
            ok(net.r(0), &["record"]);
            ok(net.r(0), &["checkout"]);
            assert_eq!(
                read(&net.r(0).join("s.txt")),
                text.as_bytes(),
                "{round}/{step}"
            );
            let id = js(net.r(0), &["status"])
                .get("head")
                .get("version")
                .get("short")
                .str()
                .to_string();
            let o = hub_env(net.r(0), &["cat", &id, "s.txt"], &[]);
            assert_eq!(o.stdout, text.as_bytes());
        }
        let k = kinds(net.r(0), "s.txt");
        assert!(
            k.iter().filter(|x| *x == "text-edit").count() >= 20,
            "{k:?}"
        );
        let last = read(&net.r(0).join("s.txt"));
        net.converge();
        assert_eq!(net.same("s.txt"), last);
    }
}

// ------------------------------------------------------------ D-056 crash

/// Every crash point a checkout-merge or a watch-merge passes (record:
/// blobs, `index.next`, op batch, promotion; checkout: working files).
const STAGES: &[&str] = &[
    "blobs-written",
    "blobs-barriered",
    "blobs-renamed",
    "v2-replica-tmp",
    "v2-switched",
    "v2-format",
    "record-index-next",
    "ops-appended",
    "commit-written",
    "ops-fsynced",
    "record-ops-appended",
    "record-index-promoted",
    "files-written",
    "files-barriered",
    "files-renamed",
];

/// r1 behind with an unrecorded text edit; the head holds r0's edit.
fn crash_setup(tag: &str) -> Net {
    let net = Net::new(tag, "file", 2);
    // Two files so the merge writes more than one working file.
    fs::write(net.r(0).join("g.txt"), b"g\n").unwrap();
    net.seed("f.txt", b"1\n2\n3\n");
    fs::write(net.r(0).join("f.txt"), b"1\n2\n3\nremote\n").unwrap();
    fs::write(net.r(0).join("g.txt"), b"g\ng2\n").unwrap();
    ok(net.r(0), &["record"]);
    net.sync(0);
    net.sync(1);
    fs::write(net.r(1).join("f.txt"), b"local\n1\n2\n3\n").unwrap();
    net
}

const MERGED: &[u8] = b"local\n1\n2\n3\nremote\n";

fn check_merged(net: &Net, what: &str) {
    assert_eq!(read(&net.r(1).join("f.txt")), MERGED, "{what}");
    assert_eq!(read(&net.r(1).join("g.txt")), b"g\ng2\n", "{what}");
    let st = js(net.r(1), &["status"]);
    assert_eq!(st.get("workdir").get("state").str(), "at-head", "{what}");
    assert!(st.get("changes").arr().is_empty(), "{what}");
    assert!(st.get("conflicts").arr().is_empty(), "{what}");
    // Exactly one save of r1 (never authored twice).
    let k = kinds(net.r(1), "f.txt");
    assert_eq!(
        k.iter().filter(|x| *x == "text-edit").count(),
        2,
        "{what}: {k:?}"
    );
}

/// The stages every merge must actually abort at (the others may be off
/// the path, e.g. the v2 upgrade of an already-v2 repository).
const MUST: &[&str] = &[
    "blobs-written",
    "record-index-next",
    "ops-appended",
    "commit-written",
    "ops-fsynced",
    "record-ops-appended",
    "record-index-promoted",
    "files-written",
    "files-barriered",
    "files-renamed",
];

#[test]
fn checkout_merge_survives_kill_at_every_step() {
    let mut crashed = Vec::new();
    for (i, st) in STAGES.iter().enumerate() {
        let net = crash_setup(&format!("ckc{i}"));
        let o = hub_env(net.r(1), &["checkout"], &[("HUB_TEST_CRASH_STAGE", st)]);
        // Every dirty byte is in an op or still on disk.
        let disk = read(&net.r(1).join("f.txt"));
        assert!(
            disk == b"local\n1\n2\n3\n" || disk == MERGED,
            "{st}: {disk:?}"
        );
        if o.status.success() {
            // The stage was not reached by this path.
            check_merged(&net, st);
            continue;
        }
        crashed.push(*st);
        ok(net.r(1), &["checkout"]);
        check_merged(&net, st);
    }
    for m in MUST {
        assert!(crashed.contains(m), "{m} never hit: {crashed:?}");
    }
}

#[test]
fn watch_merge_survives_kill_at_every_step() {
    let mut crashed = Vec::new();
    for (i, st) in STAGES.iter().enumerate() {
        let net = crash_setup(&format!("wkc{i}"));
        {
            let mut w = Watcher::start(net.r(1), 100, &[("HUB_TEST_CRASH_STAGE", st)]);
            // Aborts at the stage, or merges if the stage is not on the
            // path.
            let c = w.child.as_mut().unwrap();
            let t = std::time::Instant::now();
            loop {
                if let Some(x) = c.try_wait().unwrap() {
                    assert!(!x.success(), "{st}: watch exited");
                    crashed.push(*st);
                    break;
                }
                if read(&net.r(1).join("f.txt")) == MERGED
                    && js(net.r(1), &["status"]).get("changes").arr().is_empty()
                {
                    break;
                }
                assert!(t.elapsed() < std::time::Duration::from_secs(60), "{st}");
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
        let disk = read(&net.r(1).join("f.txt"));
        assert!(
            disk == b"local\n1\n2\n3\n" || disk == MERGED,
            "{st}: {disk:?}"
        );
        {
            let _w = Watcher::start(net.r(1), 100, &[]);
            wait_for(&format!("merged after rerun ({st})"), || {
                read(&net.r(1).join("f.txt")) == MERGED
                    && js(net.r(1), &["status"]).get("workdir").get("state").str() == "at-head"
            });
        }
        check_merged(&net, st);
    }
    for m in MUST {
        assert!(crashed.contains(m), "{m} never hit: {crashed:?}");
    }
}
