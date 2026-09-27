//! Issue #22: `hub sync <path>` between local repos, real `hub` binary.
//! Every test runs in its own temp dir.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hub-sync-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

fn hub_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_hub"));
    c.args(args).current_dir(dir);
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let o = hub_env(dir, args, &[]);
    assert!(
        o.status.success(),
        "hub {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

fn fail(dir: &Path, args: &[&str]) -> String {
    let o = hub_env(dir, args, &[]);
    assert!(!o.status.success(), "hub {args:?} unexpectedly succeeded");
    String::from_utf8(o.stderr).unwrap()
}

fn snapshot(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(dir: &Path, rel: &str, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            if rel.is_empty() && name == ".hub" {
                continue;
            }
            let p = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if e.file_type().unwrap().is_dir() {
                out.insert(p.clone(), None);
                walk(&e.path(), &p, out);
            } else {
                out.insert(p, Some(fs::read(e.path()).unwrap()));
            }
        }
    }
    let mut m = BTreeMap::new();
    walk(root, "", &mut m);
    m
}

/// Repos `names` under a fresh temp dir, each `hub init`ed.
fn repos(tag: &str, names: &[&str]) -> (PathBuf, Vec<PathBuf>) {
    let t = tmp(tag);
    let v = names
        .iter()
        .map(|n| {
            let d = t.join(n);
            fs::create_dir_all(&d).unwrap();
            ok(&d, &["init"]);
            d
        })
        .collect();
    (t, v)
}

fn head(d: &Path) -> String {
    ok(d, &["status"])
        .lines()
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .to_string()
}

fn published(d: &Path) -> BTreeSet<String> {
    ok(d, &["log"])
        .lines()
        .filter(|l| l.contains(" [published] "))
        .map(str::to_string)
        .collect()
}

fn conflicts(d: &Path) -> String {
    let st = ok(d, &["status"]);
    st.lines()
        .skip_while(|l| !l.starts_with("conflicts"))
        .take_while(|l| l.starts_with("conflicts") || l.starts_with("  "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn sync(a: &Path, b: &Path) -> String {
    ok(a, &["sync", b.to_str().unwrap()])
}

#[test]
fn clone_by_sync_edit_both_converge() {
    let (_t, r) = repos("clone", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    fs::create_dir_all(a.join("src")).unwrap();
    fs::write(a.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(a.join("README"), "hi\n").unwrap();
    ok(a, &["record", "-m", "base"]);
    ok(a, &["publish", "base"]);
    let s = sync(b, a);
    assert!(
        s.contains("received 5 op(s), 2 blob(s), 1 checkpoint(s)"),
        "{s}"
    );
    let st = ok(b, &["status"]);
    assert!(st.contains("behind head by 5 op(s)"), "{st}");
    ok(b, &["checkout"]);
    assert_eq!(snapshot(a), snapshot(b));
    // Edit and record in both.
    fs::write(a.join("a.txt"), "from a\n").unwrap();
    fs::write(a.join("README"), "hi from a\n").unwrap();
    ok(a, &["record", "-m", "a work"]);
    ok(a, &["publish", "a work"]);
    fs::write(b.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    ok(b, &["record", "-m", "b work"]);
    ok(b, &["publish", "b work"]);
    sync(a, b);
    ok(a, &["checkout"]);
    ok(b, &["checkout"]);
    assert_eq!(snapshot(a), snapshot(b));
    assert_eq!(fs::read(b.join("README")).unwrap(), b"hi from a\n");
    assert_eq!(head(a), head(b));
    assert_eq!(published(a), published(b));
    assert_eq!(published(a).len(), 3);
    assert!(ok(a, &["status"]).contains("working directory: clean"));
}

#[test]
fn concurrent_edit_same_file_same_conflict_and_winner() {
    let (_t, r) = repos("content", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    fs::write(a.join("f.txt"), "base\n").unwrap();
    ok(a, &["record"]);
    sync(b, a);
    ok(b, &["checkout"]);
    fs::write(a.join("f.txt"), "A edit\n").unwrap();
    ok(a, &["record"]);
    fs::write(b.join("f.txt"), "B edit\n").unwrap();
    ok(b, &["record"]);
    sync(a, b);
    ok(a, &["checkout"]);
    ok(b, &["checkout"]);
    let (ca, cb) = (conflicts(a), conflicts(b));
    assert!(ca.contains("content: f.txt"), "{ca}");
    assert_eq!(ca, cb);
    let wa = fs::read(a.join("f.txt")).unwrap();
    assert_eq!(wa, fs::read(b.join("f.txt")).unwrap());
    assert!(wa == b"A edit\n" || wa == b"B edit\n");
    assert_eq!(head(a), head(b));
}

#[test]
fn concurrent_rename_vs_delete_converges() {
    let (_t, r) = repos("rename-delete", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    fs::create_dir_all(a.join("d")).unwrap();
    fs::write(a.join("d/f.txt"), "x\n").unwrap();
    ok(a, &["record"]);
    sync(b, a);
    ok(b, &["checkout"]);
    ok(a, &["mv", "d/f.txt", "g.txt"]);
    ok(b, &["rm", "d"]);
    sync(a, b);
    ok(a, &["checkout"]);
    ok(b, &["checkout"]);
    assert_eq!(snapshot(a), snapshot(b));
    assert_eq!(conflicts(a), conflicts(b));
    assert_eq!(head(a), head(b));
    // D-001: the concurrent move out of the deleted dir keeps the file live.
    assert_eq!(fs::read(a.join("g.txt")).unwrap(), b"x\n");
    assert!(!a.join("d").exists());
}

#[test]
fn idempotent_and_order_independent_three_repos() {
    let build = |tag: &str| {
        let (t, r) = repos(tag, &["a", "b", "c"]);
        for (i, d) in r.iter().enumerate() {
            fs::write(d.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
            fs::write(d.join("shared.txt"), format!("by {i}\n")).unwrap();
            ok(d, &["record", "-m", &format!("r{i}")]);
            ok(d, &["publish", &format!("r{i}")]);
        }
        (t, r)
    };
    // One pre-sync state, copied byte-for-byte into a second world, so the
    // two schedules start from identical repos (same replica ids).
    let (t1, r1) = build("order");
    let t2 = t1.with_file_name(format!(
        "{}-copy",
        t1.file_name().unwrap().to_str().unwrap()
    ));
    assert!(Command::new("cp")
        .args(["-R", t1.to_str().unwrap(), t2.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    let r2: Vec<PathBuf> = ["a", "b", "c"].iter().map(|n| t2.join(n)).collect();
    // Order 1: A-B, B-C, A-C, A-B.
    sync(&r1[0], &r1[1]);
    sync(&r1[1], &r1[2]);
    sync(&r1[0], &r1[2]);
    sync(&r1[0], &r1[1]);
    // Order 2: A-C, B-C, A-B, A-C.
    sync(&r2[0], &r2[2]);
    sync(&r2[1], &r2[2]);
    sync(&r2[0], &r2[1]);
    sync(&r2[0], &r2[2]);
    assert_eq!(head(&r1[0]), head(&r2[0]));
    for r in [&r1, &r2] {
        let h = head(&r[0]);
        for d in r.iter() {
            assert_eq!(head(d), h);
            ok(d, &["checkout"]);
        }
        assert_eq!(snapshot(&r[0]), snapshot(&r[1]));
        assert_eq!(snapshot(&r[0]), snapshot(&r[2]));
        assert_eq!(published(&r[0]), published(&r[2]));
        // A second sync is a no-op.
        let s = sync(&r[0], &r[1]);
        assert!(
            s.contains("received 0 op(s), 0 blob(s), 0 checkpoint(s); sent 0 op(s), 0 blob(s), 0 checkpoint(s)"),
            "{s}"
        );
        assert_eq!(head(&r[0]), h);
    }
}

#[test]
fn interrupted_sync_is_valid_and_rerun_completes() {
    let (_t, r) = repos("interrupt", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    for i in 0..6 {
        fs::write(a.join(format!("f{i}")), format!("{i}\n")).unwrap();
    }
    ok(a, &["record", "-m", "a"]);
    ok(a, &["publish", "a"]);
    fs::write(b.join("b"), "b\n").unwrap();
    ok(b, &["record"]);
    let o = hub_env(
        b,
        &["sync", a.to_str().unwrap()],
        &[("HUB_TEST_CRASH_IN_SYNC", "5")],
    );
    assert!(!o.status.success(), "crash hook did not fire");
    // Both repos open and are valid.
    ok(a, &["status"]);
    ok(b, &["status"]);
    let partial = ok(b, &["status"]);
    // b had 2 ops and got exactly 5 of a's 12 before the crash.
    assert!(partial.contains("(7 ops"), "{partial}");
    let s = sync(b, a);
    assert!(s.contains("received 7 op(s)"), "{s}");
    ok(a, &["checkout"]);
    ok(b, &["checkout"]);
    assert_eq!(snapshot(a), snapshot(b));
    assert_eq!(head(a), head(b));
    assert_eq!(published(a), published(b));
}

#[test]
fn copied_hub_is_duplicate_replica_then_reid_allows_sync() {
    let t = tmp("dup");
    let a = t.join("a");
    let b = t.join("b");
    fs::create_dir_all(&a).unwrap();
    ok(&a, &["init"]);
    fs::write(a.join("f"), "f\n").unwrap();
    ok(&a, &["record"]);
    let st = Command::new("cp")
        .args(["-R", a.to_str().unwrap(), b.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(st.success());
    let e = fail(&b, &["sync", "../a"]);
    assert!(
        e.contains("duplicate replica id") && e.contains("hub reid"),
        "{e}"
    );
    let w = ok(&a, &["warnings"]);
    assert!(w.contains("duplicate-replica"), "{w}");
    assert!(ok(&b, &["status"]).contains("duplicate-replica"));
    let rid = ok(&b, &["reid"]);
    assert!(rid.contains(" -> "), "{rid}");
    fs::write(b.join("g"), "g\n").unwrap();
    ok(&b, &["record"]);
    let s = sync(&b, &a);
    assert!(s.contains("sent 2 op(s)"), "{s}");
    ok(&a, &["checkout"]);
    assert_eq!(snapshot(&a), snapshot(&b));
}

#[test]
fn unpublished_checkpoint_never_reaches_peer() {
    let (_t, r) = repos("publish", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    fs::write(a.join("f"), "1\n").unwrap();
    ok(a, &["record", "-m", "private one"]);
    fs::write(a.join("f"), "2\n").unwrap();
    ok(a, &["record", "-m", "public one"]);
    let p = ok(a, &["publish", "public one"]);
    assert!(p.starts_with("published ["), "{p}");
    assert!(ok(a, &["publish", "public one"]).starts_with("already published"));
    assert!(fail(a, &["publish", "nope"]).contains("no local checkpoint"));
    sync(a, b);
    let lb = ok(b, &["log"]);
    assert!(lb.contains("[published] public one"), "{lb}");
    assert!(!lb.contains("private one"), "{lb}");
    let la = ok(a, &["log"]);
    assert!(la.contains("[published] public one") && la.contains(" private one"));
    // Relay: C gets it from B, still without the private one.
    let c = b.parent().unwrap().join("c");
    fs::create_dir_all(&c).unwrap();
    ok(&c, &["init"]);
    sync(&c, b);
    let lc = ok(&c, &["log"]);
    assert!(
        lc.contains("public one") && !lc.contains("private one"),
        "{lc}"
    );
}

#[test]
fn opposite_concurrent_syncs_do_not_deadlock() {
    let (_t, r) = repos("deadlock", &["a", "b"]);
    let (a, b) = (r[0].clone(), r[1].clone());
    fs::write(a.join("x"), "x\n").unwrap();
    ok(&a, &["record"]);
    fs::write(b.join("y"), "y\n").unwrap();
    ok(&b, &["record"]);
    let hs: Vec<_> = (0..4)
        .map(|i| {
            let (from, to) = if i % 2 == 0 {
                (a.clone(), b.clone())
            } else {
                (b.clone(), a.clone())
            };
            std::thread::spawn(move || {
                Command::new(env!("CARGO_BIN_EXE_hub"))
                    .args(["sync", to.to_str().unwrap()])
                    .current_dir(&from)
                    .output()
                    .unwrap()
            })
        })
        .collect();
    for h in hs {
        assert!(h.join().unwrap().status.success());
    }
    assert_eq!(head(&a), head(&b));
}

#[test]
fn sync_leaves_unrecorded_changes_alone() {
    let (_t, r) = repos("dirty", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    fs::write(a.join("f"), "f\n").unwrap();
    ok(a, &["record"]);
    fs::write(b.join("mine"), "unrecorded\n").unwrap();
    sync(b, a);
    let st = ok(b, &["status"]);
    assert!(st.contains("behind head by 2 op(s)"), "{st}");
    assert!(st.contains("added:    mine") || st.contains("mine"), "{st}");
    assert_eq!(fs::read(b.join("mine")).unwrap(), b"unrecorded\n");
    assert!(!b.join("f").exists());
    assert!(fail(b, &["sync", "."]).contains("itself"));
}

fn op_bodies(d: &Path) -> Vec<String> {
    fs::read_to_string(d.join(".hub/ops.log"))
        .unwrap()
        .lines()
        .skip(1)
        .map(|l| l.rsplit_once('\t').unwrap().0.to_string())
        .collect()
}

fn write_pending(d: &Path, peer: &str, bodies: &[String]) {
    let mut s = String::from("barocss-hub/pending/v0\n");
    for b in bodies {
        s.push_str(&hub_cli::store::frame(&format!("P {peer} {b}")));
    }
    fs::write(d.join(".hub/pending.log"), s).unwrap();
}

#[test]
fn pending_ops_persist_and_retry_warnings_name_peer_and_stuck() {
    let (_t, r) = repos("pending", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    fs::write(a.join("f"), "f\n").unwrap();
    ok(a, &["record"]);
    let ops = op_bodies(a); // [Create f, SetContent f]
    let peer = "0000000000000000000000000000abcd";
    // b holds a's SetContent without its Create dep (and without its blob).
    write_pending(b, peer, &ops[1..]);
    let st = ok(b, &["status"]);
    assert!(st.contains("pending ops: 1"), "{st}");
    assert!(
        ok(b, &["warnings"]).contains("(missing deps)"),
        "pending listed"
    );
    sync(b, a);
    let st = ok(b, &["status"]);
    assert!(!st.contains("pending ops"), "{st}");
    ok(b, &["checkout"]);
    assert_eq!(fs::read(b.join("f")).unwrap(), b"f\n");

    // A Lamport-violating op (dep not < op) plus a descendant: rejected with
    // a D-008 warning naming the peer, the descendant listed as stuck.
    let rid = ops[0].split(' ').nth(1).unwrap().split('.').nth(1).unwrap();
    let bad = format!("O 1.{peer} 2.{rid} D 1.{rid}");
    let child = format!("O 9.{peer} 1.{peer} D 1.{rid}");
    write_pending(b, peer, &[bad, child]);
    sync(b, a);
    let w = ok(b, &["warnings"]);
    assert!(
        w.contains(&format!("lamport-violation from peer {}", &peer[..8]))
            && w.contains(&format!("stuck: 9.{peer}")),
        "{w}"
    );
    assert!(ok(b, &["status"]).contains("sync warnings:"));
    assert_eq!(head(a), head(b));
}

#[test]
fn corrupt_local_blob_refuses_sync() {
    let (_t, r) = repos("corrupt", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    fs::write(a.join("f"), "hello\n").unwrap();
    ok(a, &["record"]);
    let blobs = a.join(".hub/blobs");
    let e = fs::read_dir(&blobs).unwrap().next().unwrap().unwrap();
    fs::write(e.path(), "tampered").unwrap();
    let err = fail(b, &["sync", a.to_str().unwrap()]);
    assert!(err.contains("corrupt blob"), "{err}");
    ok(b, &["status"]);
}

#[test]
fn edit_vs_delete_revives_after_sync_d014() {
    let (_t, r) = repos("revive", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    fs::write(a.join("f.txt"), "v1\n").unwrap();
    ok(a, &["record"]);
    sync(b, a);
    ok(b, &["checkout"]);
    fs::write(a.join("f.txt"), "v2 edited\n").unwrap();
    ok(a, &["record"]);
    ok(b, &["rm", "f.txt"]);
    sync(b, a);
    ok(a, &["checkout"]);
    ok(b, &["checkout"]);
    assert_eq!(snapshot(a), snapshot(b));
    assert_eq!(fs::read(b.join("f.txt")).unwrap(), b"v2 edited\n");
    let c = conflicts(b);
    assert!(c.contains("edit-revived: "), "{c}");
    assert_eq!(c, conflicts(a));
}

fn fs_case_insensitive(d: &Path) -> bool {
    fs::write(d.join("probe"), "").unwrap();
    let ci = d.join("PROBE").exists();
    fs::remove_file(d.join("probe")).unwrap();
    ci
}

#[test]
fn case_clash_from_sync_is_unmaterialized_and_blocks_record_d019() {
    let (t, r) = repos("clash", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    if !fs_case_insensitive(&t) {
        eprintln!("skipped: case-sensitive filesystem");
        return;
    }
    fs::write(a.join("Readme"), "from a\n").unwrap();
    ok(a, &["record"]);
    fs::write(b.join("README"), "from b\n").unwrap();
    ok(b, &["record"]);
    // A sync warning to show beside the blocked path.
    let ops = op_bodies(a);
    let rid = ops[0].split(' ').nth(1).unwrap().split('.').nth(1).unwrap();
    let peer = "0000000000000000000000000000abcd";
    write_pending(b, peer, &[format!("O 1.{peer} 2.{rid} D 1.{rid}")]);
    sync(b, a);
    let co = ok(b, &["checkout"]);
    assert!(co.contains("checked out"), "{co}");
    let st = ok(b, &["status"]);
    assert!(st.contains("unmaterialized"), "{st}");
    assert!(st.contains("sync warnings:"), "{st}");
    // D-019: neither clashing name is written.
    let names: Vec<String> = fs::read_dir(b)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n != ".hub")
        .collect();
    assert!(names.is_empty(), "{names:?}");
    // A working file on the unmaterialized path is blocked; record refuses.
    let unmat = st
        .lines()
        .skip_while(|l| !l.starts_with("unmaterialized"))
        .nth(1)
        .unwrap()
        .trim()
        .to_string();
    fs::write(b.join(&unmat), "in the way\n").unwrap();
    let st = ok(b, &["status"]);
    assert!(
        st.contains("blocked") && st.contains("sync warnings:"),
        "{st}"
    );
    assert!(fail(b, &["record"]).contains("D-019"));
    // The other repo converges to the same head.
    ok(a, &["checkout"]);
    assert_eq!(head(a), head(b));
}
