//! Issue #29: group-commit durability. Real `hub` binary, crashed at every
//! group-commit stage (`HUB_TEST_CRASH_STAGE`), plus power-loss-like damage
//! to un-synced tails. Each case asserts the repo opens, a rerun converges,
//! and nothing acknowledged before the crash is lost.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use hub_cli::durable::{self, CRASH_STAGE_ENV};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hub-dur-test-{tag}-{}-{}",
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

fn run_ok(dir: &Path, args: &[&str]) -> (String, String) {
    let o = hub_env(dir, args, &[]);
    assert!(
        o.status.success(),
        "hub {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&o.stderr)
    );
    (
        String::from_utf8(o.stdout).unwrap(),
        String::from_utf8(o.stderr).unwrap(),
    )
}

fn ok(dir: &Path, args: &[&str]) -> String {
    run_ok(dir, args).0
}

fn crash(dir: &Path, args: &[&str], stage: &str) {
    let o = hub_env(dir, args, &[(CRASH_STAGE_ENV, stage)]);
    assert!(
        !o.status.success(),
        "hub {args:?} at stage {stage} did not crash: {}",
        String::from_utf8_lossy(&o.stderr)
    );
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

fn write(root: &Path, p: &str, b: &[u8]) {
    let f = root.join(p);
    fs::create_dir_all(f.parent().unwrap()).unwrap();
    fs::write(f, b).unwrap();
}

/// Fresh clone of `src` (init + sync + checkout): what `src` durably holds.
fn clone_of(src: &Path, tag: &str) -> PathBuf {
    let c = tmp(tag);
    ok(&c, &["init"]);
    ok(&c, &["sync", src.to_str().unwrap()]);
    ok(&c, &["checkout"]);
    c
}

fn ops_len(d: &Path) -> u64 {
    fs::metadata(d.join(".hub/ops.log")).unwrap().len()
}

/// A repo with one acknowledged checkpoint "base", then unrecorded edits.
fn setup(tag: &str) -> (PathBuf, BTreeMap<String, Option<Vec<u8>>>) {
    let d = tmp(tag);
    ok(&d, &["init"]);
    for i in 0..5 {
        write(&d, &format!("base/f{i}"), format!("base {i}\n").as_bytes());
    }
    ok(&d, &["record", "-m", "base"]);
    let acked = snapshot(&d);
    for i in 0..8 {
        write(&d, &format!("new/n{i}"), format!("new {i}\n").as_bytes());
    }
    write(&d, "base/f0", b"edited\n");
    fs::remove_file(d.join("base/f1")).unwrap();
    (d, acked)
}

/// After a crash: the repo opens, a rerun converges, the acknowledged
/// checkpoint survives and a clone reproduces the working directory.
fn recover_and_check(d: &Path, acked: &BTreeMap<String, Option<Vec<u8>>>, tag: &str) {
    ok(d, &["status"]);
    let log = ok(d, &["log"]);
    assert!(log.contains("base"), "acknowledged checkpoint lost: {log}");
    // Ops durable but index behind (crash between the op sync and the
    // index save, or a surviving prefix of an un-acknowledged batch): the
    // documented recovery is `hub checkout`, which lands on head. It may
    // restore a file whose *unrecorded* deletion was only partly recorded
    // (pre-existing behavior, open concern on #29); unacknowledged work only.
    let behind = !hub_env(d, &["record"], &[]).status.success();
    if behind {
        // The unacknowledged working changes are all still on disk.
        let w = snapshot(d);
        assert_eq!(w.get("base/f0"), Some(&Some(b"edited\n".to_vec())));
        if !hub_env(d, &["checkout"], &[]).status.success() {
            // A prefix that created a file but lost its content: checkout
            // refuses to clobber it (open concern on #29). The user's way
            // out: set the edits aside, force, put them back, record.
            ok(d, &["checkout", "--force"]);
            for (p, v) in &w {
                if let Some(b) = v {
                    write(d, p, b);
                }
            }
        }
        ok(d, &["record"]);
    }
    assert!(ok(d, &["record"]).contains("nothing to record"));
    let want = snapshot(d);
    assert_eq!(want.get("base/f0"), Some(&Some(b"edited\n".to_vec())));
    if !behind {
        assert!(!want.contains_key("base/f1"));
    }
    let c = clone_of(d, &format!("{tag}-clone"));
    assert_eq!(snapshot(&c), want, "clone differs from recovered repo");
    // The acknowledged version is still reachable by its checkpoint.
    let id = log
        .lines()
        .find(|l| l.contains("base"))
        .and_then(|l| l.split_whitespace().next())
        .unwrap()
        .to_string();
    ok(d, &["checkout", &id]);
    let got = snapshot(d);
    ok(d, &["checkout"]);
    assert_eq!(snapshot(d), want);
    assert_eq!(&got, acked, "acknowledged version changed");
}

fn base_id(d: &Path) -> String {
    ok(d, &["log"])
        .lines()
        .find(|l| l.ends_with(" base"))
        .and_then(|l| l.split_whitespace().next())
        .unwrap()
        .to_string()
}

const RECORD_STAGES: &[&str] = &[
    durable::BLOBS_WRITTEN,
    durable::BLOBS_BARRIERED,
    durable::BLOBS_RENAMED,
    durable::OPS_APPENDED,
    durable::COMMIT_WRITTEN,
    durable::OPS_FSYNCED,
];

#[test]
fn record_crash_at_every_stage_recovers() {
    for st in RECORD_STAGES {
        let (d, acked) = setup(&format!("rec-{st}"));
        let before = ops_len(&d);
        crash(&d, &["record", "-m", "x"], st);
        let ops_grew = ops_len(&d) > before;
        assert_eq!(
            ops_grew,
            *st == durable::OPS_APPENDED
                || *st == durable::COMMIT_WRITTEN
                || *st == durable::OPS_FSYNCED,
            "{st}: ops must only reach the log after the blob barrier"
        );
        recover_and_check(&d, &acked, &format!("rec-{st}"));
    }
}

#[test]
fn sync_crash_at_every_stage_recovers() {
    for st in RECORD_STAGES {
        let (a, acked) = setup(&format!("syn-{st}"));
        ok(&a, &["record", "-m", "more"]);
        let b = tmp(&format!("syn-{st}-b"));
        ok(&b, &["init"]);
        crash(&b, &["sync", a.to_str().unwrap()], st);
        ok(&a, &["status"]);
        ok(&b, &["status"]);
        let s = ok(&b, &["sync", a.to_str().unwrap()]);
        assert!(s.contains("received"), "{s}");
        ok(&b, &["checkout"]);
        assert_eq!(snapshot(&b), snapshot(&a), "{st}");
        assert!(ok(&b, &["record"]).contains("nothing to record"));
        // Nothing acknowledged by `a` was touched by the crashed sync.
        ok(&a, &["checkout", &base_id(&a)]);
        assert_eq!(snapshot(&a), acked, "{st}");
        let c = clone_of(&b, &format!("syn-{st}-c"));
        ok(&a, &["checkout"]);
        assert_eq!(snapshot(&c), snapshot(&a), "{st}");
    }
}

#[test]
fn checkout_crash_at_every_stage_resumes() {
    for st in [
        durable::FILES_WRITTEN,
        durable::FILES_BARRIERED,
        durable::FILES_RENAMED,
    ] {
        let (a, _) = setup(&format!("co-{st}"));
        ok(&a, &["record", "-m", "more"]);
        let b = tmp(&format!("co-{st}-b"));
        ok(&b, &["init"]);
        ok(&b, &["sync", a.to_str().unwrap()]);
        crash(&b, &["checkout"], st);
        assert!(b.join(".hub/checkout-in-progress").exists(), "{st}");
        let (_, err) = run_ok(&b, &["checkout"]);
        assert!(err.contains("resuming interrupted checkout"), "{st}: {err}");
        assert!(!b.join(".hub/checkout-in-progress").exists());
        assert_eq!(snapshot(&b), snapshot(&a), "{st}");
        assert!(ok(&b, &["record"]).contains("nothing to record"));
    }
}

/// Power-loss simulation for the op log: crash after the lines are written
/// but before the one sync, then damage the un-synced tail the ways a power
/// loss may (lost, cut mid-record, zero-filled hole). The prefix up to the
/// last acknowledged sync is untouched.
#[test]
fn power_loss_damage_to_unsynced_op_tail_recovers() {
    type Damage = fn(&Path, u64);
    let cases: &[(&str, Damage)] = &[
        ("lost", |p, synced| {
            let f = fs::OpenOptions::new().write(true).open(p).unwrap();
            f.set_len(synced).unwrap();
        }),
        ("cut", |p, synced| {
            let len = fs::metadata(p).unwrap().len();
            let f = fs::OpenOptions::new().write(true).open(p).unwrap();
            f.set_len(synced + (len - synced) / 2 + 3).unwrap();
        }),
        ("hole", |p, synced| {
            // Zero the second half of the un-synced region (a trailing
            // hole: nothing valid follows it).
            let mut b = fs::read(p).unwrap();
            let s = synced as usize;
            let lo = s + (b.len() - s) / 2;
            let hi = b.len();
            b[lo..hi].fill(0);
            fs::write(p, b).unwrap();
        }),
        ("hole-at-start", |p, synced| {
            let mut b = fs::read(p).unwrap();
            let s = synced as usize;
            let hi = b.len();
            b[s..hi].fill(0);
            fs::write(p, b).unwrap();
        }),
    ];
    for (name, damage) in cases {
        let (d, acked) = setup(&format!("pl-{name}"));
        let synced = ops_len(&d);
        crash(&d, &["record", "-m", "x"], durable::OPS_APPENDED);
        assert!(ops_len(&d) > synced);
        damage(&d.join(".hub/ops.log"), synced);
        let (_, err) = run_ok(&d, &["status"]);
        if *name != "lost" {
            assert!(
                err.contains("warning"),
                "{name}: no torn-tail warning: {err}"
            );
        }
        recover_and_check(&d, &acked, &format!("pl-{name}"));
    }
}

/// A NUL byte followed only by valid records after it is cut; a bad record
/// without NUL that is not last is still corruption (Appendix A.1).
#[test]
fn garbage_that_is_not_a_hole_is_still_corruption() {
    let (d, _) = setup("pl-garbage");
    ok(&d, &["record"]);
    let p = d.join(".hub/ops.log");
    let mut b = fs::read(&p).unwrap();
    let second_line = b.iter().position(|&c| c == b'\n').unwrap() + 1;
    b[second_line + 2] = b'#';
    fs::write(&p, b).unwrap();
    let o = hub_env(&d, &["status"], &[]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("corrupt"));
}

/// Power-loss simulation for blobs and working files: temps that were
/// written but not barriered are truncated or lost; the rerun sweeps them
/// (record) or the marker resumes (checkout).
#[test]
fn power_loss_damage_to_unbarriered_temps_recovers() {
    let (d, acked) = setup("pl-blobs");
    crash(&d, &["record", "-m", "x"], durable::BLOBS_WRITTEN);
    let blobs = d.join(".hub/blobs");
    let mut n = 0;
    for e in fs::read_dir(&blobs).unwrap() {
        let e = e.unwrap();
        if e.file_name().to_string_lossy().starts_with("tmp-") {
            n += 1;
            if n % 2 == 0 {
                fs::remove_file(e.path()).unwrap();
            } else {
                let f = fs::OpenOptions::new().write(true).open(e.path()).unwrap();
                f.set_len(1).unwrap();
            }
        }
    }
    assert!(n >= 8, "expected one temp per new blob, got {n}");
    let (_, err) = run_ok(&d, &["record", "-m", "x"]);
    assert!(err.contains("incomplete blob temp"), "{err}");
    recover_and_check(&d, &acked, "pl-blobs");

    let (a, _) = setup("pl-co");
    ok(&a, &["record", "-m", "more"]);
    let b = tmp("pl-co-b");
    ok(&b, &["init"]);
    ok(&b, &["sync", a.to_str().unwrap()]);
    crash(&b, &["checkout"], durable::FILES_WRITTEN);
    for e in fs::read_dir(b.join(".hub/tmp")).unwrap() {
        let f = fs::OpenOptions::new()
            .write(true)
            .open(e.unwrap().path())
            .unwrap();
        f.set_len(0).unwrap();
    }
    let (_, err) = run_ok(&b, &["checkout"]);
    assert!(err.contains("resuming interrupted checkout"), "{err}");
    assert_eq!(snapshot(&b), snapshot(&a));
}

/// `<stage>:<k>` fires at the k-th occurrence: a sync persists both sides,
/// so `ops-fsynced:2` crashes after the second side's op sync.
#[test]
fn stage_occurrence_selects_the_second_side_of_a_sync() {
    let (a, _) = setup("occ-a");
    ok(&a, &["record", "-m", "more"]);
    let b = tmp("occ-b");
    ok(&b, &["init"]);
    write(&b, "only-b", b"b\n");
    ok(&b, &["record", "-m", "b"]);
    let (la, lb) = (ops_len(&a), ops_len(&b));
    crash(
        &b,
        &["sync", a.to_str().unwrap()],
        &format!("{}:2", durable::OPS_FSYNCED),
    );
    assert!(ops_len(&a) > la && ops_len(&b) > lb, "both sides persisted");
    ok(&b, &["sync", a.to_str().unwrap()]);
    ok(&a, &["checkout"]);
    ok(&b, &["checkout"]);
    assert_eq!(snapshot(&a), snapshot(&b));
}

/// #29 repair: before the index save, checkout flushes the parent of every
/// level of a created directory chain, of every written file and of every
/// removed path (test-only flush log).
#[test]
fn checkout_flushes_created_chain_and_removed_parents() {
    let d = tmp("flushlog");
    ok(&d, &["init"]);
    write(&d, "gone/g", b"g\n");
    write(&d, "keep/k", b"k\n");
    ok(&d, &["record", "-m", "v1"]);
    let v1 = ok(&d, &["log"])
        .lines()
        .find(|l| l.ends_with(" v1"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    fs::remove_dir_all(d.join("gone")).unwrap();
    write(&d, "n1/n2/n3/f", b"f\n");
    ok(&d, &["record", "-m", "v2"]);
    ok(&d, &["checkout", &v1]);
    assert!(d.join("gone/g").exists() && !d.join("n1").exists());
    let log = d.with_extension("flushlog");
    let o = hub_env(
        &d,
        &["checkout"],
        &[(durable::FLUSH_LOG_ENV, log.to_str().unwrap())],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(d.join("n1/n2/n3/f").exists() && !d.join("gone").exists());
    let root = fs::canonicalize(&d).unwrap().display().to_string();
    let rel: std::collections::BTreeSet<String> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| {
            let r = l.strip_prefix(&root).expect("flushed dir under the repo");
            r.trim_start_matches('/').to_string()
        })
        .collect();
    for want in ["", "n1", "n1/n2", "n1/n2/n3"] {
        assert!(rel.contains(want), "missing flush of '{want}': {rel:?}");
    }
    // `gone/g` removed: its parent `gone` was removed too, so `gone` itself
    // is gone and its removal is covered by the root flush above.
    assert!(!rel.contains("keep"), "untouched dir flushed: {rel:?}");

    // A removal whose parent survives flushes that parent.
    fs::remove_file(&log).unwrap();
    ok(&d, &["rm", "keep/k"]);
    let o = hub_env(
        &d,
        &["checkout", &v1],
        &[(durable::FLUSH_LOG_ENV, log.to_str().unwrap())],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let _ = fs::remove_file(&log);
    let o = hub_env(
        &d,
        &["checkout"],
        &[(durable::FLUSH_LOG_ENV, log.to_str().unwrap())],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let text = fs::read_to_string(&log).unwrap();
    assert!(
        text.lines().any(|l| l == format!("{root}/keep")),
        "parent of removed keep/k not flushed: {text}"
    );
}

/// #29 re-review regression (pinned from wd_roundtrip): a removed path's
/// parent `a/a` whose ancestor `a` becomes a file in the same apply. The
/// flush skips the vanished dirs (ENOTDIR/NotFound) and flushes the nearest
/// existing ancestor (the root, which holds the new file `a`).
#[test]
fn dir_replaced_by_file_in_same_apply() {
    let d = tmp("dir2file");
    ok(&d, &["init"]);
    write(&d, "a/a/x", b"x\n");
    ok(&d, &["record", "-m", "v1"]);
    let v1 = ok(&d, &["log"])
        .lines()
        .find(|l| l.ends_with(" v1"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    fs::remove_dir_all(d.join("a")).unwrap();
    write(&d, "a", b"now a file\n");
    ok(&d, &["record", "-m", "v2"]);
    let log = d.with_extension("flushlog");
    let root = fs::canonicalize(&d).unwrap().display().to_string();
    for (target, is_file) in [(v1.as_str(), false), ("", true)] {
        let _ = fs::remove_file(&log);
        let args: Vec<&str> = if target.is_empty() {
            vec!["checkout"]
        } else {
            vec!["checkout", target]
        };
        let o = hub_env(
            &d,
            &args,
            &[(durable::FLUSH_LOG_ENV, log.to_str().unwrap())],
        );
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(!d.join(".hub/checkout-in-progress").exists());
        assert_eq!(d.join("a").is_file(), is_file);
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.lines().any(|l| l == root), "root not flushed: {text}");
    }
    assert!(ok(&d, &["record"]).contains("nothing to record"));
}

// ------------------------------------------------ #34 repair: blob durability

const BLOB_STAGES: &[&str] = &[
    durable::BLOBS_WRITTEN,
    durable::BLOBS_BARRIERED,
    durable::BLOBS_RENAMED,
];

/// Rerun `args` in `d` with the barrier and flush logs, aborting at the
/// first `ops-appended`: whatever the logs hold was done before any op line
/// was written. Returns (barrier lines, flush lines).
fn rerun_until_op_append(d: &Path, args: &[&str], tag: &str) -> (Vec<String>, Vec<String>) {
    let t = tmp(&format!("{tag}-logs"));
    let (bl, fl) = (t.join("bar"), t.join("flush"));
    let o = hub_env(
        d,
        args,
        &[
            (CRASH_STAGE_ENV, durable::OPS_APPENDED),
            (hub_cli::blobs::BARRIER_LOG_ENV, bl.to_str().unwrap()),
            (durable::FLUSH_LOG_ENV, fl.to_str().unwrap()),
        ],
    );
    assert!(!o.status.success(), "{tag}: rerun must reach the op append");
    let read = |p: &Path| -> Vec<String> {
        fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    };
    let r = (read(&bl), read(&fl));
    let _ = fs::remove_dir_all(t);
    r
}

fn assert_blobs_durable_first(d: &Path, bars: &[String], flushes: &[String], tag: &str) {
    let blobs = fs::canonicalize(d.join(".hub/blobs")).unwrap();
    let canon = |p: &str| fs::canonicalize(p).ok();
    assert!(
        bars.iter().any(|l| {
            let rest = l.strip_prefix("blob-barrier ").unwrap();
            let (dir, _) = rest.rsplit_once(' ').unwrap();
            canon(dir) == Some(blobs.clone())
        }),
        "{tag}: no blob barrier before the op append: {bars:?}"
    );
    assert!(
        flushes.iter().any(|l| canon(l) == Some(blobs.clone())),
        "{tag}: .hub/blobs not flushed before the op append: {flushes:?}"
    );
}

/// After a crash in the blob group commit, the rerun must flush and barrier
/// the blobs (including files left hash-valid but never barriered) before
/// it appends any op naming them.
#[test]
fn rerun_after_blob_crash_barriers_blobs_before_ops() {
    for st in BLOB_STAGES {
        // record
        let (d, acked) = setup(&format!("r34-rec-{st}"));
        crash(&d, &["record", "-m", "x"], st);
        let (b, f) = rerun_until_op_append(&d, &["record", "-m", "x"], &format!("rec-{st}"));
        assert_blobs_durable_first(&d, &b, &f, &format!("record {st}"));
        recover_and_check(&d, &acked, &format!("r34-rec-{st}"));
        // sync
        let (a, _) = setup(&format!("r34-syn-{st}"));
        ok(&a, &["record", "-m", "more"]);
        let s = tmp(&format!("r34-syn-{st}-b"));
        ok(&s, &["init"]);
        crash(&s, &["sync", a.to_str().unwrap()], st);
        let (b, f) =
            rerun_until_op_append(&s, &["sync", a.to_str().unwrap()], &format!("syn-{st}"));
        assert_blobs_durable_first(&s, &b, &f, &format!("sync {st}"));
        ok(&s, &["sync", a.to_str().unwrap()]);
        ok(&s, &["checkout"]);
        assert_eq!(snapshot(&s), snapshot(&a), "{st}");
    }
}

/// Non-truncated orphans: hash-valid blob files that no op names (left by a
/// crash before the barrier, so not provably durable). A record of the same
/// bytes and a sync that brings ops naming them must both flush and barrier
/// them before the ops are appended, even though nothing is rewritten.
#[test]
fn intact_orphan_blobs_are_made_durable_before_use() {
    // sync: every blob of the batch is left intact after the crash.
    let (a, _) = setup("r34-orph-a");
    ok(&a, &["record", "-m", "more"]);
    let s = tmp("r34-orph-s");
    ok(&s, &["init"]);
    crash(&s, &["sync", a.to_str().unwrap()], durable::BLOBS_WRITTEN);
    let n = fs::read_dir(s.join(".hub/blobs")).unwrap().count();
    assert!(n >= 8, "direct-written blobs present: {n}");
    let (b, f) = rerun_until_op_append(&s, &["sync", a.to_str().unwrap()], "orph-sync");
    assert_blobs_durable_first(&s, &b, &f, "sync orphan");
    let batched: usize = b
        .iter()
        .map(|l| l.rsplit(' ').next().unwrap().parse::<usize>().unwrap())
        .sum();
    assert!(batched >= n, "every orphan joins the batch: {b:?}");
    ok(&s, &["sync", a.to_str().unwrap()]);
    ok(&s, &["checkout"]);
    assert_eq!(snapshot(&s), snapshot(&a));
    // record of the same bytes as intact orphans.
    let r = tmp("r34-orph-r");
    ok(&r, &["init"]);
    for i in 0..4 {
        write(&r, &format!("o{i}"), format!("orphan {i}\n").as_bytes());
    }
    crash(&r, &["record"], durable::BLOBS_RENAMED);
    let (b, f) = rerun_until_op_append(&r, &["record"], "orph-rec");
    assert_blobs_durable_first(&r, &b, &f, "record orphan");
    ok(&r, &["record"]);
    ok(&r, &["status"]);
}
