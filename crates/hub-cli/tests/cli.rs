//! Issue #16: run the real `hub` binary in temp dirs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hub-cli-test-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
    ));
    let d = PathBuf::from(d.to_string_lossy().replace([' ', '{', '}', ':', ','], "_"));
    fs::create_dir_all(&d).unwrap();
    d
}

fn hub(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hub"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
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

#[test]
fn help_lists_all_commands() {
    let d = tmp("help");
    let h = ok(&d, &["--help"]);
    for c in [
        "init", "mkdir", "touch", "mv", "rm", "ls", "status", "commit", "log", "show",
    ] {
        assert!(h.contains(&format!("  {c}")), "missing {c} in help");
    }
}

#[test]
fn end_to_end_and_reopen() {
    let d = tmp("e2e");
    assert!(fail(&d, &["ls"]).contains("not a hub repository"));
    ok(&d, &["init"]);
    assert!(fail(&d, &["init"]).contains("already"));
    ok(&d, &["mkdir", "src"]);
    ok(&d, &["touch", "src/main.rs"]);
    ok(&d, &["touch", "README"]);
    let c1 = ok(&d, &["commit", "-m", "first tree"]);
    ok(&d, &["mkdir", "docs"]);
    ok(&d, &["mv", "README", "docs/README.md"]);
    ok(&d, &["touch", "junk"]);
    ok(&d, &["rm", "junk"]);
    ok(&d, &["commit", "-m", "second"]);

    assert_eq!(
        ok(&d, &["ls"]),
        "docs/\ndocs/README.md\nsrc/\nsrc/main.rs\n"
    );
    assert_eq!(ok(&d, &["ls", "src"]), "main.rs\n");
    let log = ok(&d, &["log"]);
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].ends_with(" second") && lines[1].ends_with(" first tree"));
    let id1 = lines[1].split(' ').next().unwrap();
    assert!(c1.contains(id1));
    let show = ok(&d, &["show", id1]);
    assert!(show.contains("    first tree\n"));
    assert!(show.ends_with("README\nsrc/\nsrc/main.rs\n"), "{show}");
    // Short prefix also resolves.
    assert_eq!(ok(&d, &["show", &id1[..6]]), show);

    // Reopen in a fresh process: identical ls / log / status.
    let (ls, log2, st) = (ok(&d, &["ls"]), ok(&d, &["log"]), ok(&d, &["status"]));
    assert_eq!(ls, ok(&d, &["ls"]));
    assert_eq!(log2, log);
    assert_eq!(st, ok(&d, &["status"]));
    assert!(st.contains("conflicts: none"));
    // Subdirectory discovery.
    fs::create_dir_all(d.join("sub")).unwrap();
    assert_eq!(ok(&d.join("sub"), &["log"]), log);
}

#[test]
fn clear_errors() {
    let d = tmp("errors");
    ok(&d, &["init"]);
    ok(&d, &["touch", "f"]);
    assert!(fail(&d, &["rm", "nope"]).contains("no such path"));
    assert!(fail(&d, &["touch", "f"]).contains("already exists"));
    assert!(fail(&d, &["mkdir", "f/x"]).contains("not a directory"));
    assert!(fail(&d, &["mkdir", "a/../b"]).contains("invalid path"));
    assert!(fail(&d, &["show", "zz"]).contains("invalid version id"));
    assert!(fail(&d, &["show", "ffffffffff"]).contains("unknown version"));
    ok(&d, &["mkdir", "a"]);
    ok(&d, &["mkdir", "a/b"]);
    assert!(fail(&d, &["mv", "a", "a/b/c"]).contains("into itself"));
    assert!(fail(&d, &["frob"]).contains("unknown command"));
}

#[test]
fn ambiguous_short_id_is_reported() {
    // 2000 ops written in-process (fast); every op's principal version is
    // resolvable, so a shared 4-digit prefix exists with overwhelming
    // probability (expected ~30 pairs among 16^4 prefixes).
    let d = tmp("ambig");
    ok(&d, &["init"]);
    let mut repo = hub_cli::Repo::discover(&d).unwrap();
    for i in 0..2000 {
        repo.mint(hub_core::tree::Payload::Create {
            kind: hub_core::tree::Kind::File,
            parent: hub_core::tree::ROOT,
            name: format!("f{i}"),
        })
        .unwrap();
    }
    let ix = repo.index();
    let mut by4 = std::collections::BTreeMap::<String, u32>::new();
    let mut ids = vec![hub_core::checkpoint::VersionId::of_frontier(&[])];
    ids.extend(
        repo.store
            .keys()
            .map(|o| hub_core::checkpoint::VersionId::of_frontier(&[*o])),
    );
    for id in &ids {
        *by4.entry(id.hex()[..4].to_string()).or_default() += 1;
    }
    let (p, _) = by4.iter().find(|(_, n)| **n > 1).expect("a shared prefix");
    drop(repo); // release the repo lock before spawning `hub`
    let e = fail(&d, &["show", p]);
    assert!(e.contains("ambiguous version"), "{e}");
    // Each listed candidate is displayed with a unique (>= 8 digit) prefix.
    let short = ix.short(&ids[0]);
    assert!(short.len() >= 8);
}

#[test]
fn truncated_tail_is_recovered_and_reported() {
    let d = tmp("torn");
    ok(&d, &["init"]);
    ok(&d, &["mkdir", "a"]);
    ok(&d, &["touch", "a/x"]);
    let before = ok(&d, &["ls"]);
    let log = d.join(".hub/ops.log");
    let good = fs::read(&log).unwrap();
    // Simulated crash mid-append: half a record, no newline.
    ok(&d, &["touch", "a/y"]);
    let full = fs::read(&log).unwrap();
    let torn = &full[..good.len() + (full.len() - good.len()) / 2];
    fs::write(&log, torn).unwrap();

    let o = hub(&d, &["ls"]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("recovered: dropped a truncated"));
    assert_eq!(String::from_utf8(o.stdout).unwrap(), before);
    // Truncated back to the last good record; repo is usable again.
    assert_eq!(fs::read(&log).unwrap(), good);
    // Issue #19: the index still names the dropped op, so the working
    // directory is "not at head" until a checkout re-materializes head.
    assert!(fail(&d, &["touch", "a/y"]).contains("run `hub checkout`"));
    // The index names an op the log no longer has, so it is distrusted:
    // `a/y` is treated as unrecorded and kept (never deleted as "clean").
    let o = hub(&d, &["checkout"]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("treating all working files as unrecorded"));
    assert!(d.join("a/y").exists());
    ok(&d, &["touch", "a/y"]);
    assert_eq!(ok(&d, &["ls"]), "a/\na/x\na/y\n");
    assert!(hub(&d, &["ls"]).stderr.is_empty());
}

#[test]
fn mid_log_corruption_is_an_error() {
    let d = tmp("corrupt");
    ok(&d, &["init"]);
    ok(&d, &["mkdir", "a"]);
    ok(&d, &["mkdir", "b"]);
    let log = d.join(".hub/ops.log");
    let mut s = fs::read_to_string(&log).unwrap();
    let i = s.find("C d").unwrap();
    s.replace_range(i..i + 3, "C f"); // checksum no longer matches
    fs::write(&log, &s).unwrap();
    let e = fail(&d, &["ls"]);
    assert!(e.contains("corrupt") && e.contains("line 2"), "{e}");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        s,
        "must not rewrite on error"
    );
}

/// Issue #16 repair: 40 concurrent writers must not lose or duplicate ops.
#[test]
fn parallel_writers_do_not_lose_ops() {
    const N: usize = 40;
    let d = tmp("parallel");
    ok(&d, &["init"]);
    let kids: Vec<_> = (0..N)
        .map(|i| {
            Command::new(env!("CARGO_BIN_EXE_hub"))
                .args(["mkdir", &format!("d{i}")])
                .current_dir(&d)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for k in kids {
        let o = k.wait_with_output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    let ls = ok(&d, &["ls"]);
    let mut got: Vec<&str> = ls.lines().collect();
    got.sort();
    let mut want: Vec<String> = (0..N).map(|i| format!("d{i}/")).collect();
    want.sort();
    assert_eq!(got, want);
    let log = fs::read_to_string(d.join(".hub/ops.log")).unwrap();
    let ids: Vec<&str> = log
        .lines()
        .skip(1)
        .map(|l| l.split(' ').nth(1).unwrap())
        .collect();
    let uniq: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(ids.len(), N);
    assert_eq!(uniq.len(), N, "duplicate OpIds in log");
}

fn crc8(body: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(body.as_bytes())[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Same OpId, different payload: corruption, file untouched.
#[test]
fn conflicting_duplicate_op_id_is_corruption() {
    let d = tmp("dup");
    ok(&d, &["init"]);
    ok(&d, &["mkdir", "a"]);
    let log = d.join(".hub/ops.log");
    let s = fs::read_to_string(&log).unwrap();
    let body = s.lines().nth(1).unwrap().rsplit_once('\t').unwrap().0;
    assert!(body.ends_with(" sa"));
    let other = format!("{} sb", &body[..body.len() - 3]);
    let bad = format!("{s}{other}\t{}\n", crc8(&other));
    fs::write(&log, &bad).unwrap();
    let e = fail(&d, &["ls"]);
    assert!(
        e.contains("corrupt") && e.contains("line 3") && e.contains("duplicate op id"),
        "{e}"
    );
    assert!(fail(&d, &["mkdir", "c"]).contains("duplicate op id"));
    assert_eq!(fs::read_to_string(&log).unwrap(), bad, "must not rewrite");

    // An exact duplicate record is idempotent.
    let exact = format!("{s}{body}\t{}\n", crc8(body));
    fs::write(&log, &exact).unwrap();
    assert_eq!(ok(&d, &["ls"]), "a/\n");
}

/// A crc-valid final line without '\n' is still a torn append.
#[test]
fn crc_valid_tail_without_newline_is_torn() {
    let d = tmp("nonl");
    ok(&d, &["init"]);
    ok(&d, &["mkdir", "a"]);
    let log = d.join(".hub/ops.log");
    let good = fs::read(&log).unwrap();
    ok(&d, &["mkdir", "b"]);
    let mut full = fs::read(&log).unwrap();
    assert_eq!(full.pop(), Some(b'\n'));
    fs::write(&log, &full).unwrap();
    let o = hub(&d, &["ls"]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("recovered: dropped a truncated"));
    assert_eq!(String::from_utf8(o.stdout).unwrap(), "a/\n");
    assert_eq!(fs::read(&log).unwrap(), good);
}

#[test]
fn usage_shows_arguments() {
    let d = tmp("usage");
    ok(&d, &["init"]);
    assert!(fail(&d, &["commit"]).contains("usage: hub commit -m <msg>"));
    assert!(fail(&d, &["commit", "-m", "a", "x"]).contains("usage: hub commit -m <msg>"));
    assert!(fail(&d, &["mkdir"]).contains("usage: hub mkdir <path>"));
    assert!(fail(&d, &["mv", "a"]).contains("usage: hub mv <from> <to>"));
    assert!(fail(&d, &["init", "x"]).contains("usage: hub init"));
}

#[test]
fn status_renders_content_conflicts() {
    use hub_core::tree::{BlobId, Kind, NodeId, Op, Payload};
    let d = tmp("content");
    ok(&d, &["init"]);
    ok(&d, &["touch", "a.txt"]);
    ok(&d, &["touch", "b.txt"]);
    {
        let mut repo = hub_cli::Repo::discover(&d).unwrap();
        let files: Vec<NodeId> = repo
            .state()
            .values()
            .filter(|n| n.kind == Kind::File)
            .map(|n| n.id)
            .collect();
        let (a, b) = (files[0], files[1]);
        let tip = *repo.store.keys().max().unwrap();
        let mk = |c, r, payload| Op {
            id: NodeId::new(c, r),
            deps: [tip].into_iter().collect(),
            payload,
        };
        // Two concurrent SetContents of a; concurrent edit and delete of b.
        for op in [
            mk(
                100,
                0x111,
                Payload::SetContent {
                    node: a,
                    blob: BlobId::of(b"x"),
                },
            ),
            mk(
                100,
                0x222,
                Payload::SetContent {
                    node: a,
                    blob: BlobId::of(b"y"),
                },
            ),
            mk(
                100,
                0x333,
                Payload::SetContent {
                    node: b,
                    blob: BlobId::of(b"z"),
                },
            ),
            mk(100, 0x444, Payload::Delete { node: b }),
        ] {
            repo.append_op(op).unwrap();
        }
    }
    let s = ok(&d, &["status"]);
    assert!(
        s.contains("content: /a.txt set by") || s.contains("content: a.txt set by"),
        "{s}"
    );
    assert!(s.contains("overwritten by"), "{s}");
    assert!(s.contains("edit-deleted: content edit"), "{s}");
    assert!(s.contains("kept on trashed node"), "{s}");
}
