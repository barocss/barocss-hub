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
    let mut c = Command::new(env!("CARGO_BIN_EXE_barocss-hub"));
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
    // Format v1 (#33): the crash hit before the batch's commit record, so
    // none of the 5 written ops count; b still has its own 2.
    assert!(partial.contains("(2 ops"), "{partial}");
    let s = sync(b, a);
    assert!(s.contains("received 12 op(s)"), "{s}");
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
        e.contains("duplicate replica id") && e.contains("barocss-hub reid"),
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

/// #23 PROVISIONAL default (`hub_cli::sync::REID_CLEARS_DUPLICATE_WARNINGS`,
/// pending user confirmation): a successful `hub reid` clears this repo's
/// saved duplicate-replica warnings; the other copy keeps its own.
#[test]
fn reid_clears_duplicate_replica_warnings() {
    let t = tmp("dup-clear");
    let a = t.join("a");
    let b = t.join("b");
    fs::create_dir_all(&a).unwrap();
    ok(&a, &["init"]);
    let st = Command::new("cp")
        .args(["-R", a.to_str().unwrap(), b.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(st.success());
    fail(&b, &["sync", "../a"]);
    assert!(ok(&b, &["warnings"]).contains("duplicate-replica"));
    let rid = ok(&b, &["reid"]);
    assert_eq!(
        rid.contains("cleared 1 duplicate-replica warning(s)"),
        hub_cli::sync::REID_CLEARS_DUPLICATE_WARNINGS,
        "{rid}"
    );
    assert_eq!(
        !ok(&b, &["warnings"]).contains("duplicate-replica"),
        hub_cli::sync::REID_CLEARS_DUPLICATE_WARNINGS
    );
    assert!(ok(&a, &["warnings"]).contains("duplicate-replica"));
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
                Command::new(env!("CARGO_BIN_EXE_barocss-hub"))
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
        .filter(|l| l.starts_with("O ")) // v1 (#33): skip commit records
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

/// Issue #26 setup: a and b each create `f1` with the same bytes and sync,
/// so both hold `f1` and `f1~<id>` (a name collision). Returns the paths.
fn collided(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let (t, r) = repos(tag, &["a", "b"]);
    let (a, b) = (r[0].clone(), r[1].clone());
    fs::write(a.join("f1"), "same\n").unwrap();
    ok(&a, &["record"]);
    fs::write(b.join("f1"), "same\n").unwrap();
    ok(&b, &["record"]);
    sync(&b, &a);
    ok(&a, &["checkout"]);
    ok(&b, &["checkout"]);
    assert_eq!(snapshot(&a), snapshot(&b));
    assert_eq!(snapshot(&a).len(), 2, "{:?}", snapshot(&a).keys());
    (t, a, b)
}

fn names(d: &Path) -> Vec<String> {
    snapshot(d).into_keys().collect()
}

/// Issue #26 hand repro: `rm f1; hub record` deletes the collision winner,
/// so the other node's projected name becomes `f1`. record must rename it
/// on disk and index it, or the replicas diverge after sync.
#[test]
fn record_materializes_rename_when_collision_disappears() {
    let (_t, a, b) = collided("i26-rec");
    fs::remove_file(a.join("f1")).unwrap();
    ok(&a, &["record"]);
    assert_eq!(names(&a), vec!["f1".to_string()]);
    assert_eq!(fs::read(a.join("f1")).unwrap(), b"same\n");
    assert_eq!(ok(&a, &["record"]).trim(), "nothing to record");
    let st = ok(&a, &["status"]);
    assert!(!st.contains("deleted") && !st.contains("added"), "{st}");
    sync(&a, &b);
    ok(&a, &["checkout"]);
    ok(&b, &["checkout"]);
    assert_eq!(snapshot(&a), snapshot(&b));
    assert_eq!(head(&a), head(&b));
    assert_eq!(ok(&a, &["record"]).trim(), "nothing to record");
    assert_eq!(ok(&b, &["record"]).trim(), "nothing to record");
}

/// Same, through the mutating command `hub rm`.
#[test]
fn rm_materializes_rename_when_collision_disappears() {
    let (_t, a, b) = collided("i26-rm");
    ok(&a, &["rm", "f1"]);
    assert_eq!(names(&a), vec!["f1".to_string()]);
    assert_eq!(ok(&a, &["record"]).trim(), "nothing to record");
    sync(&a, &b);
    ok(&b, &["checkout"]);
    assert_eq!(snapshot(&a), snapshot(&b));
}

/// Inverse (Issue #26): a collision *appears* (here through sync +
/// checkout, the only way two live siblings can get one name; see the
/// Issue comment), so an existing clean file gains a suffix on disk. A
/// later record in either repo must see nothing to record.
#[test]
fn checkout_suffixes_existing_file_when_collision_appears() {
    let (_t, r) = repos("i26-inv", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    fs::write(a.join("f1"), "from a\n").unwrap();
    ok(a, &["record"]);
    fs::write(b.join("f1"), "from b\n").unwrap();
    ok(b, &["record"]);
    sync(a, b);
    ok(a, &["checkout"]);
    ok(b, &["checkout"]);
    assert_eq!(snapshot(a), snapshot(b));
    let n = names(a);
    assert_eq!(n.len(), 2, "{n:?}");
    assert!(n[0] == "f1" && n[1].starts_with("f1~"), "{n:?}");
    assert_eq!(ok(a, &["record"]).trim(), "nothing to record");
    assert_eq!(ok(b, &["record"]).trim(), "nothing to record");
    // And back: deleting the suffixed loser keeps the winner at `f1`;
    // deleting the winner instead (in b) moves the loser to `f1`.
    fs::remove_file(b.join("f1")).unwrap();
    ok(b, &["record"]);
    assert_eq!(names(b), vec!["f1".to_string()]);
    sync(a, b);
    ok(a, &["checkout"]);
    assert_eq!(snapshot(a), snapshot(b));
    assert_eq!(ok(a, &["record"]).trim(), "nothing to record");
}

/// Inverse through record (Issue #26): in one record, delete the winner
/// `f1` and create a directory `f1/`. The new directory node joins the
/// loser's collision group with a larger id, so the loser (older) takes
/// `f1` and the user's new directory gains a suffix: record must move it on
/// disk (with its contents) and write the loser at `f1`.
#[test]
fn record_suffixes_new_node_when_collision_appears() {
    let (_t, a, b) = collided("i26-rinv");
    fs::remove_file(a.join("f1")).unwrap();
    fs::create_dir(a.join("f1")).unwrap();
    fs::write(a.join("f1/keep"), "mine\n").unwrap();
    ok(&a, &["record"]);
    let snap = snapshot(&a);
    assert_eq!(snap.get("f1"), Some(&Some(b"same\n".to_vec())), "{snap:?}");
    let dir = snap
        .keys()
        .find(|k| k.starts_with("f1~") && !k.contains('/'))
        .cloned()
        .unwrap_or_else(|| panic!("no suffixed dir: {snap:?}"));
    assert_eq!(snap.get(&dir), Some(&None));
    assert_eq!(
        snap.get(&format!("{dir}/keep")),
        Some(&Some(b"mine\n".to_vec()))
    );
    assert_eq!(snap.len(), 3, "{snap:?}");
    assert_eq!(ok(&a, &["record"]).trim(), "nothing to record");
    sync(&a, &b);
    ok(&b, &["checkout"]);
    assert_eq!(snapshot(&a), snapshot(&b));
}

/// Issue #26 + #24: materializing the projection change would have to move
/// a directory holding an ignored untracked file; record must refuse
/// (append nothing, touch nothing) rather than delete it, and succeed once
/// the ignored file is moved away.
#[test]
fn record_refuses_rename_that_would_delete_ignored_file() {
    let (_t, a, _b) = collided("i26-ign");
    fs::write(a.join(".hubignore"), "*.o\n").unwrap();
    ok(&a, &["record"]);
    let h = head(&a);
    // As in record_suffixes_new_node_when_collision_appears, plus an
    // ignored file inside the new directory.
    fs::remove_file(a.join("f1")).unwrap();
    fs::create_dir(a.join("f1")).unwrap();
    fs::write(a.join("f1/keep"), "mine\n").unwrap();
    fs::write(a.join("f1/cache.o"), "junk\n").unwrap();
    let e = fail(&a, &["record"]);
    assert!(e.contains("refusing to record"), "{e}");
    assert_eq!(head(&a), h);
    assert_eq!(fs::read(a.join("f1/cache.o")).unwrap(), b"junk\n");
    assert_eq!(fs::read(a.join("f1/keep")).unwrap(), b"mine\n");
    fs::remove_file(a.join("f1/cache.o")).unwrap();
    ok(&a, &["record"]);
    assert_eq!(fs::read(a.join("f1")).unwrap(), b"same\n");
    assert_eq!(ok(&a, &["record"]).trim(), "nothing to record");
}

// ---------------------------------------------------------------- Issue #34

/// `hub sync` (run in `a`) with the blob-transfer and blob-barrier logs.
/// Returns (stdout, `blobs-in` lines as (side, bytes, count), barrier lines
/// as (blobs dir, n)).
#[allow(clippy::type_complexity)]
fn sync_logged(a: &Path, b: &Path) -> (String, Vec<(String, u64, usize)>, Vec<(PathBuf, usize)>) {
    let t = tmp("logs");
    let (sl, bl) = (t.join("stats"), t.join("barriers"));
    let o = hub_env(
        a,
        &["sync", b.to_str().unwrap()],
        &[
            (hub_cli::sync::STATS_LOG_ENV, sl.to_str().unwrap()),
            (hub_cli::blobs::BARRIER_LOG_ENV, bl.to_str().unwrap()),
        ],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let stats = fs::read_to_string(&sl)
        .unwrap()
        .lines()
        .map(|l| {
            let v: Vec<&str> = l.split(' ').collect();
            assert_eq!(v[0], "blobs-in");
            (
                v[1].to_string(),
                v[2].parse().unwrap(),
                v[3].parse().unwrap(),
            )
        })
        .collect();
    let bars = fs::read_to_string(&bl)
        .unwrap_or_default()
        .lines()
        .map(|l| {
            let (rest, n) = l.rsplit_once(' ').unwrap();
            let dir = rest.strip_prefix("blob-barrier ").unwrap();
            (fs::canonicalize(dir).unwrap(), n.parse().unwrap())
        })
        .collect();
    let _ = fs::remove_dir_all(t);
    (String::from_utf8(o.stdout).unwrap(), stats, bars)
}

fn blobs_dir(d: &Path) -> PathBuf {
    fs::canonicalize(d.join(".hub/blobs")).unwrap()
}

#[test]
fn already_held_blobs_transfer_zero_bytes() {
    let (_t, r) = repos("held34", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    // Same bytes recorded independently: different ops, same blobs.
    for d in [a, b] {
        for i in 0..5 {
            fs::write(d.join(format!("f{i}")), format!("shared {i}\n")).unwrap();
        }
        ok(d, &["record"]);
    }
    let (out, stats, bars) = sync_logged(b, a);
    assert!(out.contains("received 10 op(s), 0 blob(s)"), "{out}");
    assert!(out.contains("sent 10 op(s), 0 blob(s)"), "{out}");
    assert_eq!(
        stats,
        vec![("this".into(), 0, 0), ("peer".into(), 0, 0)],
        "no blob bytes may be sent to a side that holds them"
    );
    assert!(
        bars.is_empty(),
        "no blob group commit without new blobs: {bars:?}"
    );
    assert_eq!(head(a), head(b));
    // Nothing new: a second sync moves no bytes either.
    let (_, stats, bars) = sync_logged(a, b);
    assert_eq!(stats, vec![("this".into(), 0, 0), ("peer".into(), 0, 0)]);
    assert!(bars.is_empty());
}

#[test]
fn partial_blob_overlap_sends_only_missing_bytes() {
    let (_t, r) = repos("partial34", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    fs::write(a.join("x"), "same\n").unwrap();
    fs::write(a.join("y"), "only in a\n").unwrap();
    fs::write(a.join("y2"), "also only in a\n").unwrap();
    ok(a, &["record"]);
    fs::write(b.join("z"), "same\n").unwrap();
    fs::write(b.join("w"), "only in b\n").unwrap();
    ok(b, &["record"]);
    let (_, stats, bars) = sync_logged(b, a);
    let want_b = ("only in a\n".len() + "also only in a\n".len()) as u64;
    assert_eq!(
        stats,
        vec![
            ("this".into(), want_b, 2),
            ("peer".into(), "only in b\n".len() as u64, 1)
        ]
    );
    // Exactly one blob barrier per receiving side, whatever the count.
    let mut bars = bars;
    bars.sort();
    let mut want = vec![(blobs_dir(a), 1), (blobs_dir(b), 2)];
    want.sort();
    assert_eq!(bars, want);
    ok(a, &["checkout"]);
    ok(b, &["checkout"]);
    assert_eq!(snapshot(a), snapshot(b));
}

#[test]
fn many_new_blobs_one_barrier_per_sync() {
    let (_t, r) = repos("bar34", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    for i in 0..60 {
        fs::write(a.join(format!("f{i:02}")), format!("unique {i}\n")).unwrap();
    }
    ok(a, &["record"]);
    let (_, stats, bars) = sync_logged(b, a);
    assert_eq!(stats[0].2, 60);
    assert_eq!(stats[1], ("peer".into(), 0, 0));
    assert_eq!(bars, vec![(blobs_dir(b), 60)]);
    let (_, stats, bars) = sync_logged(b, a);
    assert_eq!(stats, vec![("this".into(), 0, 0), ("peer".into(), 0, 0)]);
    assert!(bars.is_empty());
    ok(b, &["checkout"]);
    assert_eq!(snapshot(a), snapshot(b));
}

/// Issue #34 direct blob writes: a crash before the blob barrier may leave
/// final-named blob files with torn bytes (simulated: every new blob file is
/// truncated or emptied). No op names them, so both repos stay valid, and
/// the rerun rewrites them.
#[test]
fn torn_direct_blob_writes_after_crash_recover() {
    let (_t, r) = repos("torn34", &["a", "b"]);
    let (a, b) = (&r[0], &r[1]);
    for i in 0..8 {
        fs::write(a.join(format!("f{i}")), format!("content number {i}\n")).unwrap();
    }
    ok(a, &["record"]);
    let before: BTreeSet<_> = fs::read_dir(b.join(".hub/blobs"))
        .map(|r| r.map(|e| e.unwrap().file_name()).collect())
        .unwrap_or_default();
    let o = hub_env(
        b,
        &["sync", a.to_str().unwrap()],
        &[("HUB_TEST_CRASH_STAGE", "blobs-written")],
    );
    assert!(!o.status.success(), "must have aborted");
    let mut n = 0;
    for e in fs::read_dir(b.join(".hub/blobs")).unwrap() {
        let e = e.unwrap();
        if !before.contains(&e.file_name()) {
            n += 1;
            let f = fs::OpenOptions::new().write(true).open(e.path()).unwrap();
            f.set_len(if n % 2 == 0 { 0 } else { 3 }).unwrap();
        }
    }
    assert_eq!(n, 8, "direct writes use final names, no temps");
    ok(b, &["status"]);
    ok(a, &["status"]);
    // A record of the same bytes must not trust the torn file: `add`
    // verifies the hash before skipping a write.
    for i in [1usize, 2] {
        let bytes = format!("content number {i}\n");
        fs::write(b.join(format!("local{i}")), &bytes).unwrap();
    }
    ok(b, &["record"]);
    for i in [1usize, 2] {
        let bytes = format!("content number {i}\n");
        let p = b
            .join(".hub/blobs")
            .join(hub_core::tree::BlobId::of(bytes.as_bytes()).hex());
        assert_eq!(
            fs::read(p).unwrap(),
            bytes.as_bytes(),
            "record reused a torn blob"
        );
    }
    // The rerun re-fetches the 6 blobs that are still torn (an unreferenced
    // file is never counted as held) and converges.
    let s = sync(b, a);
    assert!(s.contains("received 16 op(s), 6 blob(s)"), "{s}");
    for i in 0..8 {
        let bytes = format!("content number {i}\n");
        let p = b
            .join(".hub/blobs")
            .join(hub_core::tree::BlobId::of(bytes.as_bytes()).hex());
        assert_eq!(fs::read(p).unwrap(), bytes.as_bytes());
    }
    ok(b, &["checkout"]);
    ok(a, &["checkout"]);
    assert_eq!(snapshot(a), snapshot(b));
}
