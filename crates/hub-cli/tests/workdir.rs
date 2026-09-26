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
    let mut c = Command::new(env!("CARGO_BIN_EXE_hub"));
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

#[test]
fn case_clash_is_refused_on_case_insensitive_fs() {
    let d = tmp("case");
    ok(&d, &["init"]);
    {
        let mut r = repo(&d);
        let rid = r.replica_id.0;
        for (c, name) in [(2u64, "README"), (3, "readme")] {
            let deps = r.head().frontier(&r.store).into_iter().collect();
            r.append_op(Op {
                id: NodeId::new(c, rid),
                deps,
                payload: Payload::Create {
                    kind: hub_core::tree::Kind::File,
                    parent: hub_core::tree::ROOT,
                    name: name.into(),
                },
            })
            .unwrap();
        }
    }
    let probe = d.join("CaseProbe");
    fs::write(&probe, b"").unwrap();
    let insensitive = d.join("caseprobe").exists();
    fs::remove_file(&probe).unwrap();
    if insensitive {
        let e = fail(&d, &["checkout"]);
        assert!(
            e.contains("case clash") && e.contains("README") && e.contains("readme"),
            "{e}"
        );
        let e = fail(&d, &["checkout", "--force"]);
        assert!(e.contains("case clash"), "{e}");
        assert!(!d.join("README").exists());
    } else {
        ok(&d, &["checkout"]);
        assert!(d.join("README").exists() && d.join("readme").exists());
    }
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
