//! Issue #35: the rename to `barocss-hub` changes no on-disk format. The
//! fixture `tests/fixtures/pre_rename_dae42dd/` was made by the pre-rename
//! `hub` binary built from develop dae42dd (`init`, `record -m first`,
//! `record -m second`, `publish first`, `HUB_SNAPSHOT_MIN=0 record -m third`;
//! its `.hub/` is stored as `dot-hub/`). The renamed binary must open it,
//! read it, write to it and sync it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hub-prerename-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let name = e.file_name();
        let dst = to.join(if name == "dot-hub" {
            ".hub".into()
        } else {
            name
        });
        if e.file_type().unwrap().is_dir() {
            copy(&e.path(), &dst);
        } else {
            fs::copy(e.path(), &dst).unwrap();
        }
    }
}

fn hub(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_barocss-hub"))
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .env_remove("HUB_SNAPSHOT_MIN")
        .env_remove("HUB_NO_SNAPSHOT")
        .output()
        .unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let o = hub(dir, args);
    assert!(
        o.status.success(),
        "barocss-hub {args:?} failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

#[test]
fn repo_made_by_pre_rename_binary_opens() {
    let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pre_rename_dae42dd");
    let d = tmp("open");
    copy(&fx, &d);
    assert_eq!(
        fs::read_to_string(d.join(".hub/format")).unwrap(),
        "barocss-hub/format/v1\n"
    );

    assert_eq!(
        ok(&d, &["log"]),
        "25542139 third\nd16f9481 second\n37e797c2 [published] first\n"
    );
    let st = ok(&d, &["status"]);
    assert!(
        st.contains("head 25542139 (8 ops, 3 checkpoints, 1 published)"),
        "{st}"
    );
    assert!(st.contains("working directory: clean"), "{st}");
    assert_eq!(ok(&d, &["ls"]), "README\nnotes.txt\nsrc/\nsrc/main.rs\n");
    assert!(ok(&d, &["show", "37e797c2"]).contains("README"));

    // Writes still work and the repo reopens.
    fs::write(d.join("new.txt"), "post-rename\n").unwrap();
    assert!(ok(&d, &["record", "-m", "fourth"]).contains("added:    new.txt"));
    let log = ok(&d, &["log"]);
    assert!(log.lines().next().unwrap().ends_with(" fourth"), "{log}");
    assert!(ok(&d, &["status"]).contains(" 4 checkpoints, 1 published)"));

    // A fresh repo syncs with it and gets the same tree.
    let b = tmp("peer");
    ok(&b, &["init"]);
    ok(&b, &["sync", d.to_str().unwrap()]);
    ok(&b, &["checkout"]);
    assert_eq!(ok(&b, &["ls"]), ok(&d, &["ls"]));
    assert_eq!(
        fs::read_to_string(b.join("README")).unwrap(),
        "hello\nmore\n"
    );
    assert!(ok(&b, &["log"]).contains("[published] first"));

    let _ = fs::remove_dir_all(&d);
    let _ = fs::remove_dir_all(&b);
}
