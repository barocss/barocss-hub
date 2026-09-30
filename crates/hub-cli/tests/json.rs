//! Issue #37: `--json` output, schema 1. Golden documents for a fixed
//! repository (replica id pinned so every id is deterministic), the
//! stdout/stderr contract for every command, and the fields #38 needs
//! (sync state, pin, watch pid, conflicts with blob ids, per-file history).
//!
//! `UPDATE_GOLDEN=1 cargo test -p barocss-hub-cli --test json` rewrites
//! the golden files under `tests/golden/json/`.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use support::*;

const WEIRD: &str = "q\"t\tn\\\u{1}.txt";

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/json")
}

/// Compare `got` (one document line) to `tests/golden/json/<name>.json`,
/// after replacing the repository root by `<root>`.
fn golden(root: &Path, name: &str, got: &str) {
    let mut g = got.to_string();
    let canon = fs::canonicalize(root).unwrap();
    for r in [canon.to_string_lossy(), root.to_string_lossy()] {
        g = g.replace(r.as_ref(), "<root>");
    }
    let p = golden_dir().join(format!("{name}.json"));
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        fs::create_dir_all(golden_dir()).unwrap();
        fs::write(&p, &g).unwrap();
        return;
    }
    let want = fs::read_to_string(&p).unwrap_or_else(|_| panic!("missing golden {}", p.display()));
    assert_eq!(g, want, "golden {name} differs");
}

fn raw(dir: &Path, args: &[&str]) -> (String, String, Option<i32>) {
    let mut a = args.to_vec();
    a.push("--json");
    let o = hub(dir, &a);
    (
        String::from_utf8(o.stdout).unwrap(),
        String::from_utf8(o.stderr).unwrap(),
        o.status.code(),
    )
}

/// The fixed repository: two checkpoints, odd file names, unrecorded
/// changes, an ignored file.
fn fixture() -> PathBuf {
    let d = tmp("golden");
    ok(&d, &["init"]);
    set_replica(&d, 0xabc);
    fs::create_dir_all(d.join("d")).unwrap();
    fs::write(d.join("a.txt"), b"hello\n").unwrap();
    fs::write(d.join("d/b.txt"), b"b\n").unwrap();
    fs::write(d.join("sp é한.txt"), "유니코드\n").unwrap();
    fs::write(d.join(WEIRD), b"\x00\xff\n").unwrap();
    ok(&d, &["record", "-m", "first \"quoted\" é"]);
    fs::write(d.join("a.txt"), b"hello again\n").unwrap();
    ok(&d, &["record", "-m", "second"]);
    fs::write(d.join("new.txt"), b"n\n").unwrap();
    fs::write(d.join("d/b.txt"), b"b2\n").unwrap();
    fs::write(d.join(".DS_Store"), b"x").unwrap();
    d
}

fn first_short(d: &Path) -> String {
    let log = js(d, &["log"]);
    log.get("checkpoints").arr()[1]
        .get("version")
        .get("short")
        .str()
        .to_string()
}

#[test]
fn golden_documents() {
    let d = fixture();
    let v1 = first_short(&d);
    let blob_a = js(&d, &["ls"]).get("entries").arr()[0]
        .get("blob")
        .str()
        .to_string();
    let docs: Vec<(&str, Vec<&str>)> = vec![
        ("status", vec!["status"]),
        ("status-ignored", vec!["status", "--ignored"]),
        ("ls", vec!["ls"]),
        ("ls-l", vec!["ls", "-l"]),
        ("ls-d", vec!["ls", "d"]),
        ("show-first", vec!["show", &v1]),
        ("log", vec!["log"]),
        ("log-path", vec!["log", "--path", "a.txt"]),
        ("log-path-weird", vec!["log", "--path", WEIRD]),
        ("warnings", vec!["warnings"]),
        ("cat-first", vec!["cat", &v1, "a.txt"]),
        ("cat-weird", vec!["cat", &v1, WEIRD]),
        ("cat-blob", vec!["cat-blob", &blob_a]),
    ];
    for (name, args) in &docs {
        let (out, err, code) = raw(&d, args);
        assert_eq!(code, Some(0), "{name}: {err}");
        assert!(err.is_empty(), "{name}: stderr {err}");
        let v = one_doc(&out);
        assert_eq!(v.keys()[..2], ["schema", "command"], "{name}");
        assert_eq!(v.keys().last(), Some(&"warnings"), "{name}");
        golden(&d, name, &out);
    }
    // Every escaped name round-trips through the parser.
    let ls = js(&d, &["ls"]);
    let names: Vec<&str> = ls
        .get("entries")
        .arr()
        .iter()
        .map(|e| e.get("path").str())
        .collect();
    assert!(
        names.contains(&WEIRD) && names.contains(&"sp é한.txt"),
        "{names:?}"
    );
    // Content is base64 of the exact bytes.
    let c = js(&d, &["cat", &v1, WEIRD]);
    assert_eq!(c.get("content").str(), "AP8K");
    let c = js(&d, &["cat", &v1, "a.txt"]);
    assert_eq!(c.get("content").str(), "aGVsbG8K");
    // Raw `cat` / `cat-blob` (no --json) write the bytes.
    assert_eq!(hub(&d, &["cat", &v1, WEIRD]).stdout, b"\x00\xff\n");
    assert_eq!(hub(&d, &["cat-blob", &blob_a]).stdout, b"hello again\n");

    // Errors: JSON on stderr, nothing on stdout, exit 1.
    let errs: Vec<(&str, Vec<&str>, &str)> = vec![
        ("err-unknown-version", vec!["show", "ffff"], "bad-version"),
        ("err-bad-version", vec!["cat", "zz", "a.txt"], "bad-version"),
        ("err-no-such-path", vec!["cat", &v1, "nope"], "no-such-path"),
        ("err-not-a-file", vec!["cat", &v1, "d"], "not-a-file"),
        ("err-bad-blob", vec!["cat-blob", "123"], "bad-blob-id"),
        (
            "err-no-such-blob",
            vec![
                "cat-blob",
                "0000000000000000000000000000000000000000000000000000000000000001",
            ],
            "no-such-blob",
        ),
        ("err-usage", vec!["ls", "a", "b"], "usage"),
        ("err-unknown-command", vec!["frob"], "usage"),
        (
            "err-log-path",
            vec!["log", "--path", "nope"],
            "no-such-path",
        ),
    ];
    for (name, args, code) in &errs {
        let (out, err, st) = raw(&d, args);
        assert_eq!(st, Some(1), "{name}");
        assert!(out.is_empty(), "{name}: stdout {out}");
        let v = one_doc(&err);
        assert_eq!(v.get("error").get("code").str(), *code, "{name}: {err}");
        golden(&d, name, &err);
    }
    let e = tmp("norepo");
    let v = js_err(&e, &["status"]);
    assert_eq!(v.get("error").get("code").str(), "not-a-repository");
}

/// Human output keeps its format; the error prefix is `barocss-hub: `.
#[test]
fn human_errors_use_the_program_prefix() {
    let d = tmp("prefix");
    let o = hub(&d, &["status"]);
    assert_eq!(o.status.code(), Some(1));
    let e = String::from_utf8(o.stderr).unwrap();
    assert!(e.starts_with("barocss-hub: not a hub repository"), "{e}");
    assert!(o.stdout.is_empty());
}

/// Writers and every other command: one document on stdout, never text.
#[test]
fn every_command_emits_one_document() {
    let d = tmp("writers");
    let init = js(&d, &["init"]);
    assert_eq!(init.get("command").str(), "init");
    assert!(init.get("output").arr()[0]
        .str()
        .starts_with("initialized hub repository"));
    set_replica(&d, 0xdef);
    fs::write(d.join("f.txt"), b"1").unwrap();
    let r = js(&d, &["record", "-m", "c1"]);
    assert_eq!(r.get("recorded"), &V::Bool(true));
    assert_eq!(r.get("checkpoint").get("message").str(), "c1");
    let node = r.get("changes").arr()[0].get("node").str().to_string();
    assert_eq!(node, format!("2.{:032x}", 0xdef));
    assert_eq!(js(&d, &["record"]).get("recorded"), &V::Bool(false));
    let c = js(&d, &["commit", "-m", "c2"]);
    assert_eq!(c.get("checkpoint").get("message").str(), "c2");
    for args in [
        vec!["mkdir", "dd"],
        vec!["touch", "dd/e"],
        vec!["mv", "dd/e", "dd/g"],
        vec!["rm", "dd/g"],
        vec!["publish", "c1"],
        vec!["peer", "list"],
        vec!["reid"],
        vec!["help"],
    ] {
        let v = js(&d, &args);
        assert_eq!(v.get("command").str(), args[0], "{args:?}");
    }
    assert_eq!(js(&d, &["--version"]).get("command").str(), "version");
    let h = js(&d, &["help"]);
    assert_eq!(h.get("text").str(), hub_cli::HELP);
    let co = js(&d, &["checkout"]);
    assert_eq!(co.get("at_head"), &V::Bool(true));
    // Warnings are folded into the document, never on stderr.
    let pub_log = js(&d, &["log"]);
    assert_eq!(
        pub_log.get("checkpoints").arr()[1].get("published"),
        &V::Bool(true)
    );
    fs::write(d.join(".hub/snapshot"), b"garbage").unwrap();
    let st = js(&d, &["status"]);
    let w = st.get("warnings").arr();
    assert!(
        w.iter().any(|x| x.str().contains("snapshot")),
        "snapshot warning folded in: {w:?}"
    );
}

/// What the VS Code extension (#38) reads: behind/pinned state, a running
/// watcher's pid, content conflicts with both blob ids, per-file history.
#[test]
fn sync_state_pin_watch_and_conflicts() {
    let a = tmp("a");
    let b = tmp("b");
    ok(&a, &["init"]);
    ok(&b, &["init"]);
    fs::write(a.join("f.txt"), b"base\n").unwrap();
    ok(&a, &["record", "-m", "base"]);
    ok(&b, &["sync", a.to_str().unwrap()]);
    let st = js(&b, &["status"]);
    let wdir = st.get("workdir");
    assert_eq!(wdir.get("state").str(), "behind");
    assert!(matches!(wdir.get("behind_ops"), V::Num(n) if *n >= 2.0));
    ok(&b, &["checkout"]);
    assert_eq!(
        js(&b, &["status"]).get("workdir").get("state").str(),
        "at-head"
    );
    ok(&b, &["commit", "-m", "early"]);

    // Concurrent edits of the same file: a content conflict.
    fs::write(a.join("f.txt"), b"from a\n").unwrap();
    ok(&a, &["record"]);
    fs::write(b.join("f.txt"), b"from b\n").unwrap();
    ok(&b, &["record"]);
    ok(&b, &["sync", a.to_str().unwrap()]);
    let st = js(&b, &["status"]);
    let cs = st.get("conflicts").arr();
    assert_eq!(cs.len(), 1, "{cs:?}");
    let c = &cs[0];
    assert_eq!(c.get("kind").str(), "content");
    assert_eq!(c.get("path").str(), "f.txt");
    let blobs: Vec<Vec<u8>> = ["winner_blob", "loser_blob"]
        .iter()
        .map(|k| hub(&b, &["cat-blob", c.get(k).str()]).stdout)
        .collect();
    let mut sorted = blobs.clone();
    sorted.sort();
    assert_eq!(sorted, [b"from a\n".to_vec(), b"from b\n".to_vec()]);
    assert_ne!(c.get("winner").str(), c.get("loser").str());
    // The file's history lists both concurrent edits.
    let h = js(&b, &["log", "--path", "f.txt"]);
    let edits = h
        .get("ops")
        .arr()
        .iter()
        .filter(|o| o.get("kind").str() == "set-content")
        .count();
    assert_eq!(edits, 3);
    assert_eq!(h.get("checkpoints").arr().len(), 1);

    // Pinned.
    let v = js(&b, &["log"]).get("checkpoints").arr()[0]
        .get("version")
        .get("short")
        .str()
        .to_string();
    ok(&b, &["checkout", &v]);
    let st = js(&b, &["status"]);
    assert_eq!(st.get("workdir").get("state").str(), "pinned");
    assert_eq!(
        st.get("workdir").get("pinned").get("short").str(),
        v.as_str()
    );
    ok(&b, &["checkout"]);

    // Watch running with its pid.
    assert_eq!(
        js(&b, &["status"]).get("watch").get("running"),
        &V::Bool(false)
    );
    let w = Watcher::start(&b, 200, &[]);
    let pid = w.child.as_ref().unwrap().id() as f64;
    let st = js(&b, &["status"]);
    assert_eq!(st.get("watch").get("running"), &V::Bool(true));
    assert_eq!(st.get("watch").get("pid"), &V::Num(pid));
    let started = &w.of("started")[0];
    assert_eq!(started.get("pid"), &V::Num(pid));
    let evs = w.stop();
    assert_eq!(evs.last().unwrap().get("event").str(), "stopped");
    assert_eq!(
        js(&b, &["status"]).get("watch").get("running"),
        &V::Bool(false)
    );
}

/// Watch events from a peer's ops: `synced` and `checked-out`.
#[test]
fn watch_events_for_incoming_ops() {
    let a = tmp("wa");
    let b = tmp("wb");
    ok(&a, &["init"]);
    ok(&b, &["init"]);
    ok(&b, &["peer", "add", a.to_str().unwrap()]);
    let w = Watcher::start(&b, 200, &[]);
    fs::write(a.join("n.txt"), b"x\n").unwrap();
    ok(&a, &["record"]);
    wait_for("checked-out", || !w.of("checked-out").is_empty());
    let s = &w.of("synced")[0];
    assert_eq!(s.get("received").get("ops"), &V::Num(2.0));
    assert_eq!(s.get("received").get("blobs"), &V::Num(1.0));
    let c = &w.of("checked-out")[0];
    assert_eq!(c.get("reason").str(), "incoming");
    assert_eq!(c.get("written"), &V::Num(1.0));
    w.stop();
    assert_eq!(fs::read(b.join("n.txt")).unwrap(), b"x\n");
}
