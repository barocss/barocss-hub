//! Issue #19: working directory (blob store, checkout, record) with the real
//! `hub` binary. Every test runs in its own temp dir.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use hub_core::tree::{BlobId, NodeId, Op, Payload};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hub-wd-test-{tag}-{}-{}",
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

fn hub(dir: &Path, args: &[&str]) -> Output {
    hub_env(dir, args, &[])
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let o = hub(dir, args);
    assert!(
        o.status.success(),
        "hub {args:?} failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

fn fail(dir: &Path, args: &[&str]) -> String {
    let o = hub(dir, args);
    assert!(!o.status.success(), "hub {args:?} unexpectedly succeeded");
    String::from_utf8(o.stderr).unwrap()
}

/// path -> Some(bytes) for files, None for dirs; `.hub` excluded.
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

fn wipe(root: &Path) {
    for e in fs::read_dir(root).unwrap() {
        let e = e.unwrap();
        if e.file_name() == ".hub" {
            continue;
        }
        if e.file_type().unwrap().is_dir() {
            fs::remove_dir_all(e.path()).unwrap();
        } else {
            fs::remove_file(e.path()).unwrap();
        }
    }
}

fn write(root: &Path, p: &str, bytes: &[u8]) {
    let fp = root.join(p);
    fs::create_dir_all(fp.parent().unwrap()).unwrap();
    fs::write(fp, bytes).unwrap();
}

/// `ls -l` / `show -l` lines -> path -> (node short id, blob prefix).
fn long(listing: &str) -> BTreeMap<String, (String, String)> {
    listing
        .lines()
        .filter(|l| !l.starts_with("version ") && !l.starts_with("    "))
        .map(|l| {
            let mut t = l.split_whitespace();
            let (n, b, p) = (t.next().unwrap(), t.next().unwrap(), t.next().unwrap());
            (p.to_string(), (n.to_string(), b.to_string()))
        })
        .collect()
}

fn blob12(bytes: &[u8]) -> String {
    BlobId::of(bytes).hex()[..12].to_string()
}

fn short_of(checkpoint_line: &str) -> String {
    checkpoint_line
        .trim()
        .trim_start_matches('[')
        .split(']')
        .next()
        .unwrap()
        .to_string()
}

#[test]
fn record_edit_rename_delete_then_log_show() {
    let d = tmp("flow");
    ok(&d, &["init"]);
    write(&d, "src/main.rs", b"fn main() {}\n");
    write(&d, "README", b"hello\n");
    write(&d, "notes.txt", b"scratch\n");
    fs::create_dir_all(d.join("empty")).unwrap();
    let st = ok(&d, &["status"]);
    assert!(st.contains("added:    src/main.rs"), "{st}");
    assert!(st.contains("added:    empty/"), "{st}");
    let r1 = ok(&d, &["record", "-m", "first"]);
    assert!(r1.contains("recorded 5 change(s)"), "{r1}");
    let c1 = short_of(r1.lines().last().unwrap());
    assert!(ok(&d, &["status"]).contains("working directory: clean"));

    // Edit, rename, delete through the filesystem.
    write(&d, "README", b"hello, world\n");
    fs::rename(d.join("src/main.rs"), d.join("src/app.rs")).unwrap();
    fs::remove_file(d.join("notes.txt")).unwrap();
    let st = ok(&d, &["status"]);
    assert!(st.contains("modified: README"), "{st}");
    assert!(st.contains("renamed:  src/main.rs -> src/app.rs"), "{st}");
    assert!(st.contains("deleted:  notes.txt"), "{st}");
    let r2 = ok(&d, &["record", "-m", "second"]);
    let c2 = short_of(r2.lines().last().unwrap());
    assert!(ok(&d, &["status"]).contains("working directory: clean"));

    let log = ok(&d, &["log"]);
    assert_eq!(log, format!("{c2} second\n{c1} first\n"));

    let s1 = long(&ok(&d, &["show", "-l", &c1]));
    let s2 = long(&ok(&d, &["show", "-l", &c2]));
    assert_eq!(
        s1.keys().collect::<Vec<_>>(),
        ["README", "empty/", "notes.txt", "src/", "src/main.rs"]
    );
    assert_eq!(
        s2.keys().collect::<Vec<_>>(),
        ["README", "empty/", "src/", "src/app.rs"]
    );
    assert_eq!(s1["README"].1, blob12(b"hello\n"));
    assert_eq!(s2["README"].1, blob12(b"hello, world\n"));
    assert_eq!(s1["src/main.rs"].1, blob12(b"fn main() {}\n"));
    // Rename detection kept the NodeId (a Move, not delete + create).
    assert_eq!(s1["src/main.rs"].0, s2["src/app.rs"].0);
    assert_eq!(s1["README"].0, s2["README"].0);
    assert_eq!(long(&ok(&d, &["ls", "-l"])), s2);
    let ops = fs::read_to_string(d.join(".hub/ops.log")).unwrap();
    assert_eq!(ops.lines().filter(|l| l.contains(" M ")).count(), 1);

    // Blobs are on disk under their SHA-256 and verified.
    for b in [&b"hello\n"[..], b"hello, world\n", b"fn main() {}\n"] {
        let p = d.join(".hub/blobs").join(BlobId::of(b).hex());
        assert_eq!(fs::read(p).unwrap(), b);
    }
}

#[test]
fn wipe_then_checkout_restores_bytes_and_old_versions() {
    let d = tmp("wipe");
    ok(&d, &["init"]);
    write(&d, "a/b/c.bin", &[0, 1, 2, 255, 10, 13]);
    write(&d, "a/empty.txt", b"");
    write(&d, "top with space.md", b"# t\n");
    let c1 = short_of(ok(&d, &["record", "-m", "v1"]).lines().last().unwrap());
    let snap1 = snapshot(&d);
    write(&d, "a/b/c.bin", b"v2");
    write(&d, "new/file", b"n");
    fs::remove_file(d.join("top with space.md")).unwrap();
    ok(&d, &["record", "-m", "v2"]);
    let snap2 = snapshot(&d);

    wipe(&d);
    assert!(ok(&d, &["status"]).contains("deleted:  a/"));
    let out = ok(&d, &["checkout"]);
    assert!(out.starts_with("checked out "), "{out}");
    assert_eq!(snapshot(&d), snap2);
    assert!(ok(&d, &["status"]).contains("working directory: clean"));

    // Older version, then back to head.
    let out = ok(&d, &["checkout", &c1]);
    assert!(out.contains("not at head"), "{out}");
    assert_eq!(snapshot(&d), snap1);
    let st = ok(&d, &["status"]);
    assert!(
        st.contains("(not head)") && st.contains("working directory: clean"),
        "{st}"
    );
    // record / mkdir refuse while detached.
    assert!(fail(&d, &["record"]).contains("not head"));
    assert!(fail(&d, &["mkdir", "x"]).contains("not head"));
    ok(&d, &["checkout"]);
    assert_eq!(snapshot(&d), snap2);
    assert_eq!(ok(&d, &["record"]), "nothing to record\n");
}

#[test]
fn checkout_refuses_to_clobber_without_force() {
    let d = tmp("clobber");
    ok(&d, &["init"]);
    write(&d, "f.txt", b"one");
    let c1 = short_of(ok(&d, &["record", "-m", "v1"]).lines().last().unwrap());
    write(&d, "f.txt", b"two");
    write(&d, "g.txt", b"g");
    ok(&d, &["record", "-m", "v2"]);

    // Modified tracked file would be overwritten.
    write(&d, "f.txt", b"local edit");
    let e = fail(&d, &["checkout", &c1]);
    assert!(e.contains("'f.txt' has unrecorded changes"), "{e}");
    assert_eq!(fs::read(d.join("f.txt")).unwrap(), b"local edit");
    // Checking out head (f.txt unchanged by target) keeps the local edit.
    ok(&d, &["checkout"]);
    assert_eq!(fs::read(d.join("f.txt")).unwrap(), b"local edit");
    assert!(ok(&d, &["status"]).contains("modified: f.txt"));
    // --force discards it.
    ok(&d, &["checkout", "--force", &c1]);
    assert_eq!(fs::read(d.join("f.txt")).unwrap(), b"one");
    assert!(!d.join("g.txt").exists());

    // Untracked file in the way of a path head would create.
    write(&d, "g.txt", b"my own untracked g");
    let e = fail(&d, &["checkout"]);
    assert!(e.contains("g.txt"), "{e}");
    assert_eq!(fs::read(d.join("g.txt")).unwrap(), b"my own untracked g");
    // Untracked files not in the way are never touched, even with --force.
    write(&d, "keep.me", b"k");
    ok(&d, &["checkout", "--force"]);
    assert_eq!(fs::read(d.join("g.txt")).unwrap(), b"g");
    assert_eq!(fs::read(d.join("keep.me")).unwrap(), b"k");
    // Deleting a modified tracked file is also refused.
    write(&d, "g.txt", b"edited g");
    assert!(fail(&d, &["checkout", &c1]).contains("would be deleted"));
}

#[test]
fn log_commands_update_the_working_directory() {
    let d = tmp("mut");
    ok(&d, &["init"]);
    ok(&d, &["mkdir", "src"]);
    ok(&d, &["touch", "src/a"]);
    assert!(d.join("src").is_dir() && d.join("src/a").is_file());
    write(&d, "src/a", b"content");
    ok(&d, &["record", "-m", "a"]);
    ok(&d, &["mv", "src/a", "src/b"]);
    assert_eq!(fs::read(d.join("src/b")).unwrap(), b"content");
    assert!(!d.join("src/a").exists());
    assert!(ok(&d, &["status"]).contains("working directory: clean"));
    // A modified file blocks rm/mv of it (nothing is appended).
    write(&d, "src/b", b"edited");
    let before = fs::read(d.join(".hub/ops.log")).unwrap();
    assert!(fail(&d, &["rm", "src/b"]).contains("unrecorded changes"));
    assert_eq!(fs::read(d.join(".hub/ops.log")).unwrap(), before);
    // An untracked file with other bytes blocks touch.
    write(&d, "src/c", b"untracked");
    assert!(fail(&d, &["touch", "src/c"]).contains("src/c"));
    ok(&d, &["record"]);
    ok(&d, &["rm", "src"]);
    assert!(!d.join("src").exists());
    assert!(ok(&d, &["status"]).contains("working directory: clean"));
}

fn repo(d: &Path) -> hub_cli::Repo {
    hub_cli::Repo::discover(d).unwrap()
}

#[test]
fn concurrent_set_content_conflict_shows_in_status_and_checkout_writes_winner() {
    let d = tmp("conflict");
    ok(&d, &["init"]);
    write(&d, "f", b"base");
    ok(&d, &["record", "-m", "base"]);
    let (node, winner_bytes) = {
        let mut r = repo(&d);
        let blobs = r.blobs().unwrap();
        let node = *hub_core::projection::project(r.state())
            .entries
            .get("f")
            .unwrap();
        let head: std::collections::BTreeSet<_> = r.head().frontier(&r.store).into_iter().collect();
        let mut ops = Vec::new();
        for (i, (rep, bytes)) in [(0xa1u128, &b"from A"[..]), (0xb2, b"from B")]
            .into_iter()
            .enumerate()
        {
            let blob = blobs.put(bytes).unwrap();
            ops.push(Op {
                id: NodeId::new(100 + i as u64, rep),
                deps: head.clone(),
                payload: Payload::SetContent { node, blob },
            });
        }
        for o in ops {
            r.append_op(o).unwrap();
        }
        (node, b"from B".to_vec())
    };
    let st = ok(&d, &["status"]);
    assert!(st.contains("content: f set by "), "{st}");
    assert!(st.contains("(not head)"), "{st}");
    ok(&d, &["checkout"]);
    assert_eq!(fs::read(d.join("f")).unwrap(), winner_bytes);
    assert!(ok(&d, &["status"]).contains("working directory: clean"));
    let _ = node;
}

/// Head tree: `README`, `readme` (files, clash), `Src/a` and `src/b` (dirs
/// clash), `other` (no clash, content "o").
fn clash_repo(d: &Path) {
    ok(d, &["init"]);
    let mut r = repo(d);
    let rid = r.replica_id.0;
    let dir = hub_core::tree::Kind::Directory;
    let file = hub_core::tree::Kind::File;
    let root = hub_core::tree::ROOT;
    let n = |c: u64| NodeId::new(c, rid);
    let creates = [
        (2u64, file, root, "README"),
        (3, file, root, "readme"),
        (4, dir, root, "Src"),
        (5, dir, root, "src"),
        (6, file, n(4), "a"),
        (7, file, n(5), "b"),
        (8, file, root, "other"),
    ];
    for (c, kind, parent, name) in creates {
        let deps = r.head().frontier(&r.store).into_iter().collect();
        r.append_op(Op {
            id: n(c),
            deps,
            payload: Payload::Create {
                kind,
                parent,
                name: name.into(),
            },
        })
        .unwrap();
    }
    let blob = r.blobs().unwrap().put(b"o").unwrap();
    let deps = r.head().frontier(&r.store).into_iter().collect();
    r.append_op(Op {
        id: n(9),
        deps,
        payload: Payload::SetContent { node: n(8), blob },
    })
    .unwrap();
}

fn fs_is_case_insensitive(d: &Path) -> bool {
    let probe = d.join("CaseProbe");
    fs::write(&probe, b"").unwrap();
    let r = d.join("caseprobe").exists();
    fs::remove_file(&probe).unwrap();
    r
}

/// D-019 assertions for a (real or forced) case-insensitive checkout.
fn assert_clash_skipped(d: &Path, env: &[(&str, &str)]) {
    let run = |args: &[&str]| {
        let o = hub_env(d, args, env);
        assert!(
            o.status.success(),
            "hub {args:?} failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        (
            String::from_utf8(o.stdout).unwrap(),
            String::from_utf8(o.stderr).unwrap(),
        )
    };
    let (_, err) = run(&["checkout"]);
    for p in ["README", "readme", "Src", "src", "Src/a", "src/b"] {
        assert!(err.contains(&format!("'{p}'")), "warning for {p}: {err}");
    }
    // Only the clashing paths are skipped; the rest is materialized.
    assert_eq!(
        snapshot(d),
        BTreeMap::from([("other".to_string(), Some(b"o".to_vec()))])
    );
    let (st, _) = run(&["status"]);
    assert!(st.contains("unmaterialized (case clash"), "{st}");
    for p in [
        "  README\n",
        "  readme\n",
        "  Src\n",
        "  src\n",
        "  Src/a\n",
        "  src/b\n",
    ] {
        assert!(st.contains(p), "status lists {p:?}: {st}");
    }
    // Unmaterialized paths are not "deleted" and record nothing.
    assert!(st.contains("working directory: clean"), "{st}");
    let (rec, _) = run(&["record"]);
    assert!(rec.contains("nothing to record"), "{rec}");
    let idx = fs::read_to_string(d.join(".hub/index")).unwrap();
    assert_eq!(
        idx.lines().filter(|l| l.starts_with("u ")).count(),
        6,
        "{idx}"
    );
    // Idempotent re-checkout; a non-clashing edit still records normally.
    run(&["checkout"]);
    assert_eq!(snapshot(d).len(), 1);
    fs::write(d.join("other"), b"o2").unwrap();
    let (rec, _) = run(&["record"]);
    assert!(rec.contains("modified: other"), "{rec}");
    let (st, _) = run(&["status"]);
    assert!(st.contains("unmaterialized (case clash"), "{st}");
    assert!(st.contains("working directory: clean"), "{st}");
}

/// #20 repair: a working file occupying (or case-folding onto, or under) an
/// unmaterialized path is reported as blocked; record refuses; a mutating
/// command that would materialize that path does not overwrite it.
fn assert_blocked_reported(d: &Path, env: &[(&str, &str)]) {
    let run = |args: &[&str]| hub_env(d, args, env);
    let out = |o: Output| String::from_utf8(o.stdout).unwrap();
    assert!(run(&["checkout"]).status.success());
    // A file at an exact unmaterialized path.
    fs::write(d.join("README"), b"user bytes").unwrap();
    let st = out(run(&["status"]));
    assert!(
        st.contains("blocked:  README collides with unmaterialized"),
        "{st}"
    );
    assert!(st.contains("'README'") && st.contains("'readme'"), "{st}");
    assert!(!st.contains("working directory: clean"), "{st}");
    let o = run(&["record"]);
    assert!(!o.status.success());
    let e = String::from_utf8(o.stderr).unwrap();
    assert!(
        e.contains("refusing to record") && e.contains("README"),
        "{e}"
    );
    assert!(e.contains("renam"), "{e}");
    // A directory case-folding onto an unmaterialized dir, with a file in it.
    fs::remove_file(d.join("README")).unwrap();
    fs::create_dir(d.join("SRC")).unwrap();
    fs::write(d.join("SRC/new"), b"n").unwrap();
    let st = out(run(&["status"]));
    assert!(
        st.contains("blocked:  SRC collides with unmaterialized"),
        "{st}"
    );
    assert!(
        st.contains("blocked:  SRC/new collides with unmaterialized"),
        "{st}"
    );
    assert!(!run(&["record"]).status.success());
    // Checkout does not touch the blocked paths.
    assert!(run(&["checkout"]).status.success());
    assert_eq!(fs::read(d.join("SRC/new")).unwrap(), b"n");
    // Resolve: remove them; clean again, record has nothing to do.
    fs::remove_dir_all(d.join("SRC")).unwrap();
    let st = out(run(&["status"]));
    assert!(st.contains("working directory: clean"), "{st}");
    assert!(out(run(&["record"])).contains("nothing to record"));
    // Un-clash by renaming one node: the occupying file is not overwritten.
    fs::write(d.join("README"), b"user bytes").unwrap();
    let o = run(&["mv", "readme", "readme2"]);
    assert_eq!(fs::read(d.join("README")).unwrap(), b"user bytes");
    // #23: the refusal names the command (mv), not checkout.
    assert!(!o.status.success());
    let e = String::from_utf8(o.stderr).unwrap();
    assert!(e.contains("refusing to mv") && e.contains("README"), "{e}");
    assert!(!e.contains("check out"), "{e}");
}

#[test]
fn blocked_file_on_unmaterialized_path_real_fs() {
    let d = tmp("case-blocked-real");
    clash_repo(&d);
    if fs_is_case_insensitive(&d) {
        assert_blocked_reported(&d, &[]);
    }
}

#[test]
fn blocked_file_on_unmaterialized_path_forced_probe() {
    let d = tmp("case-blocked-forced");
    clash_repo(&d);
    assert_blocked_reported(&d, &[("HUB_TEST_FORCE_CASE_INSENSITIVE", "1")]);
}

#[test]
fn case_clash_skips_only_clashing_files_real_fs() {
    // Uses the real filesystem's behavior (macOS APFS/HFS+ default:
    // case-insensitive; Linux ext4: case-sensitive).
    let d = tmp("case-real");
    clash_repo(&d);
    if fs_is_case_insensitive(&d) {
        assert_clash_skipped(&d, &[]);
    } else {
        ok(&d, &["checkout"]);
        for p in ["README", "readme", "Src/a", "src/b", "other"] {
            assert!(d.join(p).exists(), "{p}");
        }
        assert!(!ok(&d, &["status"]).contains("unmaterialized"));
    }
}

#[test]
fn case_clash_skips_only_clashing_files_forced_probe() {
    // Simulated case-insensitive probe via the test hook, so D-019 is
    // exercised on case-sensitive filesystems too.
    let d = tmp("case-forced");
    clash_repo(&d);
    assert_clash_skipped(&d, &[("HUB_TEST_FORCE_CASE_INSENSITIVE", "1")]);
}

#[test]
fn case_clash_appearing_later_removes_clean_tracked_file() {
    // `README` materialized first; a later `readme` makes both clash: the
    // clean tracked `README` is removed and both become unmaterialized.
    let d = tmp("case-later");
    let env = [("HUB_TEST_FORCE_CASE_INSENSITIVE", "1")];
    ok(&d, &["init"]);
    fs::write(d.join("README"), b"r").unwrap();
    assert!(hub_env(&d, &["record"], &env).status.success());
    {
        let mut r = repo(&d);
        let rid = r.replica_id.0;
        let deps = r.head().frontier(&r.store).into_iter().collect();
        let c = r
            .head()
            .frontier(&r.store)
            .iter()
            .map(|i| i.counter)
            .max()
            .unwrap()
            + 1;
        r.append_op(Op {
            id: NodeId::new(c, rid),
            deps,
            payload: Payload::Create {
                kind: hub_core::tree::Kind::File,
                parent: hub_core::tree::ROOT,
                name: "readme".into(),
            },
        })
        .unwrap();
    }
    let o = hub_env(&d, &["checkout"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(snapshot(&d).is_empty());
    let st = String::from_utf8(hub_env(&d, &["status"], &env).stdout).unwrap();
    assert!(
        st.contains("  README\n") && st.contains("  readme\n"),
        "{st}"
    );
    assert!(st.contains("working directory: clean"), "{st}");
}

#[cfg(unix)]
#[test]
fn symlinks_are_skipped_with_a_warning() {
    let d = tmp("symlink");
    ok(&d, &["init"]);
    write(&d, "real", b"r");
    std::os::unix::fs::symlink("real", d.join("link")).unwrap();
    let o = hub(&d, &["record"]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("skipping symlink 'link'"));
    assert_eq!(long(&ok(&d, &["ls", "-l"])).len(), 1);
}

#[test]
fn blob_store_crash_leaves_no_visible_blob() {
    let d = tmp("crash");
    ok(&d, &["init"]);
    let payload = vec![7u8; 4096];
    write(&d, "big", &payload);
    let o = hub_env(
        &d,
        &["record", "-m", "x"],
        &[(hub_cli::blobs::CRASH_ENV, "1")],
    );
    assert!(!o.status.success(), "must have aborted");
    let blobs = d.join(".hub/blobs");
    let names: Vec<String> = fs::read_dir(&blobs)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(names[0].starts_with("tmp-"), "{names:?}");
    assert_eq!(
        fs::read(blobs.join(&names[0])).unwrap().len(),
        2048,
        "torn temp"
    );
    // Blobs are stored before any op is appended: the log is untouched.
    let ops = fs::read_to_string(d.join(".hub/ops.log")).unwrap();
    assert_eq!(ops.lines().count(), 1, "{ops}");
    assert!(!blobs.join(BlobId::of(&payload).hex()).exists());

    // The next record sweeps the temp and records everything.
    let o = hub(&d, &["record", "-m", "x"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stderr).contains("incomplete blob temp"));
    let names: Vec<_> = fs::read_dir(&blobs).unwrap().collect();
    assert_eq!(names.len(), 1);
    assert_eq!(
        fs::read(blobs.join(BlobId::of(&payload).hex())).unwrap(),
        payload
    );
    wipe(&d);
    ok(&d, &["checkout"]);
    assert_eq!(fs::read(d.join("big")).unwrap(), payload);
}

#[test]
fn corrupt_blob_is_detected_on_read() {
    let d = tmp("corruptblob");
    ok(&d, &["init"]);
    write(&d, "f", b"precious");
    ok(&d, &["record", "-m", "x"]);
    let bp = d.join(".hub/blobs").join(BlobId::of(b"precious").hex());
    fs::write(&bp, b"precioux").unwrap();
    wipe(&d);
    let e = fail(&d, &["checkout"]);
    assert!(e.contains("corrupt blob"), "{e}");
    assert!(!d.join("f").exists(), "nothing written from a corrupt blob");
    // Re-adding the bytes repairs the blob in place.
    let blobs = hub_cli::blobs::Blobs::open(&d.join(".hub")).unwrap();
    assert!(!blobs.contains(&BlobId::of(b"precious")));
    blobs.put(b"precious").unwrap();
    ok(&d, &["checkout"]);
    assert_eq!(fs::read(d.join("f")).unwrap(), b"precious");
}

/// Issue #19 repair: temps live in `.hub/tmp/`; an untracked file named
/// like the old `.<name>.hub-tmp` scheme is never touched, and a leftover
/// temp is never seen by status or record.
#[test]
fn old_temp_name_survives_checkout_and_leftover_temps_are_invisible() {
    let d = tmp("tmpname");
    ok(&d, &["init"]);
    write(&d, "README", b"v1");
    let c1 = short_of(ok(&d, &["record", "-m", "one"]).lines().last().unwrap());
    write(&d, "README", b"v2");
    ok(&d, &["record", "-m", "two"]);
    write(&d, ".README.hub-tmp", b"precious");
    ok(&d, &["checkout", &c1]);
    assert_eq!(fs::read(d.join("README")).unwrap(), b"v1");
    assert_eq!(fs::read(d.join(".README.hub-tmp")).unwrap(), b"precious");
    ok(&d, &["checkout"]);
    assert_eq!(fs::read(d.join("README")).unwrap(), b"v2");
    assert_eq!(fs::read(d.join(".README.hub-tmp")).unwrap(), b"precious");
    fs::remove_file(d.join(".README.hub-tmp")).unwrap();

    // A crash leftover in .hub/tmp/: invisible to status and record, and
    // swept by the next exclusive writer.
    write(&d, ".hub/tmp/w-1-2-3", b"torn");
    let st = ok(&d, &["status"]);
    assert!(st.contains("working directory: clean"), "{st}");
    assert!(ok(&d, &["record"]).contains("nothing to record"));
    assert!(!d.join(".hub/tmp/w-1-2-3").exists(), "swept");
    assert!(!long(&ok(&d, &["ls", "-l"]))
        .keys()
        .any(|p| p.contains("tmp")));
    // No temp is left in the working tree or .hub/tmp after checkouts.
    let names: Vec<_> = snapshot(&d).into_keys().collect();
    assert_eq!(names, vec!["README".to_string()]);
    assert_eq!(fs::read_dir(d.join(".hub/tmp")).unwrap().count(), 0);
}

/// Several same-blob candidates are ambiguous: delete + create, no rename.
#[test]
fn ambiguous_rename_falls_back_to_delete_and_create() {
    // Two deleted, one added.
    let d = tmp("ambig1");
    ok(&d, &["init"]);
    write(&d, "a", b"same");
    write(&d, "b", b"same");
    ok(&d, &["record"]);
    let before = long(&ok(&d, &["ls", "-l"]));
    fs::remove_file(d.join("a")).unwrap();
    fs::remove_file(d.join("b")).unwrap();
    write(&d, "c", b"same");
    let r = ok(&d, &["record"]);
    assert!(!r.contains("renamed:"), "{r}");
    for l in ["deleted:  a", "deleted:  b", "added:    c"] {
        assert!(r.contains(l), "{r}");
    }
    let after = long(&ok(&d, &["ls", "-l"]));
    assert_eq!(after.keys().collect::<Vec<_>>(), vec!["c"]);
    assert!(before.values().all(|(n, _)| *n != after["c"].0), "new node");

    // One deleted, two added.
    let d = tmp("ambig2");
    ok(&d, &["init"]);
    write(&d, "a", b"same");
    ok(&d, &["record"]);
    let before = long(&ok(&d, &["ls", "-l"]));
    fs::remove_file(d.join("a")).unwrap();
    write(&d, "b", b"same");
    write(&d, "c", b"same");
    let r = ok(&d, &["record"]);
    assert!(!r.contains("renamed:"), "{r}");
    for l in ["deleted:  a", "added:    b", "added:    c"] {
        assert!(r.contains(l), "{r}");
    }
    let after = long(&ok(&d, &["ls", "-l"]));
    assert!(
        after.values().all(|(n, _)| *n != before["a"].0),
        "new nodes"
    );
}

/// A crash mid-checkout leaves the marker; record and tree edits refuse
/// until `hub checkout` finishes (resuming the interrupted target).
#[test]
fn interrupted_checkout_blocks_record_until_checkout_completes() {
    let d = tmp("midcheckout");
    ok(&d, &["init"]);
    write(&d, "a", b"a1");
    write(&d, "b", b"b1");
    let c1 = short_of(ok(&d, &["record", "-m", "one"]).lines().last().unwrap());
    write(&d, "a", b"a2");
    write(&d, "b", b"b2");
    ok(&d, &["record", "-m", "two"]);
    let o = hub_env(&d, &["checkout", &c1], &[(hub_cli::wd::CRASH_ENV, "1")]);
    assert!(!o.status.success(), "must have aborted");
    assert!(d.join(".hub/checkout-in-progress").exists());
    // Exactly one file was switched to the target: a half-checked-out tree.
    let snap = snapshot(&d);
    let n1 = snap
        .values()
        .filter(|v| v.as_deref().is_some_and(|b| b.ends_with(b"1")))
        .count();
    assert_eq!(n1, 1, "{snap:?}");

    for args in [
        &["record"][..],
        &["record", "-m", "x"],
        &["touch", "z"],
        &["rm", "a"],
    ] {
        let e = fail(&d, args);
        assert!(e.contains("checkout was interrupted"), "{args:?}: {e}");
    }
    assert!(ok(&d, &["status"]).contains("interrupted checkout"));
    let ops = fs::read_to_string(d.join(".hub/ops.log")).unwrap();

    // `hub checkout` resumes the interrupted target without --force.
    let o = hub(&d, &["checkout"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stderr).contains("resuming interrupted checkout"));
    assert!(!d.join(".hub/checkout-in-progress").exists());
    assert_eq!(fs::read(d.join("a")).unwrap(), b"a1");
    assert_eq!(fs::read(d.join("b")).unwrap(), b"b1");
    assert_eq!(
        fs::read_to_string(d.join(".hub/ops.log")).unwrap(),
        ops,
        "nothing recorded"
    );
    ok(&d, &["checkout"]);
    assert_eq!(fs::read(d.join("a")).unwrap(), b"a2");
    assert!(ok(&d, &["record"]).contains("nothing to record"));
}

fn index_line<'a>(idx: &'a str, path: &str) -> &'a str {
    idx.lines()
        .find(|l| l.starts_with(&format!("f {} ", hub_cli::store::esc(path))))
        .unwrap()
}

/// Racy-git: freshly written entries are smudged to `0 0`, and the stat
/// cache is trusted only if the file is strictly older than the index.
#[test]
fn racy_entries_are_smudged_and_guard_is_enforced() {
    let d = tmp("racy");
    ok(&d, &["init"]);
    write(&d, "f", b"aaaa");
    // Pin the mtime inside the racy window regardless of how long `record`
    // takes to start under load (#55): a minute ahead of now is always
    // within `RACY_NS` of the save time.
    fs::File::options()
        .write(true)
        .open(d.join("f"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
        .unwrap();
    ok(&d, &["record"]);
    let ip = d.join(".hub/index");
    let text = fs::read_to_string(&ip).unwrap();
    assert!(index_line(&text, "f").ends_with(" 0 0"), "smudged: {text}");

    // Same-size edit; forge a cache entry matching the new stat.
    write(&d, "f", b"bbbb");
    let md = fs::metadata(d.join("f")).unwrap();
    let mt = md
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let line = index_line(&text, "f");
    let mut t: Vec<&str> = line.split(' ').collect();
    let (sz, ms) = (md.len().to_string(), mt.to_string());
    t[4] = &sz;
    t[5] = &ms;
    let forged = text.replace(line, &t.join(" "));
    let set_idx_mtime = |at: std::time::SystemTime| {
        fs::write(&ip, &forged).unwrap();
        fs::File::options()
            .write(true)
            .open(&ip)
            .unwrap()
            .set_modified(at)
            .unwrap();
    };
    // Index not strictly newer than the file: racy, rehashed, detected.
    set_idx_mtime(md.modified().unwrap());
    assert!(ok(&d, &["status"]).contains("modified: f"));
    // Index clearly newer: the forged cache is trusted (proves the guard is
    // what caught the edit above).
    set_idx_mtime(md.modified().unwrap() + std::time::Duration::from_secs(10));
    assert!(ok(&d, &["status"]).contains("working directory: clean"));
}

#[test]
fn broken_pipe_is_quiet() {
    let d = tmp("epipe");
    ok(&d, &["init"]);
    ok(&d, &["commit", "-m", "c"]);
    let mut c = Command::new(env!("CARGO_BIN_EXE_barocss-hub"))
        .arg("log")
        .current_dir(&d)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    drop(c.stdout.take()); // reader goes away before hub writes
    let o = c.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(
        o.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}
