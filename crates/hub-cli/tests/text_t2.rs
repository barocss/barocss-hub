//! Issue #54 (T2): `TextEdit` storage and wire with the real binary.
//! Text ops are injected through the library (nothing in the CLI authors
//! them before T3); everything else runs `barocss-hub`:
//! * convergence over `file://` and SSH/stdio (fake ssh), two and three
//!   replicas, and an in-process three-replica `file://` proptest;
//! * Hello feature `text`: `upgrade-required` on pull and on push, mixed
//!   remotes, `file://` too, nothing persisted (byte-identical `.hub/`);
//! * the lazy format v2 header: kill -9 (abort) at every upgrade stage,
//!   then rerun; idempotent; a v1 repository without text ops stays v1;
//!   the `v2-rescue` marker;
//! * copy-divergence detection (#46 digest) over text ops;
//! * with `HUB_TEST_OLD_BIN` (the pre-#54 develop build): the old binary
//!   refuses a v2 repository before writing, is refused text over the
//!   wire (old Hello), and behaves byte-identically on a v1 repository.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use hub_cli::store::LockMode;
use hub_cli::Repo;
use hub_core::text::author_text_edit;
use hub_core::tree::{evaluate_with, BlobId, NodeId, Op, OpId, Payload};
use proptest::prelude::*;

const BIN: &str = env!("CARGO_BIN_EXE_barocss-hub");

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "hub54-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    fs::canonicalize(p).unwrap()
}

fn cfg_home() -> &'static str {
    static D: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    D.get_or_init(|| tmp("xdg").to_string_lossy().into_owned())
}

fn run_bin(bin: &Path, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(bin);
    c.env("XDG_CONFIG_HOME", cfg_home())
        .env(
            "BAROCSS_HUB_CONFIG_DIR",
            Path::new(cfg_home()).join("barocss-hub"),
        )
        .env_remove("BAROCSS_HUB_SERVE_ROOT")
        .env_remove("HUB_TEST_NO_TEXT_FEATURE")
        .env_remove("HUB_SNAPSHOT_MIN")
        .env_remove("HUB_NO_SNAPSHOT")
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null());
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn hub(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    run_bin(Path::new(BIN), dir, args, env)
}

fn ok(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let o = hub(dir, args, env);
    assert!(
        o.status.success(),
        "hub {args:?} in {}: {}{}",
        dir.display(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

fn err_code(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let mut a = args.to_vec();
    a.push("--json");
    let o = hub(dir, &a, env);
    assert!(!o.status.success(), "hub {args:?} unexpectedly succeeded");
    let e = String::from_utf8_lossy(&o.stderr).into_owned();
    let i = e
        .find("\"code\":\"")
        .unwrap_or_else(|| panic!("no code in {e}"));
    e[i + 8..].split('"').next().unwrap().to_string()
}

/// Every file under `d` (`.hub/` included) with its bytes.
fn all_bytes(d: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut m = BTreeMap::new();
    let mut stack = vec![d.to_path_buf()];
    while let Some(x) = stack.pop() {
        for e in fs::read_dir(&x).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                m.insert(e.path(), fs::read(e.path()).unwrap());
            }
        }
    }
    m
}

/// `.hub/` bytes without files every run rewrites by design.
fn hub_bytes(d: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    all_bytes(&d.join(".hub"))
}

fn header(d: &Path) -> String {
    fs::read_to_string(d.join(".hub/replica"))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string()
}

fn repo(t: &Path, name: &str) -> PathBuf {
    let d = t.join(name);
    fs::create_dir_all(&d).unwrap();
    ok(&d, &["init"], &[]);
    d
}

/// Record `notes.md` in `d` with `text`; returns its node id.
fn record_file(d: &Path, text: &str) -> NodeId {
    fs::write(d.join("notes.md"), text).unwrap();
    ok(d, &["record"], &[]);
    node_of(d, "notes.md")
}

fn node_of(d: &Path, name: &str) -> NodeId {
    let r = Repo::open_dir_with(d.join(".hub"), LockMode::Shared).unwrap();
    *r.state()
        .iter()
        .find(|(_, n)| n.name == name)
        .unwrap_or_else(|| panic!("no {name}"))
        .0
}

/// Blob bytes of every `SetContent` in the store (epoch seeds).
fn blobs_of(r: &Repo) -> BTreeMap<BlobId, Vec<u8>> {
    let mut m = BTreeMap::new();
    for o in r.store.values() {
        if let Payload::SetContent { blob, .. } = o.payload {
            if let Ok(b) = hub_cli::blobs::read_verified(&r.dir.join("blobs"), &blob) {
                m.insert(blob, b);
            }
        }
    }
    m
}

/// Author a `TextEdit` turning `node`'s head text into `text` (deps = the
/// whole store = the author's base), through the library.
fn edit(d: &Path, node: NodeId, text: &str) -> Op {
    let mut r = Repo::open_dir_with(d.join(".hub"), LockMode::Exclusive).unwrap();
    let blobs = blobs_of(&r);
    let ev = evaluate_with(r.store.values(), &blobs);
    let te = author_text_edit(&r.store, &ev, &blobs, node, text.as_bytes()).expect("text epoch");
    let op = r.prepare(Payload::TextEdit(te));
    r.append_op(op.clone()).unwrap();
    op
}

/// Head text of `node` from the replica (and equal to `evaluate_with`).
fn text(d: &Path, node: NodeId) -> String {
    let mut r = Repo::open_dir_with(d.join(".hub"), LockMode::Shared).unwrap();
    let blobs = blobs_of(&r);
    // Docs from a snapshot v2 reference their seeds: load them.
    r.replica.load_text_docs(&blobs).unwrap();
    let ev = evaluate_with(r.store.values(), &blobs);
    assert_eq!(r.state(), &ev.state, "replica state = evaluate");
    assert_eq!(r.conflicts(), ev.conflicts, "replica conflicts = evaluate");
    assert_eq!(r.replica.text_notices(), ev.text.notices);
    let (epoch, blob) = r.replica.text_content()[&node];
    let head = r.replica.text_doc(node, epoch).unwrap().materialize_head();
    assert_eq!(BlobId::of(&head), blob);
    String::from_utf8(head).unwrap()
}

fn ids(d: &Path) -> BTreeSet<OpId> {
    let r = Repo::open_dir_with(d.join(".hub"), LockMode::Shared).unwrap();
    r.store.keys().copied().collect()
}

/// A fake ssh (no sshd): `FAKE_NOTEXT=1` gives the server a pre-#54
/// Hello (no `text`); the client's own variable is never passed through.
fn fake_ssh(dir: &Path) -> String {
    let p = dir.join("fake-ssh");
    fs::write(
        &p,
        r#"#!/bin/sh
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift
cmd="$2"
unset HUB_TEST_NO_TEXT_FEATURE
[ "$FAKE_NOTEXT" = 1 ] && export HUB_TEST_NO_TEXT_FEATURE=1
exec sh -c "$cmd"
"#,
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&p).status().unwrap();
    p.to_string_lossy().into_owned()
}

const SEED: &str = "# Plan\n- a\n- b\n";

#[test]
fn file_sync_two_and_three_replicas_converge_and_upgrade_lazily() {
    let t = tmp("file3");
    let (a, b, c) = (repo(&t, "a"), repo(&t, "b"), repo(&t, "c"));
    let n = record_file(&a, SEED);
    ok(&b, &["sync", a.to_str().unwrap()], &[]);
    ok(&c, &["sync", a.to_str().unwrap()], &[]);
    // No text op anywhere yet: all three stay v1.
    for d in [&a, &b, &c] {
        assert_eq!(header(d), "barocss-hub/replica/v1");
    }
    // Concurrent text edits on the same seed (virtual seed, 7.4.2).
    edit(&a, n, "# Plan\n- a\n- a2\n- b\n");
    assert_eq!(header(&a), "barocss-hub/replica/v2");
    edit(&b, n, "# Plan\n- a\n- b\n- b2\n");
    edit(&c, n, "# Plan!\n- a\n- b\n");
    // Received text ops upgrade the receiver too.
    ok(&b, &["sync", a.to_str().unwrap()], &[]);
    ok(&c, &["sync", b.to_str().unwrap()], &[]);
    ok(&a, &["sync", c.to_str().unwrap()], &[]);
    ok(&b, &["sync", a.to_str().unwrap()], &[]);
    let want = "# Plan!\n- a\n- a2\n- b\n- b2\n";
    for d in [&a, &b, &c] {
        assert_eq!(header(d), "barocss-hub/replica/v2");
        assert_eq!(
            fs::read_to_string(d.join(".hub/format")).unwrap(),
            "barocss-hub/format/v2\n"
        );
        assert_eq!(text(d, n), want, "{}", d.display());
    }
    assert_eq!(ids(&a), ids(&b));
    assert_eq!(ids(&b), ids(&c));
    // Snapshot v2 written under HUB_SNAPSHOT_MIN=0, then reopened.
    ok(&a, &["status"], &[("HUB_SNAPSHOT_MIN", "0")]);
    edit(&a, n, "# Plan!\n- a\n- b\n- b2\n");
    ok(
        &a,
        &["sync", c.to_str().unwrap()],
        &[("HUB_SNAPSHOT_MIN", "0")],
    );
    let snap = fs::read(a.join(".hub/snapshot")).unwrap();
    assert!(snap.starts_with(b"barocss-hub/snapshot/v2\n"));
    assert_eq!(text(&a, n), "# Plan!\n- a\n- b\n- b2\n");
    let st = ok(&a, &["status"], &[]);
    let st2 = ok(&a, &["status"], &[("HUB_NO_SNAPSHOT", "1")]);
    assert_eq!(st, st2, "snapshot open = full replay");
}

#[test]
fn ssh_sync_converges_and_old_hello_gets_upgrade_required() {
    let t = tmp("ssh");
    let ssh = fake_ssh(&t);
    let env = [("HUB_SSH_COMMAND", ssh.as_str())];
    ok(&t, &["init", "--bare", "srv.hub"], &[]);
    let srv = t.join("srv.hub");
    let url = format!("ssh://localhost{}", srv.display());
    let (a, b) = (repo(&t, "a"), repo(&t, "b"));
    for d in [&a, &b] {
        ok(d, &["remote", "add", "--exec", BIN, "origin", &url], &[]);
    }
    let n = record_file(&a, SEED);
    ok(&a, &["sync"], &env);
    ok(&b, &["sync"], &env);
    edit(&a, n, "# Plan\n- a\n- a2\n- b\n");
    edit(&b, n, "# Plan\n- a\n- b\n- b2\n");
    // Push to a server whose Hello lacks `text`: upgrade-required, the
    // server is byte-identical (also its pull half sent nothing new).
    let srv0 = all_bytes(&srv);
    let a0 = hub_bytes(&a);
    let noenv = [("HUB_SSH_COMMAND", ssh.as_str()), ("FAKE_NOTEXT", "1")];
    assert_eq!(err_code(&a, &["sync"], &noenv), "upgrade-required");
    assert_eq!(
        all_bytes(&srv),
        srv0,
        "old-hello server persisted something"
    );
    assert_eq!(hub_bytes(&a), a0, "client persisted something");
    assert_eq!(header(&srv), "barocss-hub/replica/v1");
    // A text-aware server takes it (and becomes v2).
    ok(&a, &["sync"], &env);
    assert_eq!(header(&srv), "barocss-hub/replica/v2");
    // Pull by a client whose Hello lacks `text`: the server ends the
    // session with upgrade-required, the client persists nothing.
    let c = repo(&t, "c");
    ok(&c, &["remote", "add", "--exec", BIN, "origin", &url], &[]);
    let c0 = hub_bytes(&c);
    let cenv = [
        ("HUB_SSH_COMMAND", ssh.as_str()),
        ("HUB_TEST_NO_TEXT_FEATURE", "1"),
    ];
    assert_eq!(err_code(&c, &["sync"], &cenv), "upgrade-required");
    assert_eq!(hub_bytes(&c), c0);
    assert_eq!(header(&c), "barocss-hub/replica/v1");
    // Everyone text-aware converges.
    ok(&b, &["sync"], &env);
    ok(&a, &["sync"], &env);
    ok(&c, &["sync"], &env);
    let want = "# Plan\n- a\n- a2\n- b\n- b2\n";
    for d in [&a, &b, &c] {
        assert_eq!(text(d, n), want);
    }
    assert_eq!(ids(&a), ids(&b));
    assert_eq!(ids(&b), ids(&c));
}

/// Mixed remotes (Text-CRDT 7.9): a client with a text remote and an old
/// one; once its store holds text ops, a push to the old remote ends with
/// upgrade-required (`file://` here, the far end with an old Hello).
#[test]
fn mixed_remotes_and_file_remotes_follow_the_gate() {
    let t = tmp("mixed");
    let (a, old, new) = (repo(&t, "a"), repo(&t, "old"), repo(&t, "new"));
    let n = record_file(&a, SEED);
    let noenv = [("HUB_TEST_NO_TEXT_FEATURE", "1")];
    // Before any text op, the old side syncs normally.
    ok(&old, &["sync", a.to_str().unwrap()], &noenv);
    edit(&a, n, "# Plan\n- a\n- b\n- c\n");
    let old0 = hub_bytes(&old);
    let a0 = hub_bytes(&a);
    // Push (a -> old) and pull (old <- a), `file://`, both refused.
    assert_eq!(
        err_code(&a, &["sync", old.to_str().unwrap()], &noenv),
        "upgrade-required"
    );
    assert_eq!(
        err_code(&old, &["sync", a.to_str().unwrap()], &noenv),
        "upgrade-required"
    );
    assert_eq!(hub_bytes(&old), old0);
    assert_eq!(hub_bytes(&a), a0);
    // The text remote works at the same time.
    ok(&a, &["sync", new.to_str().unwrap()], &[]);
    assert_eq!(text(&new, n), "# Plan\n- a\n- b\n- c\n");
    // An old peer with only structural ops still syncs with `new`'s
    // non-text half? No: new's store holds text ops, so it is refused too.
    assert_eq!(
        err_code(&old, &["sync", new.to_str().unwrap()], &noenv),
        "upgrade-required"
    );
}

/// #46 digest over text ops: a copied `.hub/` whose copy authors a
/// different `TextEdit` under the shared id is detected on sync.
#[test]
fn copy_divergence_detection_covers_text_edits() {
    let t = tmp("div");
    let a = repo(&t, "a");
    let n = record_file(&a, SEED);
    edit(&a, n, "# Plan\n- a\n- b\n- base\n");
    let s = repo(&t, "s");
    ok(&s, &["sync", a.to_str().unwrap()], &[]);
    // Copy a's .hub/ (same replica id), then both author one TextEdit with
    // the same counter and different content.
    let c = t.join("c");
    fs::create_dir_all(&c).unwrap();
    assert!(Command::new("cp")
        .args(["-R", a.join(".hub").to_str().unwrap(), c.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    let oa = edit(&a, n, "# Plan\n- a\n- b\n- base\n- from a\n");
    let oc = edit(&c, n, "# Plan\n- a\n- b\n- base\n- from the copy\n");
    assert_eq!(oa.id, oc.id, "same id, different TextEdit");
    assert_ne!(oa, oc);
    ok(&s, &["sync", a.to_str().unwrap()], &[]);
    // The copy's sync with s: the digest finds the text-edit divergence.
    let o = hub(&c, &["sync", s.to_str().unwrap(), "--json"], &[]);
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success(), "copy synced: {e}");
    assert!(e.contains("duplicate-replica"), "{e}");
    assert!(!ids(&s).is_empty());
    let r = Repo::open_dir_with(s.join(".hub"), LockMode::Shared).unwrap();
    assert_eq!(r.store[&oa.id], oa, "s kept a's op, never the copy's");
}

/// Same-content text edits never look divergent (the digest is a
/// function of the op set).
#[test]
fn honest_text_histories_report_no_divergence() {
    let t = tmp("honest");
    let (a, b) = (repo(&t, "a"), repo(&t, "b"));
    let n = record_file(&a, SEED);
    ok(&b, &["sync", a.to_str().unwrap()], &[]);
    for k in 0..3 {
        edit(&a, n, &format!("{SEED}a{k}\n"));
        let tb = text_or_seed(&b, n);
        edit(&b, n, &format!("b{k}\n{tb}"));
        ok(&b, &["sync", a.to_str().unwrap()], &[]);
    }
    assert_eq!(text(&a, n), text(&b, n));
}

// ------------------------------------------------------------- format v2

/// kill -9 (abort) at every stage of the v1 -> v2 header upgrade (the
/// upgrade runs in the receiving `sync`), then rerun: same result as an
/// uninterrupted run; rerunning is idempotent.
#[test]
fn v2_upgrade_survives_a_crash_at_every_stage() {
    let t = tmp("crash");
    let a = repo(&t, "a");
    let n = record_file(&a, SEED);
    edit(&a, n, "# Plan\n- a\n- b\n- c\n");
    let clean = repo(&t, "clean");
    ok(&clean, &["sync", a.to_str().unwrap()], &[]);
    for stage in ["v2-replica-tmp", "v2-switched", "v2-format"] {
        let b = repo(&t, &format!("b-{stage}"));
        let o = hub(
            &b,
            &["sync", a.to_str().unwrap()],
            &[("HUB_TEST_CRASH_STAGE", stage)],
        );
        assert!(!o.status.success(), "{stage}: no crash");
        let h = header(&b);
        match stage {
            "v2-replica-tmp" => {
                // Before the switch: still a working v1 repository.
                assert_eq!(h, "barocss-hub/replica/v1", "{stage}");
                ok(&b, &["status"], &[]);
                assert_eq!(header(&b), "barocss-hub/replica/v1");
            }
            _ => assert_eq!(h, "barocss-hub/replica/v2", "{stage}"),
        }
        // No text op reached the log before the switch was durable.
        let log = fs::read_to_string(b.join(".hub/ops.log")).unwrap();
        assert!(!log.contains(" T "), "{stage}: T record before the switch");
        ok(&b, &["sync", a.to_str().unwrap()], &[]);
        ok(&b, &["sync", a.to_str().unwrap()], &[]);
        assert_eq!(header(&b), "barocss-hub/replica/v2");
        assert_eq!(
            fs::read_to_string(b.join(".hub/format")).unwrap(),
            "barocss-hub/format/v2\n"
        );
        assert!(!b.join(".hub/.replica.tmp").exists());
        assert_eq!(text(&b, n), text(&clean, n), "{stage}");
        assert_eq!(ids(&b), ids(&clean));
    }
    // A shared-lock open finishes a pending roll-forward too.
    let b = repo(&t, "b-shared");
    let _ = hub(
        &b,
        &["sync", a.to_str().unwrap()],
        &[("HUB_TEST_CRASH_STAGE", "v2-switched")],
    );
    ok(&b, &["status"], &[]);
    assert_eq!(
        fs::read_to_string(b.join(".hub/format")).unwrap(),
        "barocss-hub/format/v2\n"
    );
}

/// The ops-appended crash after the switch: the batch is dropped or
/// kept whole, never a partial T record.
#[test]
fn text_append_crash_stages_after_the_switch() {
    let t = tmp("append");
    let a = repo(&t, "a");
    let n = record_file(&a, SEED);
    edit(&a, n, "# Plan\n- a\n- b\n- c\n");
    for stage in [
        "blobs-written",
        "ops-appended",
        "commit-written",
        "ops-fsynced",
    ] {
        let b = repo(&t, &format!("b-{stage}"));
        let _ = hub(
            &b,
            &["sync", a.to_str().unwrap()],
            &[("HUB_TEST_CRASH_STAGE", stage)],
        );
        ok(&b, &["status"], &[]);
        ok(&b, &["sync", a.to_str().unwrap()], &[]);
        assert_eq!(text(&b, n), "# Plan\n- a\n- b\n- c\n", "{stage}");
    }
}

/// A v1 repository without text ops is never upgraded: every command
/// leaves the header and `format` at v1.
#[test]
fn v1_repo_without_text_stays_v1() {
    let t = tmp("v1");
    let (a, b) = (repo(&t, "a"), repo(&t, "b"));
    record_file(&a, SEED);
    for args in [
        vec!["status"],
        vec!["log"],
        vec!["mkdir", "d"],
        vec!["commit", "-m", "x"],
        vec!["sync", b.to_str().unwrap()],
        vec!["reid"],
        vec!["checkout"],
    ] {
        ok(&a, &args, &[("HUB_SNAPSHOT_MIN", "0")]);
        assert_eq!(header(&a), "barocss-hub/replica/v1", "{args:?}");
        assert_eq!(
            fs::read_to_string(a.join(".hub/format")).unwrap(),
            "barocss-hub/format/v1\n"
        );
        assert!(fs::read(a.join(".hub/snapshot"))
            .map(|s| s.starts_with(b"barocss-hub/snapshot/v1\n"))
            .unwrap_or(true));
    }
    assert_eq!(header(&b), "barocss-hub/replica/v1");
}

/// `reid` keeps the v2 header; a rescue in progress shows `v2-rescue`
/// (#49 marker) and restores `v2` when it completes; text ops survive.
#[test]
fn reid_and_rescue_keep_the_v2_header() {
    let t = tmp("rescue");
    let a = repo(&t, "a");
    let n = record_file(&a, SEED);
    edit(&a, n, "# Plan\n- a\n- b\n- c\n");
    ok(&a, &["reid"], &[]);
    assert_eq!(header(&a), "barocss-hub/replica/v2");
    // A rescue interrupted after its marker: v2-rescue.
    let o = hub(
        &a,
        &["reid", "--rescue", "--offline"],
        &[("HUB_TEST_CRASH_STAGE", "rescue-ops")],
    );
    let h = header(&a);
    if !o.status.success() && h.ends_with("-rescue") {
        assert_eq!(h, "barocss-hub/replica/v2-rescue");
        // Commands refuse while it is in progress; finishing restores v2.
        ok(&a, &["reid", "--rescue", "--offline"], &[]);
    }
    assert_eq!(header(&a), "barocss-hub/replica/v2");
    assert_eq!(text(&a, n), "# Plan\n- a\n- b\n- c\n");
}

// --------------------------------------------------------- old binaries

fn old_bin() -> Option<PathBuf> {
    match std::env::var_os("HUB_TEST_OLD_BIN") {
        Some(p) => Some(PathBuf::from(p)),
        None => {
            eprintln!("HUB_TEST_OLD_BIN unset: skipping");
            None
        }
    }
}

/// A.2.5-style test with the real pre-#54 develop binary: every command
/// that opens the repository refuses a v2 repository before writing
/// (`.hub/` and the working tree byte-identical), with the
/// "unsupported repository format" message.
#[test]
fn old_binary_refuses_a_v2_repository_before_writing() {
    let Some(old) = old_bin() else { return };
    let t = tmp("oldv2");
    let a = repo(&t, "a");
    let n = record_file(&a, SEED);
    edit(&a, n, "# Plan\n- a\n- b\n- c\n");
    let o2 = repo(&t, "o2");
    ok(
        &a,
        &[
            "remote",
            "add",
            "--exec",
            BIN,
            "origin",
            &format!("file://{}", o2.display()),
        ],
        &[],
    );
    // Also mid roll-forward (format still v1).
    let b = repo(&t, "b");
    let _ = hub(
        &b,
        &["sync", a.to_str().unwrap()],
        &[("HUB_TEST_CRASH_STAGE", "v2-switched")],
    );
    for d in [&a, &b] {
        let before = all_bytes(d);
        let cmds: &[&[&str]] = &[
            &["status"],
            &["ls"],
            &["log"],
            &["show", "head"],
            &["warnings"],
            &["record"],
            &["record", "-m", "x"],
            &["checkout"],
            &["checkout", "--force"],
            &["commit", "-m", "x"],
            &["publish", "x"],
            &["mkdir", "dd"],
            &["touch", "tt"],
            &["mv", "notes.md", "n2"],
            &["rm", "notes.md"],
            &["cat", "head", "notes.md"],
            &["sync"],
            &["reid"],
            &["reid", "--rescue"],
            &["init"],
            &["serve", "--stdio", "."],
        ];
        for args in cmds {
            let o = run_bin(&old, d, args, &[]);
            assert!(!o.status.success(), "old {args:?} succeeded on v2");
            assert_eq!(
                all_bytes(d),
                before,
                "old {args:?} wrote to a v2 repository"
            );
        }
        let o = run_bin(&old, d, &["status"], &[]);
        assert!(
            String::from_utf8_lossy(&o.stderr).contains("unsupported repository format"),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
}

/// The old binary over the wire: as client pulling text it gets
/// `upgrade-required` and persists nothing; as server it is refused a
/// push of text ops, nothing persisted.
#[test]
fn old_binary_peers_get_upgrade_required() {
    let Some(old) = old_bin() else { return };
    let t = tmp("oldwire");
    let ssh = fake_ssh(&t);
    let a = repo(&t, "a");
    let n = record_file(&a, SEED);
    let o = t.join("o");
    fs::create_dir_all(&o).unwrap();
    assert!(run_bin(&old, &o, &["init"], &[]).status.success());
    // Old client pulls from a new (text) server over ssh.
    let url_a = format!("ssh://localhost{}", a.display());
    assert!(run_bin(
        &old,
        &o,
        &["remote", "add", "--exec", BIN, "origin", &url_a],
        &[]
    )
    .status
    .success());
    let env = [("HUB_SSH_COMMAND", ssh.as_str())];
    assert!(
        run_bin(&old, &o, &["sync"], &env).status.success(),
        "pre-text sync"
    );
    edit(&a, n, "# Plan\n- a\n- b\n- c\n");
    let o0 = all_bytes(&o);
    let out = run_bin(&old, &o, &["sync"], &env);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("text"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(all_bytes(&o), o0, "old client persisted something");
    // New client pushes text to an old server.
    let url_o = format!("ssh://localhost{}", o.display());
    ok(
        &a,
        &[
            "remote",
            "add",
            "--exec",
            old.to_str().unwrap(),
            "old",
            &url_o,
        ],
        &[],
    );
    let a0 = hub_bytes(&a);
    assert_eq!(err_code(&a, &["sync", "old"], &env), "upgrade-required");
    assert_eq!(all_bytes(&o), o0, "old server persisted something");
    assert_eq!(hub_bytes(&a), a0);
}

/// A v1 repository without text ops: the new binary writes exactly what
/// the old one writes (same command sequence on two copies).
#[test]
fn v1_repo_is_byte_identical_under_old_and_new_binaries() {
    let Some(old) = old_bin() else { return };
    let t = tmp("v1same");
    let base = repo(&t, "base");
    record_file(&base, SEED);
    let copy = |name: &str| {
        let d = t.join(name);
        assert!(Command::new("cp")
            .args(["-R", base.to_str().unwrap(), d.to_str().unwrap()])
            .status()
            .unwrap()
            .success());
        d
    };
    let (x, y) = (copy("x"), copy("y"));
    let peer = repo(&t, "peer");
    let seq: Vec<Vec<String>> = vec![
        vec!["status".into()],
        vec!["log".into()],
        vec!["ls".into()],
        vec!["mkdir".into(), "d".into()],
        vec!["touch".into(), "d/t".into()],
        vec!["commit".into(), "-m".into(), "c1".into()],
        vec!["warnings".into()],
    ];
    for (i, args) in seq.iter().enumerate() {
        if i == 3 {
            for d in [&x, &y] {
                fs::write(d.join("notes.md"), "changed\n").unwrap();
                fs::write(d.join("new.txt"), "new\n").unwrap();
            }
            let ro = run_bin(&old, &x, &["record", "-m", "r"], &[]);
            let rn = hub(&y, &["record", "-m", "r"], &[]);
            assert!(ro.status.success() && rn.status.success());
            assert_eq!(ro.stdout, rn.stdout);
        }
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        let ro = run_bin(&old, &x, &a, &[]);
        let rn = hub(&y, &a, &[]);
        assert_eq!(ro.status.success(), rn.status.success(), "{args:?}");
        assert_eq!(ro.stdout, rn.stdout, "{args:?} output");
    }
    // Review R2 (#54): `.hub/index` file entries are
    // `f <path> <node> <blob> <size> <mtime ns>`. The last two are the
    // working file's stat cache: the mtime differs between the two copies
    // and runs, and the racy-git guard smudges an entry to `0 0` when the
    // file's mtime is within RACY_NS of the save (timing-dependent under
    // load). Only those two fields of `f` lines are masked; every other
    // byte of every `.hub/` file (index header, base, paths, node ids,
    // blob ids, and all other files) is compared exactly.
    let mask_index = |b: Vec<u8>| -> Vec<u8> {
        let t = String::from_utf8(b).unwrap();
        t.lines()
            .map(|l| match l.strip_prefix("f ") {
                Some(rest) => {
                    let f: Vec<&str> = rest.rsplitn(3, ' ').collect();
                    format!("f {} <size> <mtime>", f[2])
                }
                None => l.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes()
    };
    let strip = |d: &Path| -> BTreeMap<PathBuf, Vec<u8>> {
        all_bytes(&d.join(".hub"))
            .into_iter()
            .map(|(p, b)| {
                let rel = p.strip_prefix(d).unwrap().to_path_buf();
                let b = if rel == Path::new(".hub/index") {
                    mask_index(b)
                } else {
                    b
                };
                (rel, b)
            })
            .collect()
    };
    assert_eq!(
        strip(&x),
        strip(&y),
        ".hub differs between old and new binary"
    );
    assert_eq!(header(&y), "barocss-hub/replica/v1");
    let _ = peer;
}

// ---------------------------------------------------------- proptest

fn sync_local(a: &Path, b: &Path) {
    let mut w = Vec::new();
    hub_cli::sync::sync_local(a, b, None, &mut w).unwrap();
}

proptest! {
    // Each case runs many in-process `file://` sessions on real `.hub/`
    // directories (seconds per case): 4 by default, 8 at the merge gate
    // (PROPTEST_CASES >= 512), 12 in the deep job.
    #![proptest_config(ProptestConfig::with_cases(sync_cases()))]
    /// Two- and three-replica `file://` sync with random text edits and
    /// random sync pairs: after full rounds every replica has the same op
    /// set and the same head text, and each equals `evaluate_with`.
    #[test]
    fn text_sync_converges(nrep in 2usize..=3, steps in prop::collection::vec((0u8..3, 0u8..3, any::<u16>()), 1..10)) {
        let t = tmp("prop");
        let rs: Vec<PathBuf> = (0..nrep).map(|i| repo(&t, &format!("r{i}"))).collect();
        let n = record_file(&rs[0], SEED);
        for r in &rs[1..] {
            sync_local(r, &rs[0]);
        }
        for (who, other, k) in steps {
            let w = who as usize % nrep;
            let o = other as usize % nrep;
            if k % 3 == 0 && w != o {
                sync_local(&rs[w], &rs[o]);
            } else {
                let cur = text_or_seed(&rs[w], n);
                let mut lines: Vec<String> = cur.lines().map(|l| format!("{l}\n")).collect();
                let at = k as usize % (lines.len() + 1);
                match k % 4 {
                    0 if !lines.is_empty() => { lines.remove(at.min(lines.len() - 1)); }
                    _ => lines.insert(at, format!("r{w}-{k}\n")),
                }
                edit(&rs[w], n, &lines.concat());
            }
        }
        for _ in 0..2 {
            for i in 1..nrep {
                sync_local(&rs[i], &rs[0]);
            }
        }
        let want = text_or_seed(&rs[0], n);
        for r in &rs {
            prop_assert_eq!(ids(r), ids(&rs[0]));
            prop_assert_eq!(text_or_seed(r, n), want.clone());
            // Replica = evaluate (checked inside `text`) when text exists.
            if want != SEED {
                prop_assert_eq!(text(r, n), want.clone());
            }
        }
        let _ = fs::remove_dir_all(&t);
    }
}

fn sync_cases() -> u32 {
    if std::env::var_os("HUB_TEST_FULL_PROPTEST").is_some_and(|v| v == "1") {
        return 12;
    }
    match std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    {
        Some(n) if n >= 512 => 8,
        _ => 4,
    }
}

fn text_or_seed(d: &Path, n: NodeId) -> String {
    let mut r = Repo::open_dir_with(d.join(".hub"), LockMode::Shared).unwrap();
    let blobs = blobs_of(&r);
    r.replica.load_text_docs(&blobs).unwrap();
    match r.replica.text_content().get(&n) {
        Some(&(e, _)) => {
            String::from_utf8(r.replica.text_doc(n, e).unwrap().materialize_head()).unwrap()
        }
        None => SEED.to_string(),
    }
}

/// Regenerates `tests/fixtures/v2_text/` (a v2 repository with text ops,
/// used by the VS Code tests): `HUB_MAKE_V2_FIXTURE=<dir> cargo test
/// --test text_t2 make_v2_fixture -- --ignored`.
#[test]
#[ignore]
fn make_v2_fixture() {
    let out = PathBuf::from(std::env::var("HUB_MAKE_V2_FIXTURE").expect("HUB_MAKE_V2_FIXTURE"));
    let t = tmp("fixture");
    let a = repo(&t, "a");
    let n = record_file(&a, SEED);
    edit(&a, n, "# Plan\n- a\n- a2\n- b\n");
    assert_eq!(header(&a), "barocss-hub/replica/v2");
    let _ = fs::remove_dir_all(&out);
    fs::create_dir_all(&out).unwrap();
    assert!(Command::new("cp")
        .args([
            "-R",
            a.join(".hub").to_str().unwrap(),
            out.join("dot-hub").to_str().unwrap()
        ])
        .status()
        .unwrap()
        .success());
    fs::write(out.join("notes.md"), SEED).unwrap();
}

/// The committed v2 fixture opens with this binary (and is v2).
#[test]
fn v2_fixture_opens() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2_text");
    let t = tmp("fixopen");
    let d = t.join("w");
    fs::create_dir_all(&d).unwrap();
    assert!(Command::new("cp")
        .args([
            "-R",
            src.join("dot-hub").to_str().unwrap(),
            d.join(".hub").to_str().unwrap()
        ])
        .status()
        .unwrap()
        .success());
    assert_eq!(header(&d), "barocss-hub/replica/v2");
    ok(&d, &["status"], &[]);
    let n = node_of(&d, "notes.md");
    assert_eq!(text(&d, n), "# Plan\n- a\n- a2\n- b\n");
}

/// Issue #54 (chained saves): a 2 MiB save split by `split_text_edit`
/// into records within TEXT_OP_MAX, appended as one batch, round-trips the
/// store and syncs over `file://` and SSH.
#[test]
fn chained_save_round_trips_and_syncs() {
    let t = tmp("chain");
    let ssh = fake_ssh(&t);
    let env = [("HUB_SSH_COMMAND", ssh.as_str())];
    ok(&t, &["init", "--bare", "srv.hub"], &[]);
    let url = format!("ssh://localhost{}", t.join("srv.hub").display());
    let (a, b, c) = (repo(&t, "a"), repo(&t, "b"), repo(&t, "c"));
    let n = record_file(&a, SEED);
    let big = format!(
        "{SEED}{}{}",
        "x한🙂".repeat(200_000),
        "\nline\n".repeat(50_000)
    );
    {
        let mut r = Repo::open_dir_with(a.join(".hub"), LockMode::Exclusive).unwrap();
        let blobs = blobs_of(&r);
        let ev = evaluate_with(r.store.values(), &blobs);
        let te = author_text_edit(&r.store, &ev, &blobs, n, big.as_bytes()).unwrap();
        let first = r.prepare(Payload::TextEdit(te.clone()));
        let size = |o: &Op| hub_cli::store::encode_op(o).len();
        let chain = hub_core::text::split_text_edit(
            first.id,
            first.deps.clone(),
            te,
            &size,
            hub_cli::store::TEXT_OP_MAX,
        )
        .unwrap();
        assert!(chain.len() > 5, "{}", chain.len());
        assert!(chain.iter().all(|o| size(o) <= hub_cli::store::TEXT_OP_MAX));
        r.append_ops(chain).unwrap();
    }
    assert_eq!(text(&a, n), big);
    ok(&b, &["sync", a.to_str().unwrap()], &[]);
    assert_eq!(text(&b, n), big);
    ok(&b, &["remote", "add", "--exec", BIN, "origin", &url], &[]);
    ok(&c, &["remote", "add", "--exec", BIN, "origin", &url], &[]);
    ok(&b, &["sync"], &env);
    ok(&c, &["sync"], &env);
    assert_eq!(text(&c, n), big);
}

/// Snapshot v2 references each epoch's seed by blob id (B5): the seed
/// bytes are not in `.hub/snapshot`, opening from it equals a full
/// replay, and a missing referenced seed makes open ignore the snapshot
/// with a warning and replay `ops.log` (A.2.3 fallback).
#[test]
fn snapshot_references_seeds_and_falls_back_without_them() {
    let t = tmp("seedref");
    let a = repo(&t, "a");
    let seed = format!("{SEED}{}", "unique seed line zq\n".repeat(200));
    let n = record_file(&a, &seed);
    edit(&a, n, &format!("{seed}- edited\n"));
    let snap0 = [("HUB_SNAPSHOT_MIN", "0")];
    ok(&a, &["commit", "-m", "s"], &snap0);
    let snap = fs::read(a.join(".hub/snapshot")).unwrap();
    assert!(snap.starts_with(b"barocss-hub/snapshot/v2\n"));
    assert!(
        !snap.windows(20).any(|w| w == b"unique seed line zq\n"),
        "seed copied"
    );
    let want = ok(&a, &["status"], &[("HUB_NO_SNAPSHOT", "1")]);
    let o = hub(&a, &["status"], &[]);
    assert!(o.status.success());
    assert!(!String::from_utf8_lossy(&o.stderr).contains("ignoring"));
    assert_eq!(String::from_utf8_lossy(&o.stdout), want);
    // A later text edit loads the lazy doc from blobs/ and stays equal.
    edit(&a, n, &format!("{seed}- edited\n- again\n"));
    assert_eq!(text(&a, n), format!("{seed}- edited\n- again\n"));
    ok(&a, &["commit", "-m", "s2"], &snap0);
    // Remove the referenced seed blob: the snapshot is ignored.
    let b = BlobId::of(seed.as_bytes()).hex();
    let p = a.join(".hub/blobs").join(&b);
    let keep = fs::read(&p).unwrap();
    fs::remove_file(&p).unwrap();
    let o = hub(&a, &["status"], &[]);
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("ignoring") && e.contains(&b), "{e}");
    // Back in place (same length, damaged bytes): open trusts the
    // snapshot (existence and length), the next text op that needs the
    // seed reports corrupt, and an open after that falls back.
    fs::write(&p, vec![b'x'; keep.len()]).unwrap();
    let mut r = Repo::open_dir_with(a.join(".hub"), LockMode::Exclusive).unwrap();
    let last = *r.store.keys().max().unwrap();
    let te = hub_core::text::TextEdit {
        node: n,
        epoch: r
            .store
            .values()
            .find(|o| matches!(o.payload, Payload::SetContent { node, .. } if node == n))
            .unwrap()
            .id,
        seen: [last].into(),
        del: vec![],
        ins: vec![hub_core::text::Ins {
            left: None,
            right: None,
            bytes: b"x\n".to_vec(),
        }],
    };
    let op = r.prepare(Payload::TextEdit(te));
    let err = r.append_op(op).unwrap_err();
    assert!(err.to_string().contains(&b), "{err}");
    fs::write(&p, &keep).unwrap();
    drop(r);
    ok(&a, &["status"], &[]);
}
