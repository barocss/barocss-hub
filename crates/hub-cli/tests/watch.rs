//! Issue #25: `hub watch`, real binary, two repos, watchers as child
//! processes. Every watcher is owned by a [`Watcher`] guard that kills and
//! reaps it on drop (also on a panicking assertion), so no test leaves a
//! watcher running. All waits poll for convergence with generous deadlines.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(60);

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hub-watch-test-{tag}-{}-{}",
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
    Command::new(env!("CARGO_BIN_EXE_barocss-hub"))
        .env("XDG_CONFIG_HOME", xdg())
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let o = hub(dir, args);
    assert!(
        o.status.success(),
        "hub {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

struct Watcher {
    child: Option<Child>,
    log: PathBuf,
}

impl Watcher {
    fn start(dir: &Path, env: &[(&str, &str)]) -> Watcher {
        let log = dir.with_extension(format!(
            "watch-{}.log",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let f = fs::File::create(&log).unwrap();
        let mut c = Command::new(env!("CARGO_BIN_EXE_barocss-hub"));
        c.env("XDG_CONFIG_HOME", xdg());
        c.args([
            "watch",
            "--poll",
            "50",
            "--debounce",
            "300",
            "--sync",
            "500",
        ])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(f.try_clone().unwrap())
        .stderr(f);
        for (k, v) in env {
            c.env(k, v);
        }
        let child = c.spawn().unwrap();
        let w = Watcher {
            child: Some(child),
            log,
        };
        // Ready once it has announced itself.
        wait_for("watcher start", || w.output().contains("watching "));
        w
    }
    fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }
    fn output(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
    fn signal(&self, sig: &str) {
        let st = Command::new("kill")
            .args([sig, &self.pid().to_string()])
            .status()
            .unwrap();
        assert!(st.success());
    }
    /// Wait for exit; returns the status.
    fn wait_exit(&mut self) -> std::process::ExitStatus {
        let c = self.child.as_mut().unwrap();
        let t = Instant::now();
        loop {
            if let Some(s) = c.try_wait().unwrap() {
                self.child = None;
                return s;
            }
            assert!(t.elapsed() < DEADLINE, "watcher did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn kill9(&mut self) {
        let c = self.child.as_mut().unwrap();
        c.kill().unwrap();
        c.wait().unwrap();
        self.child = None;
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) -> Duration {
    let t = Instant::now();
    loop {
        if f() {
            return t.elapsed();
        }
        assert!(t.elapsed() < DEADLINE, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn read(p: &Path) -> Option<Vec<u8>> {
    fs::read(p).ok()
}

fn op_count(dir: &Path) -> usize {
    let s = ok(dir, &["status"]);
    let l = s.lines().next().unwrap();
    // "head <id> (<n> ops, ..."
    l.split('(')
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

fn clean(dir: &Path) -> bool {
    ok(dir, &["status"]).contains("working directory: clean")
}

/// Two peered repos, each with a running watcher.
fn pair(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let t = tmp(tag);
    let (a, b) = (t.join("a"), t.join("b"));
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    ok(&a, &["init"]);
    ok(&b, &["init"]);
    ok(&a, &["remote", "add", "b", "../b"]);
    ok(&b, &["remote", "add", "a", "../a"]);
    (t, a, b)
}

fn report(name: &str, d: Duration) {
    eprintln!("[timing] {name}: {} ms", d.as_millis());
}

#[test]
fn remote_commands() {
    let (t, a, b) = pair("peer");
    let l = ok(&a, &["remote", "list"]);
    assert!(l.trim_end().ends_with("/b\t(default)"), "{l}");
    assert!(ok(&a, &["status"]).contains("remotes:"));
    assert!(!hub(&a, &["remote", "add", "b2", "../b"]).status.success());
    assert!(!hub(&a, &["remote", "add", "self", "."]).status.success());
    assert!(!hub(&a, &["peer", "add", "../b"]).status.success());
    ok(&a, &["remote", "rm", "b"]);
    assert_eq!(ok(&a, &["remote", "list"]), "");
    assert!(!hub(&a, &["remote", "rm", "b"]).status.success());
    assert!(b.is_dir());
    let _ = fs::remove_dir_all(t);
}

#[test]
fn edits_renames_deletes_propagate_ignored_never() {
    let (t, a, b) = pair("prop");
    fs::write(a.join(".hubignore"), "*.log\n").unwrap();
    let _wa = Watcher::start(&a, &[]);
    let _wb = Watcher::start(&b, &[]);

    fs::write(a.join("f.txt"), b"from a").unwrap();
    fs::write(a.join("build.log"), b"ignored").unwrap();
    report(
        "A->B edit",
        wait_for("f.txt in B", || {
            read(&b.join("f.txt")).as_deref() == Some(b"from a")
        }),
    );
    fs::write(b.join("g.txt"), b"from b").unwrap();
    report(
        "B->A edit",
        wait_for("g.txt in A", || {
            read(&a.join("g.txt")).as_deref() == Some(b"from b")
        }),
    );
    fs::write(b.join("f.txt"), b"edited in b").unwrap();
    wait_for("f.txt edit in A", || {
        read(&a.join("f.txt")).as_deref() == Some(b"edited in b")
    });
    fs::create_dir(a.join("d")).unwrap();
    fs::rename(a.join("f.txt"), a.join("d/h.txt")).unwrap();
    report(
        "rename",
        wait_for("rename in B", || {
            read(&b.join("d/h.txt")).as_deref() == Some(b"edited in b") && !b.join("f.txt").exists()
        }),
    );
    fs::remove_file(b.join("g.txt")).unwrap();
    report(
        "delete",
        wait_for("delete in A", || !a.join("g.txt").exists()),
    );
    wait_for("both clean", || clean(&a) && clean(&b));
    assert!(!b.join("build.log").exists(), "ignored file propagated");
    assert!(!ok(&b, &["ls"]).contains("build.log"));
    let _ = fs::remove_dir_all(t);
}

#[test]
fn burst_debounces_and_no_feedback_loop() {
    let t = tmp("burst");
    ok(&t, &["init"]);
    let _w = Watcher::start(&t, &[]);
    // 20 saves 20 ms apart: well inside the 300 ms debounce window.
    for i in 0..20 {
        fs::write(t.join("f.txt"), format!("v{i}")).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    wait_for("recorded", || clean(&t) && op_count(&t) >= 2);
    let n = op_count(&t);
    // Create + SetContent, maybe one extra record if a save straddled a
    // scan; never one record per save.
    assert!(n <= 4, "burst produced {n} ops");
    // Quiescence: several poll, debounce and sync periods; no growth.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(op_count(&t), n, "op count grew while idle");
    let _ = fs::remove_dir_all(t);
}

#[test]
fn two_watchers_quiesce() {
    let (t, a, b) = pair("quiesce");
    let _wa = Watcher::start(&a, &[]);
    let _wb = Watcher::start(&b, &[]);
    fs::write(a.join("x"), b"1").unwrap();
    fs::write(b.join("y"), b"2").unwrap();
    wait_for("converged", || {
        a.join("y").exists() && b.join("x").exists() && clean(&a) && clean(&b)
    });
    wait_for("same op count", || op_count(&a) == op_count(&b));
    let n = op_count(&a);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!((op_count(&a), op_count(&b)), (n, n), "feedback loop");
    let _ = fs::remove_dir_all(t);
}

#[test]
fn concurrent_same_file_edits_conflict_and_converge() {
    let (t, a, b) = pair("conflict");
    fs::write(a.join("f"), b"base").unwrap();
    let _wa = Watcher::start(&a, &[]);
    let _wb = Watcher::start(&b, &[]);
    wait_for("base in B", || {
        read(&b.join("f")).as_deref() == Some(b"base")
    });
    wait_for("clean", || clean(&a) && clean(&b));
    fs::write(a.join("f"), b"edit A").unwrap();
    fs::write(b.join("f"), b"edit B").unwrap();
    report(
        "conflict converge",
        wait_for("same bytes, conflict in both", || {
            let (x, y) = (read(&a.join("f")), read(&b.join("f")));
            x.is_some()
                && x == y
                && ok(&a, &["status"]).contains("content: ")
                && ok(&b, &["status"]).contains("content: ")
        }),
    );
    // Neither edit is lost: both blobs are named by the conflict.
    for d in [&a, &b] {
        let s = ok(d, &["status"]);
        assert!(s.contains(&hex_sha256(b"edit A")[..8]), "{s}");
        assert!(s.contains(&hex_sha256(b"edit B")[..8]), "{s}");
    }
    let _ = fs::remove_dir_all(t);
}

#[test]
fn unrecorded_edit_is_never_clobbered_by_incoming_ops() {
    let (t, a, b) = pair("noclobber");
    fs::write(a.join("f"), b"base").unwrap();
    {
        let _wa = Watcher::start(&a, &[]);
        let _wb = Watcher::start(&b, &[]);
        wait_for("base in B", || {
            read(&b.join("f")).as_deref() == Some(b"base")
        });
        wait_for("clean", || clean(&a) && clean(&b));
    }
    // A's watcher is down; A edits locally (unrecorded) while B's edit is
    // delivered into A's log.
    fs::write(a.join("f"), b"unrecorded A").unwrap();
    fs::write(a.join("new"), b"only in A").unwrap();
    fs::write(b.join("f"), b"B edit").unwrap();
    ok(&b, &["record"]);
    ok(&b, &["sync", "../a"]);
    assert!(ok(&a, &["status"]).contains("behind head"));
    let _wa = Watcher::start(&a, &[]);
    let _wb = Watcher::start(&b, &[]);
    wait_for("converged with conflict", || {
        let (x, y) = (read(&a.join("f")), read(&b.join("f")));
        x.is_some()
            && x == y
            && read(&b.join("new")).as_deref() == Some(b"only in A")
            && ok(&a, &["status"]).contains("content: ")
            && clean(&a)
            && clean(&b)
    });
    // The unrecorded edit is in the log (recorded before applying): the
    // conflict names both blobs, and A's bytes are among the head bytes or
    // the conflict loser.
    let st = ok(&a, &["status"]);
    let blob_a = hex_sha256(b"unrecorded A");
    let blob_b = hex_sha256(b"B edit");
    assert!(st.contains(&blob_a[..8]), "{st}");
    assert!(st.contains(&blob_b[..8]), "{st}");
    let _ = fs::remove_dir_all(t);
}

fn hex_sha256(bytes: &[u8]) -> String {
    // The blob id is the SHA-256 of the bytes; ask the binary instead of
    // depending on a hash crate: record into a scratch repo, read `ls -l`.
    let d = tmp("sha");
    ok(&d, &["init"]);
    fs::write(d.join("x"), bytes).unwrap();
    ok(&d, &["record"]);
    let l = ok(&d, &["ls", "-l"]);
    let id = l.split_whitespace().nth(1).unwrap().to_string();
    let _ = fs::remove_dir_all(d);
    id
}

#[test]
fn sigint_exits_clean() {
    let t = tmp("sigint");
    ok(&t, &["init"]);
    let mut w = Watcher::start(&t, &[]);
    assert!(ok(&t, &["status"]).contains(&format!("watch: active (pid {})", w.pid())));
    assert!(t.join(".hub/watch.pid").exists());
    // A second watcher refuses while the first runs.
    let o = hub(&t, &["watch"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("already running"));
    fs::write(t.join("f"), b"x").unwrap();
    w.signal("-INT");
    let st = w.wait_exit();
    assert_eq!(st.code(), Some(0), "{}", w.output());
    assert!(w.output().contains("watch stopped"));
    assert!(!t.join(".hub/watch.pid").exists());
    assert!(!t.join(".hub/checkout-in-progress").exists());
    let s = ok(&t, &["status"]);
    assert!(!s.contains("watch: active"), "{s}");
    let _ = fs::remove_dir_all(t);
}

#[test]
fn sigkill_mid_activity_then_restart_recovers() {
    let (t, a, b) = pair("sigkill");
    let mut wa = Watcher::start(&a, &[]);
    let _wb = Watcher::start(&b, &[]);
    for i in 0..30 {
        fs::write(a.join(format!("f{}", i % 5)), format!("{i}")).unwrap();
        fs::write(b.join(format!("g{}", i % 5)), format!("{i}")).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        if i == 20 {
            wa.kill9();
        }
    }
    // Simulate the worst torn write: a partial record at the log tail.
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(a.join(".hub/ops.log"))
        .unwrap();
    std::io::Write::write_all(&mut log, b"O 99@torn").unwrap();
    drop(log);
    assert!(a.join(".hub/watch.pid").exists(), "stale pid file expected");
    assert!(!ok(&a, &["status"]).contains("watch: active"));
    let wa2 = Watcher::start(&a, &[]);
    report(
        "sigkill recovery",
        wait_for("converged", || {
            (0..5).all(|i| {
                read(&a.join(format!("g{i}"))).is_some()
                    && read(&a.join(format!("g{i}"))) == read(&b.join(format!("g{i}")))
                    && read(&a.join(format!("f{i}"))) == read(&b.join(format!("f{i}")))
            }) && clean(&a)
                && clean(&b)
                && op_count(&a) == op_count(&b)
        }),
    );
    assert!(!a.join(".hub/checkout-in-progress").exists());
    assert!(ok(&a, &["status"]).contains(&format!("watch: active (pid {})", wa2.pid())));
    let _ = fs::remove_dir_all(t);
}

#[test]
fn crash_mid_checkout_then_restart_resumes() {
    let (t, a, b) = pair("crashco");
    let _wa = Watcher::start(&a, &[]);
    // B's watcher aborts right after writing the first file of a checkout.
    let mut wb = Watcher::start(&b, &[("HUB_TEST_CRASH_IN_CHECKOUT", "1")]);
    fs::write(a.join("one"), b"1").unwrap();
    fs::write(a.join("two"), b"2").unwrap();
    let st = wb.wait_exit();
    assert!(!st.success());
    assert!(b.join(".hub/checkout-in-progress").exists());
    let _wb2 = Watcher::start(&b, &[]);
    wait_for("resumed", || {
        read(&b.join("one")).as_deref() == Some(b"1")
            && read(&b.join("two")).as_deref() == Some(b"2")
            && !b.join(".hub/checkout-in-progress").exists()
            && clean(&b)
    });
    let _ = fs::remove_dir_all(t);
}

#[test]
fn watch_usage_errors() {
    let t = tmp("usage");
    ok(&t, &["init"]);
    let o = hub(&t, &["watch", "--poll", "0"]);
    assert!(!o.status.success());
    let o = hub(&t, &["watch", "--bogus"]);
    assert!(!o.status.success());
    let _ = fs::remove_dir_all(t);
}

#[test]
fn checkout_old_pins_and_pauses_watch_until_plain_checkout() {
    let (t, a, b) = pair("pin");
    fs::write(a.join("notes.txt"), b"v1").unwrap();
    ok(&a, &["record", "-m", "cp1"]);
    let cp1 = ok(&a, &["log"])
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    fs::write(a.join("extra.txt"), b"x").unwrap();
    ok(&a, &["record"]);
    // `checkout <head id>` never pins.
    let head = ok(&a, &["status"])
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string();
    ok(&a, &["checkout", &head]);
    assert!(!a.join(".hub/pinned").exists());

    let wa = Watcher::start(&a, &[]);
    let _wb = Watcher::start(&b, &[]);
    wait_for("B has extra", || b.join("extra.txt").exists());
    ok(&a, &["checkout", &cp1]);
    assert!(a.join(".hub/pinned").exists());
    assert!(!a.join("extra.txt").exists());
    assert!(ok(&a, &["status"]).contains("pinned at "));
    wait_for("paused message", || wa.output().contains("paused: "));
    // An edit while pinned is not recorded, and the checkout is not undone.
    let n = op_count(&a);
    fs::write(a.join("notes.txt"), b"edited while pinned").unwrap();
    // Log keeps syncing while pinned.
    fs::write(b.join("fromb.txt"), b"b").unwrap();
    wait_for("B's op reaches A's log", || op_count(&a) > n);
    std::thread::sleep(Duration::from_secs(3));
    assert!(!a.join("extra.txt").exists(), "old checkout was undone");
    assert!(!a.join("fromb.txt").exists(), "auto-checkout while pinned");
    assert_eq!(
        read(&a.join("notes.txt")).as_deref(),
        Some(&b"edited while pinned"[..])
    );
    let st = ok(&a, &["status"]);
    assert!(st.contains("unrecorded changes"), "{st}");
    assert!(!hub(&a, &["record"]).status.success());
    // Plain checkout unpins. notes.txt is unchanged between cp1 and head,
    // so the unrecorded edit is kept (never clobbered), and the resumed
    // watcher records it at head and propagates it.
    ok(&a, &["checkout"]);
    assert!(!a.join(".hub/pinned").exists());
    assert_eq!(
        read(&a.join("notes.txt")).as_deref(),
        Some(&b"edited while pinned"[..])
    );
    wait_for("resumed", || {
        wa.output().contains("resumed: ")
            && a.join("extra.txt").exists()
            && a.join("fromb.txt").exists()
            && read(&b.join("notes.txt")).as_deref() == Some(b"edited while pinned")
            && clean(&a)
    });
    let _ = fs::remove_dir_all(t);
}

#[test]
fn peer_add_before_peer_init_is_not_self() {
    let t = tmp("peerpre");
    let (a, b) = (t.join("a"), t.join("b"));
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    ok(&a, &["init"]);
    let o = hub(&a, &["remote", "add", "b", "../b"]);
    assert!(!o.status.success());
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("no hub repository at ../b"), "{e}");
    assert!(!e.contains("own peer"), "{e}");
    let o = hub(&a, &["sync", "../b"]);
    assert!(!o.status.success());
    assert!(!String::from_utf8_lossy(&o.stderr).contains("itself"));
    ok(&b, &["init"]);
    ok(&a, &["remote", "add", "b", "../b"]);
    let _ = fs::remove_dir_all(t);
}

/// Crash a `checkout <old>` after the index save (before the pin write) or
/// after the pin write (before the marker removal): the repo must end up
/// pinned and a watcher must not record a local edit.
#[test]
fn crash_around_pin_write_still_pins() {
    for stage in ["after-index", "after-pin"] {
        let t = tmp(&format!("pincrash-{stage}"));
        ok(&t, &["init"]);
        fs::write(t.join("notes.txt"), b"v1").unwrap();
        ok(&t, &["record", "-m", "cp1"]);
        let cp1 = ok(&t, &["log"])
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        fs::write(t.join("extra.txt"), b"x").unwrap();
        ok(&t, &["record"]);
        let o = Command::new(env!("CARGO_BIN_EXE_barocss-hub"))
            .args(["checkout", &cp1])
            .current_dir(&t)
            .env("HUB_TEST_CRASH_IN_CHECKOUT", stage)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!o.status.success(), "{stage}: checkout did not crash");
        // Every crash point leaves the marker (and, after-pin, the pin).
        assert!(t.join(".hub/checkout-in-progress").exists(), "{stage}");
        assert_eq!(t.join(".hub/pinned").exists(), stage == "after-pin");
        fs::write(t.join("notes.txt"), b"edited after crash").unwrap();
        let n = op_count(&t);
        let w = Watcher::start(&t, &[]);
        wait_for("resume + pause", || {
            w.output().contains("paused: ") && !t.join(".hub/checkout-in-progress").exists()
        });
        assert!(t.join(".hub/pinned").exists(), "{stage}: not pinned");
        std::thread::sleep(Duration::from_secs(3));
        assert_eq!(op_count(&t), n, "{stage}: edit recorded while pinned");
        assert!(ok(&t, &["status"]).contains("pinned at "));
        drop(w);
        // Plain checkout clears it.
        ok(&t, &["checkout"]);
        assert!(!t.join(".hub/pinned").exists(), "{stage}");
        let _ = fs::remove_dir_all(t);
    }
}

/// Issue #45: a private trusted-remotes store per test process.
fn xdg() -> String {
    static D: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    D.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("hub45-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into_owned()
    })
    .clone()
}
