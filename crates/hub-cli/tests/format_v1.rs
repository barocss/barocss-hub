//! Issue #33: on-disk format v1 (Wiki Appendix A.2). Real `hub` binary:
//! batch commit records under crashes and power-loss damage, snapshot
//! validation and fallback, the v0 -> v1 migration crashed at every step,
//! and (with `HUB_V0_BIN` set to a v0 release binary) old-binary refusal.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use hub_cli::durable::{self, CRASH_STAGE_ENV};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hub-v1-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

fn hub_bin(bin: &Path, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(bin);
    c.args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null());
    c.env_remove("HUB_SNAPSHOT_MIN")
        .env_remove("HUB_NO_SNAPSHOT");
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn hub_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    hub_bin(Path::new(env!("CARGO_BIN_EXE_barocss-hub")), dir, args, env)
}

/// Snapshot on every write (the policy's minimum set to 1 op).
const SNAP: &[(&str, &str)] = &[("HUB_SNAPSHOT_MIN", "0")];
const NOSNAP: &[(&str, &str)] = &[("HUB_NO_SNAPSHOT", "1")];

fn ok_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> (String, String) {
    let o = hub_env(dir, args, env);
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
    ok_env(dir, args, SNAP).0
}

fn crash(dir: &Path, args: &[&str], stage: &str, env: &[(&str, &str)]) {
    let mut e = env.to_vec();
    e.push((CRASH_STAGE_ENV, stage));
    let o = hub_env(dir, args, &e);
    assert!(
        !o.status.success(),
        "hub {args:?} at stage {stage} did not crash: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

fn write(root: &Path, p: &str, b: &[u8]) {
    let f = root.join(p);
    fs::create_dir_all(f.parent().unwrap()).unwrap();
    fs::write(f, b).unwrap();
}

/// Every file under `dir` (relative path -> bytes; directories as None).
fn files(dir: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(base: &Path, d: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        for e in fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            let rel = e
                .path()
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .to_string();
            if e.file_type().unwrap().is_dir() {
                out.insert(rel, None);
                walk(base, &e.path(), out);
            } else {
                out.insert(rel, Some(fs::read(e.path()).unwrap()));
            }
        }
    }
    let mut m = BTreeMap::new();
    walk(dir, dir, &mut m);
    m
}

fn worktree(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    files(root)
        .into_iter()
        .filter(|(k, _)| k != ".hub" && !k.starts_with(".hub/"))
        .collect()
}

/// Read-only views whose stdout must not depend on the snapshot/migration.
const VIEWS: &[&[&str]] = &[
    &["status"],
    &["log"],
    &["ls", "-l"],
    &["warnings"],
    &["show", "-l", "0000"],
];

fn views(d: &Path, env: &[(&str, &str)]) -> Vec<String> {
    VIEWS
        .iter()
        .map(|a| {
            let o = hub_env(d, a, env);
            format!(
                "{:?}\n{}",
                o.status.code(),
                String::from_utf8_lossy(&o.stdout)
            )
        })
        .collect()
}

/// A repo with some history: three recorded rounds, a checkpoint each.
fn setup(tag: &str) -> PathBuf {
    let d = tmp(tag);
    ok(&d, &["init"]);
    for r in 0..3 {
        for i in 0..4 {
            write(&d, &format!("d{i}/f{r}"), format!("{r}.{i}\n").as_bytes());
        }
        write(&d, "top", format!("round {r}\n").as_bytes());
        ok(&d, &["record", "-m", &format!("r{r}")]);
    }
    d
}

fn ops_len(d: &Path) -> u64 {
    fs::metadata(d.join(".hub/ops.log")).unwrap().len()
}

// ---------- batches ----------

/// The commit record is written after the op lines; a process crash at
/// `commit-written` keeps the batch (page cache), at `ops-appended` drops
/// it (no commit record), and both open cleanly.
#[test]
fn batch_crash_stages() {
    for (st, kept) in [
        (durable::OPS_APPENDED, false),
        (durable::COMMIT_WRITTEN, true),
        (durable::OPS_FSYNCED, true),
    ] {
        let d = setup(&format!("b-{st}"));
        let before_views = views(&d, SNAP);
        let synced = ops_len(&d);
        write(&d, "top", b"crashed edit\n");
        crash(&d, &["record"], st, SNAP);
        assert!(ops_len(&d) > synced, "{st}: lines written");
        let (_, err) = ok_env(&d, &["ls"], SNAP);
        if kept {
            assert!(!err.contains("recovered"), "{st}: {err}");
            assert!(ops_len(&d) > synced);
            // Index still at the old base: checkout brings it to head.
            ok(&d, &["checkout"]);
        } else {
            assert!(err.contains("recovered: dropped"), "{st}: {err}");
            assert_eq!(ops_len(&d), synced, "{st}: uncommitted batch not cut");
            assert_eq!(views(&d, SNAP)[1..], before_views[1..], "{st}");
        }
        let o = ok(&d, &["record"]);
        assert!(
            kept == o.contains("nothing to record") || !kept,
            "{st}: {o}"
        );
        assert!(ok(&d, &["record"]).contains("nothing to record"));
        assert_eq!(fs::read(d.join("top")).unwrap(), b"crashed edit\n");
    }
}

/// Byte offsets of every line start after the header.
fn line_starts(b: &[u8]) -> Vec<usize> {
    let mut v = Vec::new();
    let mut p = b.iter().position(|&c| c == b'\n').unwrap() + 1;
    while p < b.len() {
        v.push(p);
        p += b[p..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(b.len() - p, |k| k + 1);
    }
    v
}

/// Power loss inside and after the last (un-acknowledged) batch: every
/// damage form is cut back to the last committed batch automatically,
/// including the #29 case (a zero hole followed by surviving records of
/// the same batch, even its commit record).
#[test]
fn power_loss_in_last_batch_recovers_automatically() {
    type Damage = fn(&mut Vec<u8>, usize);
    let cases: &[(&str, Damage)] = &[
        ("lost", |b, s| b.truncate(s)),
        ("cut-mid", |b, s| {
            let n = s + (b.len() - s) / 2 + 3;
            b.truncate(n)
        }),
        ("cut-commit", |b, _| {
            let n = b.len() - 5;
            b.truncate(n)
        }),
        ("hole-tail", |b, s| {
            let lo = s + (b.len() - s) / 2;
            let hi = b.len();
            b[lo..hi].fill(0)
        }),
        // #29: hole at the start of the batch, the rest (incl. B) intact.
        ("hole-then-survivors", |b, s| {
            let e = s + 20;
            b[s + 3..e].fill(0)
        }),
        ("hole-middle-line", |b, s| {
            let ls = line_starts(b);
            let k = ls.iter().position(|&x| x >= s).unwrap();
            let mid = ls[k + (ls.len() - k) / 2];
            b[mid..mid + 10].fill(0)
        }),
        ("hole-whole", |b, s| {
            let hi = b.len();
            b[s..hi].fill(0)
        }),
    ];
    for (name, damage) in cases {
        let d = setup(&format!("pl-{name}"));
        let synced = ops_len(&d) as usize;
        let want = views(&d, NOSNAP);
        for i in 0..6 {
            write(&d, &format!("new/n{i}"), b"x\n");
        }
        crash(
            &d,
            &["record", "-m", "lost"],
            durable::COMMIT_WRITTEN,
            NOSNAP,
        );
        let p = d.join(".hub/ops.log");
        let mut b = fs::read(&p).unwrap();
        damage(&mut b, synced);
        fs::write(&p, &b).unwrap();
        let (_, err) = ok_env(&d, &["ls"], NOSNAP);
        if *name != "lost" {
            assert!(err.contains("recovered: dropped"), "{name}: {err}");
        }
        assert_eq!(
            ops_len(&d) as usize,
            synced,
            "{name}: not cut to the last commit"
        );
        assert_eq!(views(&d, NOSNAP)[1..], want[1..], "{name}");
        // The rerun records the edits again.
        assert!(ok(&d, &["record", "-m", "again"]).contains("again"));
        assert!(ok(&d, &["log"]).contains("again"));
    }
}

/// Damage before the last committed batch is corruption: hard error, the
/// file is left byte-identical. With a valid snapshot covering the damaged
/// region the prefix is not re-read (Appendix A.2.3): the open succeeds
/// with the snapshot's (pre-damage) state and still writes nothing.
#[test]
fn damage_before_a_committed_batch_is_corruption() {
    for (name, hole) in [("nul", true), ("garbage", false)] {
        let d = setup(&format!("corr-{name}"));
        let p = d.join(".hub/ops.log");
        let mut b = fs::read(&p).unwrap();
        let ls = line_starts(&b);
        let at = ls[1] + 2;
        if hole {
            b[at..at + 6].fill(0);
        } else {
            b[at] = b'#';
        }
        fs::write(&p, &b).unwrap();
        let o = hub_env(&d, &["status"], NOSNAP);
        assert!(!o.status.success(), "{name}");
        assert!(
            String::from_utf8_lossy(&o.stderr).contains("corrupt"),
            "{name}"
        );
        assert_eq!(fs::read(&p).unwrap(), b, "{name}: file must be untouched");
        assert!(hub_env(&d, &["status"], SNAP).status.success(), "{name}");
        assert_eq!(fs::read(&p).unwrap(), b, "{name}: file must be untouched");
        // Without the snapshot the same corruption is reported again.
        fs::remove_file(d.join(".hub/snapshot")).unwrap();
        let o = hub_env(&d, &["status"], SNAP);
        assert!(!o.status.success(), "{name}");
        assert!(
            String::from_utf8_lossy(&o.stderr).contains("corrupt"),
            "{name}"
        );
        assert_eq!(fs::read(&p).unwrap(), b, "{name}: file must be untouched");
    }
}

// ---------- snapshots ----------

#[test]
fn snapshot_is_written_and_equals_replay() {
    let d = setup("snap-eq");
    assert!(d.join(".hub/snapshot").exists());
    let with = views(&d, SNAP);
    let without = views(&d, NOSNAP);
    assert_eq!(with, without);
    fs::remove_file(d.join(".hub/snapshot")).unwrap();
    assert_eq!(views(&d, SNAP), without);
    // Default policy: small repos take no snapshot.
    let e = tmp("snap-default");
    ok_env(&e, &["init"], &[]);
    write(&e, "a", b"a\n");
    ok_env(&e, &["record"], &[]);
    assert!(!e.join(".hub/snapshot").exists());
}

/// A snapshot followed by a tail of committed batches (policy threshold
/// not reached) gives the same outputs as a full replay.
#[test]
fn snapshot_plus_tail_equals_replay() {
    let d = setup("snap-tail");
    let snap = fs::read(d.join(".hub/snapshot")).unwrap();
    for r in 0..3 {
        write(&d, "top", format!("tail {r}\n").as_bytes());
        ok_env(
            &d,
            &["record", "-m", &format!("t{r}")],
            &[("HUB_SNAPSHOT_MIN", "100000")],
        );
    }
    assert_eq!(
        fs::read(d.join(".hub/snapshot")).unwrap(),
        snap,
        "not rewritten"
    );
    assert_eq!(views(&d, SNAP), views(&d, NOSNAP));
}

fn ignored(d: &Path, why: &str) {
    let want = views(d, NOSNAP);
    let o = hub_env(d, &["status"], SNAP);
    let err = String::from_utf8_lossy(&o.stderr).to_string();
    assert!(o.status.success(), "{why}: {err}");
    assert!(
        err.contains("warning: ignoring") && err.contains("snapshot"),
        "{why}: {err}"
    );
    assert!(err.contains(why), "{why}: {err}");
    // Read-only: warned, not removed; outputs are the replayed ones.
    assert!(d.join(".hub/snapshot").exists());
    assert_eq!(views(d, SNAP), want, "{why}");
    // A writer removes it (then the policy writes a fresh one).
    ok_env(d, &["commit", "-m", "w"], &[("HUB_SNAPSHOT_MIN", "100000")]);
    assert!(!d.join(".hub/snapshot").exists(), "{why}");
    ok(d, &["commit", "-m", "w2"]);
    assert!(d.join(".hub/snapshot").exists());
    let (_, err) = ok_env(d, &["status"], SNAP);
    assert!(!err.contains("ignoring"), "{err}");
}

#[test]
fn corrupt_snapshot_is_ignored() {
    let d = setup("snap-corrupt");
    let p = d.join(".hub/snapshot");
    let mut b = fs::read(&p).unwrap();
    let k = b.len() / 2;
    b[k] ^= 0x40;
    fs::write(&p, &b).unwrap();
    ignored(&d, "checksum mismatch");
    // Truncated file.
    let b = fs::read(&p).unwrap();
    fs::write(&p, &b[..b.len() - 3]).unwrap();
    ignored(&d, "checksum mismatch");
}

#[test]
fn stale_snapshot_is_ignored() {
    // The log no longer contains the covered batch: rolled back to an
    // older copy of ops.log (e.g. restored from a backup).
    let d = setup("snap-stale");
    let old = fs::read(d.join(".hub/ops.log")).unwrap();
    let ck = fs::read(d.join(".hub/checkpoints.log")).unwrap();
    let idx = fs::read(d.join(".hub/index")).unwrap();
    let wt = worktree(&d);
    write(&d, "top", b"newer\n");
    ok(&d, &["record"]);
    let snap = fs::read(d.join(".hub/snapshot")).unwrap();
    fs::write(d.join(".hub/ops.log"), &old).unwrap();
    fs::write(d.join(".hub/checkpoints.log"), &ck).unwrap();
    fs::write(d.join(".hub/index"), &idx).unwrap();
    for (k, v) in &wt {
        if let Some(b) = v {
            write(&d, k, b);
        }
    }
    fs::write(d.join(".hub/snapshot"), &snap).unwrap();
    ignored(&d, "stale");
}

#[test]
fn mismatched_snapshot_is_ignored() {
    // A snapshot from another repository with the same shape.
    let d = setup("snap-mis-a");
    let e = setup("snap-mis-b");
    fs::copy(e.join(".hub/snapshot"), d.join(".hub/snapshot")).unwrap();
    ignored(&d, "stale");
    // A different format version.
    let p = d.join(".hub/snapshot");
    let mut b = fs::read(&p).unwrap();
    let k = b"barocss-hub/snapshot/v".len();
    b[k] = b'9';
    fs::write(&p, &b).unwrap();
    ignored(&d, "wrong magic or version");
}

/// Crash while writing the snapshot: the old one (or none) stays valid.
#[test]
fn snapshot_write_crash_stages() {
    for st in [durable::SNAPSHOT_WRITTEN, durable::SNAPSHOT_RENAMED] {
        let d = setup(&format!("snap-crash-{st}"));
        write(&d, "top", b"more\n");
        crash(&d, &["record", "-m", "m"], st, SNAP);
        let (_, err) = ok_env(&d, &["status"], SNAP);
        assert!(!err.contains("ignoring"), "{st}: {err}");
        assert_eq!(views(&d, SNAP), views(&d, NOSNAP), "{st}");
        // The command's own work was durable before the snapshot step.
        assert!(ok(&d, &["log"]).contains(" m"), "{st}");
        assert!(ok(&d, &["record"]).contains("nothing to record"), "{st}");
    }
}

// ---------- migration ----------

/// Rewrite a v1 repository as the v0 layout (what a develop <= #32 binary
/// wrote for the same history): v0 headers, no commit records, no
/// snapshot/format/v1 peers/watch.lock.
fn to_v0(d: &Path) {
    let h = d.join(".hub");
    let log = fs::read_to_string(h.join("ops.log")).unwrap();
    let mut out = String::from("barocss-hub/oplog/v0\n");
    for l in log.lines().skip(1) {
        if !l.starts_with("B ") {
            out.push_str(l);
            out.push('\n');
        }
    }
    fs::write(h.join("ops.log"), out).unwrap();
    let r = fs::read_to_string(h.join("replica")).unwrap();
    fs::write(h.join("replica"), r.replace("replica/v1", "replica/v0")).unwrap();
    for f in ["snapshot", "format", "watch.lock", "peers"] {
        let _ = fs::remove_file(h.join(f));
    }
}

#[test]
fn migration_preserves_every_output() {
    let d = setup("mig-eq");
    let want = views(&d, NOSNAP);
    to_v0(&d);
    let v0 = files(&d.join(".hub"));
    let o = hub_env(&d, &["status"], &[]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("migrated"));
    assert_eq!(views(&d, NOSNAP), want);
    assert_eq!(views(&d, SNAP), want);
    let h = d.join(".hub");
    assert!(fs::read_to_string(h.join("replica"))
        .unwrap()
        .starts_with("barocss-hub/replica/v1\n"));
    assert_eq!(
        fs::read_to_string(h.join("format")).unwrap(),
        "barocss-hub/format/v1\n"
    );
    // #43 (D-041): no `peers` file any more.
    assert!(!h.join("peers").exists());
    assert!(h.join("watch.lock").exists());
    assert!(!h.join("ops.log.v1").exists());
    // Same op records, now one batch.
    let log = fs::read_to_string(h.join("ops.log")).unwrap();
    let v0log = String::from_utf8(v0["ops.log"].clone().unwrap()).unwrap();
    let ops: Vec<&str> = log
        .lines()
        .skip(1)
        .filter(|l| l.starts_with("O "))
        .collect();
    let ops0: Vec<&str> = v0log.lines().skip(1).collect();
    assert_eq!(ops, ops0);
    assert_eq!(log.lines().filter(|l| l.starts_with("B ")).count(), 1);
    // Idempotent: a second open changes nothing.
    let after = files(&h);
    ok_env(&d, &["status"], &[]);
    let again = files(&h);
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        again.keys().collect::<Vec<_>>()
    );
    assert_eq!(after["ops.log"], again["ops.log"]);
    // Writes continue normally.
    write(&d, "top", b"after migration\n");
    ok(&d, &["record", "-m", "post"]);
    assert_eq!(views(&d, SNAP), views(&d, NOSNAP));
}

#[test]
fn migration_keeps_peers_and_skips_torn_v0_tail() {
    let d = setup("mig-peers");
    let p = setup("mig-peers-peer");
    // #43: the v0 peer becomes a `file://` remote named after its folder,
    // exactly what this `remote add` makes.
    let name = p.file_name().unwrap().to_str().unwrap().to_string();
    ok(&d, &["remote", "add", &name, p.to_str().unwrap()]);
    let want = views(&d, NOSNAP);
    to_v0(&d);
    let h = d.join(".hub");
    fs::remove_file(h.join("remotes")).unwrap();
    fs::write(
        h.join("peers"),
        format!(
            "barocss-hub/peers/v0\n{}\n",
            fs::canonicalize(&p).unwrap().display()
        ),
    )
    .unwrap();
    // A torn v0 tail: left out of the new log (the v0 file is not cut
    // before the switch).
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(h.join("ops.log"))
        .unwrap();
    std::io::Write::write_all(&mut f, b"O 99.garbage").unwrap();
    drop(f);
    let (_, err) = ok_env(&d, &["status"], &[]);
    assert!(err.contains("migrated"), "{err}");
    assert_eq!(views(&d, NOSNAP), want);
    let want_list = format!(
        "{name}\tfile://{}\t(default)",
        fs::canonicalize(&p).unwrap().display()
    );
    assert_eq!(ok(&d, &["remote", "list"]).trim(), want_list);
    // The first exclusive command converts `peers` into `remotes`.
    ok(&d, &["record"]);
    assert!(!h.join("peers").exists());
    assert_eq!(ok(&d, &["remote", "list"]).trim(), want_list);
}

const MIG_STAGES: &[&str] = &[
    durable::MIG_LOG_WRITTEN,
    durable::MIG_LOG_SYNCED,
    durable::MIG_REPLICA_TMP,
    durable::MIG_SWITCHED,
    durable::MIG_LOG_RENAMED,
    durable::MIG_PEERS,
    durable::MIG_FORMAT,
    durable::SNAPSHOT_WRITTEN,
    durable::SNAPSHOT_RENAMED,
];

/// kill -9 (abort) at every migration step, for a writer and a reader:
/// before the switch the v0 files are byte-identical; after it the repo is
/// v1; either way the next open finishes and every output is unchanged.
#[test]
fn migration_crash_at_every_step() {
    for st in MIG_STAGES {
        for cmd in [&["status"][..], &["record"][..]] {
            let d = setup(&format!("mig-crash-{st}"));
            let want = views(&d, NOSNAP);
            to_v0(&d);
            let h = d.join(".hub");
            let v0 = files(&h);
            crash(&d, cmd, st, SNAP);
            let switched = fs::read_to_string(h.join("replica"))
                .unwrap()
                .contains("/v1\n");
            let before_switch = matches!(
                *st,
                durable::MIG_LOG_WRITTEN | durable::MIG_LOG_SYNCED | durable::MIG_REPLICA_TMP
            );
            assert_eq!(switched, !before_switch, "{st}");
            if before_switch {
                let now = files(&h);
                for (k, v) in &v0 {
                    assert_eq!(
                        now.get(k),
                        Some(v),
                        "{st}: v0 file {k} changed before the switch"
                    );
                }
            }
            // Reader first (shared lock upgrades to finish), then views.
            let (_, err) = ok_env(&d, &["ls"], SNAP);
            assert!(!err.contains("ignoring"), "{st}: {err}");
            assert_eq!(views(&d, SNAP), want, "{st} {cmd:?}");
            assert_eq!(views(&d, NOSNAP), want, "{st} {cmd:?}");
            assert!(!h.join("ops.log.v1").exists(), "{st}");
            assert_eq!(
                fs::read_to_string(h.join("format")).unwrap(),
                "barocss-hub/format/v1\n"
            );
            assert!(ok(&d, &["record"]).contains("nothing to record"), "{st}");
        }
    }
}

/// A newer format is refused, not migrated or damaged.
#[test]
fn newer_format_is_refused() {
    let d = setup("newer");
    let h = d.join(".hub");
    let r = fs::read_to_string(h.join("replica")).unwrap();
    fs::write(h.join("replica"), r.replace("replica/v1", "replica/v2")).unwrap();
    let before = files(&h);
    let o = hub_env(&d, &["status"], &[]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("unsupported repository format 'v2'"));
    assert_eq!(files(&h), before);
}

// ---------- old binaries ----------

/// `HUB_V0_BIN=<develop 0aff2d9 release hub>`: every v0 command on a v1
/// repository is refused and leaves `.hub/` byte-identical. Skipped (with a
/// note) when the variable is unset.
#[test]
fn v0_binary_refuses_v1_without_touching_it() {
    let Some(bin) = std::env::var_os("HUB_V0_BIN") else {
        eprintln!("HUB_V0_BIN not set; skipping the old-binary refusal check");
        return;
    };
    let bin = PathBuf::from(bin);
    let d = setup("v0bin");
    let peer = setup("v0bin-peer");
    ok(&d, &["remote", "add", "p", peer.to_str().unwrap()]);
    write(&d, "top", b"unrecorded edit\n");
    // A committed-but-uncheckpointed batch at the end of the log, plus a
    // trailing half line the v0 torn-tail rule would cut.
    write(&d, "extra", b"e\n");
    ok(&d, &["record"]);
    let h = d.join(".hub");
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(h.join("ops.log"))
            .unwrap();
        f.write_all(b"O 99.torn-tail-without-newline").unwrap();
    }
    let before = files(&h);
    let wt = worktree(&d);
    let ps = peer.to_str().unwrap();
    let cmds: &[&[&str]] = &[
        &["init"],
        &["record"],
        &["record", "-m", "x"],
        &["checkout"],
        &["checkout", "--force"],
        &["checkout", "0000"],
        &["mkdir", "nd"],
        &["touch", "nf"],
        &["mv", "top", "top2"],
        &["rm", "top"],
        &["ls"],
        &["ls", "-l"],
        &["status"],
        &["status", "--ignored"],
        &["commit", "-m", "c"],
        &["log"],
        &["show", "0000"],
        &["show", "-l", "0000"],
        &["sync", ps],
        &["publish", "r0"],
        &["peer", "list"],
        &["peer", "add", ps],
        &["peer", "rm", ps],
        &["watch", "--poll", "50"],
        &["warnings"],
        &["reid"],
    ];
    let mut report = Vec::new();
    for c in cmds {
        let o = hub_bin(&bin, &d, c, &[]);
        let err = String::from_utf8_lossy(&o.stderr).to_string();
        report.push(format!("{c:?} -> {:?}: {}", o.status.code(), err.trim()));
        assert!(!o.status.success(), "v0 {c:?} succeeded on v1: {err}");
        assert_eq!(files(&h), before, "v0 {c:?} changed .hub/");
        assert_eq!(worktree(&d), wt, "v0 {c:?} changed the working tree");
    }
    // The peer (v1) is untouched by the v0 sync too.
    eprintln!("{}", report.join("\n"));
    // Also from a subdirectory and via the peer side of a sync.
    let v0peer = tmp("v0bin-v0");
    assert!(hub_bin(&bin, &v0peer, &["init"], &[]).status.success());
    let o = hub_bin(&bin, &v0peer, &["sync", d.to_str().unwrap()], &[]);
    assert!(!o.status.success());
    assert_eq!(files(&h), before, "v0 sync from a v0 peer changed .hub/");
    // The v1 binary still opens it and the help text needs no repo.
    assert!(hub_bin(&bin, &d, &["help"], &[]).status.success());
    assert_eq!(files(&h), before);
    ok_env(&d, &["status"], SNAP);
}
