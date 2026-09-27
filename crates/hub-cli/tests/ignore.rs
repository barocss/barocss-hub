//! Issue #24: `.hubignore`, real `hub` binary. Every test runs in its own
//! temp dir.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hub-ignore-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
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

fn init(tag: &str) -> PathBuf {
    let d = tmp(tag);
    ok(&d, &["init"]);
    d
}

fn write(d: &Path, p: &str, s: &str) {
    let f = d.join(p);
    fs::create_dir_all(f.parent().unwrap()).unwrap();
    fs::write(f, s).unwrap();
}

fn tree(d: &Path) -> String {
    ok(d, &["ls"])
}

#[test]
fn target_log_and_negation() {
    let d = init("neg");
    write(&d, ".hubignore", "# build\ntarget/\n*.log\n!keep.log\n");
    write(&d, "target/debug/app", "bin");
    write(&d, "a.log", "x");
    write(&d, "sub/b.log", "x");
    write(&d, "keep.log", "k");
    write(&d, "src/main.rs", "fn main(){}");
    let out = ok(&d, &["record", "-m", "v1"]);
    assert!(out.contains("added:    keep.log"), "{out}");
    assert!(out.contains("added:    .hubignore"), "{out}");
    assert!(!out.contains("target"), "{out}");
    assert!(!out.contains("a.log") && !out.contains("b.log"), "{out}");
    let t = tree(&d);
    assert!(t.contains("keep.log") && t.contains("main.rs"), "{t}");
    assert!(!t.contains("target") && !t.contains("a.log"), "{t}");
    let st = ok(&d, &["status"]);
    assert!(st.contains("working directory: clean"), "{st}");
    assert!(!st.contains("a.log"), "{st}");
}

#[test]
fn node_modules_at_any_depth() {
    let d = init("nm");
    write(&d, ".hubignore", "**/node_modules/\n");
    write(&d, "node_modules/x/index.js", "1");
    write(&d, "web/app/node_modules/y.js", "1");
    write(&d, "web/app/main.js", "1");
    write(&d, "node_modules_not/z", "1"); // name differs: not ignored
    ok(&d, &["record", "-m", "v1"]);
    let t = tree(&d);
    assert!(
        t.contains("main.js") && t.contains("node_modules_not"),
        "{t}"
    );
    assert!(!t.contains("index.js") && !t.contains("y.js"), "{t}");
    let st = ok(&d, &["status", "--ignored"]);
    assert!(st.contains("ignored:\n"), "{st}");
    assert!(st.contains("  node_modules/\n"), "{st}");
    assert!(st.contains("  web/app/node_modules/\n"), "{st}");
    assert!(!st.contains("index.js"), "top-most only: {st}");
}

#[test]
fn hubignore_is_synced_and_applies_on_the_other_replica() {
    let a = init("sync-a");
    let b = init("sync-b");
    write(&a, ".hubignore", "*.tmp\n");
    write(&a, "f.txt", "f");
    ok(&a, &["record", "-m", "v1"]);
    ok(&b, &["sync", a.to_str().unwrap()]);
    ok(&b, &["checkout"]);
    assert_eq!(fs::read_to_string(b.join(".hubignore")).unwrap(), "*.tmp\n");
    write(&b, "scratch.tmp", "junk");
    write(&b, "g.txt", "g");
    let out = ok(&b, &["record", "-m", "v2"]);
    assert!(
        out.contains("g.txt") && !out.contains("scratch.tmp"),
        "{out}"
    );
    ok(&a, &["sync", b.to_str().unwrap()]);
    ok(&a, &["checkout"]);
    assert!(a.join("g.txt").exists());
    assert!(!a.join("scratch.tmp").exists());
}

#[test]
fn tracked_file_that_becomes_ignored_stays_tracked() {
    let d = init("tracked");
    write(&d, "gen.out", "v1");
    ok(&d, &["record", "-m", "v1"]);
    write(&d, ".hubignore", "*.out\n");
    write(&d, "gen.out", "v2");
    write(&d, "new.out", "n");
    let st = ok(&d, &["status"]);
    assert!(st.contains("tracked but ignored"), "{st}");
    assert!(st.contains("  gen.out\n"), "{st}");
    assert!(st.contains("modified: gen.out"), "{st}");
    assert!(!st.contains("new.out"), "{st}");
    let out = ok(&d, &["record", "-m", "v2"]);
    assert!(out.contains("modified: gen.out"), "{out}");
    assert!(!out.contains("new.out"), "{out}");
    // Deleting it is still recorded (not silently dropped).
    fs::remove_file(d.join("gen.out")).unwrap();
    let out = ok(&d, &["record", "-m", "v3"]);
    assert!(out.contains("deleted:  gen.out"), "{out}");
}

#[test]
fn status_ignored_lists_ignored_files() {
    let d = init("stat");
    let st = ok(&d, &["status", "--ignored"]);
    assert!(st.contains("ignored: none"), "{st}");
    write(&d, ".hubignore", "*.log\nbuild/\n");
    write(&d, "x.log", "1");
    write(&d, "build/o", "1");
    write(&d, "a", "1");
    let st = ok(&d, &["status"]);
    assert!(!st.contains("x.log") && !st.contains("build"), "{st}");
    let st = ok(&d, &["status", "--ignored"]);
    assert!(
        st.contains("  x.log\n") && st.contains("  build/\n"),
        "{st}"
    );
    assert!(st.contains("added:    a"), "{st}");
}

#[test]
fn checkout_never_deletes_ignored_untracked_files() {
    let d = init("co");
    write(&d, ".hubignore", "*.log\n");
    write(&d, "dir/a.txt", "a");
    ok(&d, &["record", "-m", "v1"]);
    let log = ok(&d, &["log"]);
    let v1 = log.split_whitespace().next().unwrap().to_string();
    // A later version replaces directory `dir` with a file `dir`.
    fs::remove_dir_all(d.join("dir")).unwrap();
    write(&d, "dir", "now a file");
    ok(&d, &["record", "-m", "v2"]);
    // Back at v1, an ignored log appears inside dir/.
    ok(&d, &["checkout", &v1]);
    write(&d, "dir/debug.log", "precious");
    // Going to head replaces dir/ with a file: dir/ holds an ignored file.
    let e = fail(&d, &["checkout"]);
    assert!(e.contains("dir/debug.log"), "{e}");
    let e = fail(&d, &["checkout", "--force"]);
    assert!(e.contains("never deleted, even with --force"), "{e}");
    assert_eq!(
        fs::read_to_string(d.join("dir/debug.log")).unwrap(),
        "precious"
    );
    // Moving it away unblocks the checkout.
    fs::rename(d.join("dir/debug.log"), d.join("moved.log")).unwrap();
    ok(&d, &["checkout"]);
    assert_eq!(fs::read_to_string(d.join("moved.log")).unwrap(), "precious");
}

#[test]
fn checkout_refuses_ignored_file_at_a_target_path() {
    // Replica b ignores *.bin locally only after a has tracked one.
    let a = init("tp-a");
    let b = init("tp-b");
    write(&b, ".hubignore", "*.bin\n");
    ok(&b, &["record", "-m", "ign"]);
    ok(&a, &["sync", b.to_str().unwrap()]);
    ok(&a, &["checkout"]);
    write(&a, "blob.bin", "tracked in a");
    fs::write(a.join(".hubignore"), "").unwrap();
    ok(&a, &["record", "-m", "track"]);
    write(&b, "blob.bin", "b's local ignored file");
    ok(&b, &["sync", a.to_str().unwrap()]);
    let e = fail(&b, &["checkout", "--force"]);
    assert!(e.contains("ignored untracked 'blob.bin'"), "{e}");
    assert_eq!(
        fs::read_to_string(b.join("blob.bin")).unwrap(),
        "b's local ignored file"
    );
}

#[test]
fn removing_a_tracked_dir_keeps_its_ignored_contents() {
    let d = init("rmdir");
    write(&d, ".hubignore", "*.log\n");
    write(&d, "w/a", "a");
    ok(&d, &["record", "-m", "v1"]);
    write(&d, "w/x.log", "keep me");
    ok(&d, &["rm", "w"]);
    assert_eq!(fs::read_to_string(d.join("w/x.log")).unwrap(), "keep me");
    assert!(!d.join("w/a").exists());
}

#[test]
fn file_replacing_a_tracked_dir_refuses_on_untracked_leftovers() {
    let d = init("leftover");
    write(&d, "dir/a.txt", "a");
    ok(&d, &["record", "-m", "v1"]);
    let log = ok(&d, &["log"]);
    let v1 = log.split_whitespace().next().unwrap().to_string();
    fs::remove_dir_all(d.join("dir")).unwrap();
    write(&d, "dir", "file");
    ok(&d, &["record", "-m", "v2"]);
    ok(&d, &["checkout", &v1]);
    write(&d, "dir/new.txt", "unrecorded");
    let e = fail(&d, &["checkout"]);
    assert!(
        e.contains("untracked 'dir/new.txt' is inside directory 'dir'"),
        "{e}"
    );
    assert!(!d.join(".hub/checkout-in-progress").exists());
    // --force discards the (non-ignored) leftover, as for any clobber.
    ok(&d, &["checkout", "--force"]);
    assert_eq!(fs::read_to_string(d.join("dir")).unwrap(), "file");
}
