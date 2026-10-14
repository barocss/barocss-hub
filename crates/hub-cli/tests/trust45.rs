//! Issue #45: planted remotes and first contact. A `.hub/remotes` that came
//! with a folder must not make `sync` or `watch` contact its ssh remote
//! silently. Every test uses a private `BAROCSS_HUB_CONFIG_DIR` (or `HOME`), never
//! the user's real configuration.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_barocss-hub");

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "hub45t-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    fs::canonicalize(p).unwrap()
}

struct Env {
    t: PathBuf,
    ssh: String,
    log: PathBuf,
    xdg: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Env {
        let t = tmp(tag);
        let ssh = t.join("fake-ssh");
        fs::write(
            &ssh,
            r#"#!/bin/sh
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift
printf '%s\n' "$1" >> "$FAKE_SSH_LOG"
exec sh -c "$2"
"#,
        )
        .unwrap();
        Command::new("chmod").arg("+x").arg(&ssh).status().unwrap();
        let xdg = t.join("xdg");
        Env {
            ssh: ssh.to_string_lossy().into_owned(),
            log: t.join("ssh.log"),
            xdg,
            t,
        }
    }
    fn cmd(&self, dir: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .current_dir(dir)
            .env("HUB_SSH_COMMAND", &self.ssh)
            .env("FAKE_SSH_LOG", &self.log)
            .env("XDG_CONFIG_HOME", &self.xdg)
            .env(
                "BAROCSS_HUB_CONFIG_DIR",
                std::path::Path::new(&self.xdg).join("barocss-hub"),
            )
            .env_remove("BAROCSS_HUB_SERVE_ROOT")
            .stdin(Stdio::null());
        c
    }
    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.cmd(dir, args).output().unwrap()
    }
    fn ok(&self, dir: &Path, args: &[&str]) -> String {
        let o = self.run(dir, args);
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap()
    }
    fn repo(&self, name: &str) -> PathBuf {
        let d = self.t.join(name);
        fs::create_dir_all(&d).unwrap();
        self.ok(&d, &["init"]);
        d
    }
    fn contacts(&self) -> usize {
        fs::read_to_string(&self.log)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }
    fn store(&self) -> PathBuf {
        self.xdg.join("barocss-hub/trusted-remotes")
    }
}

fn plant(repo: &Path, url: &str, exec: &str) {
    fs::write(
        repo.join(".hub/remotes"),
        format!("barocss-hub/remotes/v0\nR sorigin s{url} exec={exec} default=1\n"),
    )
    .unwrap();
}

/// A planted repository (a folder "someone gave you") and a real server.
fn planted(e: &Env) -> (PathBuf, PathBuf, String) {
    let srv = e.t.join("srv");
    e.ok(&e.t, &["init", "--bare", "srv"]);
    let a = e.repo("a");
    fs::write(a.join("f.txt"), "secret\n").unwrap();
    e.ok(&a, &["record"]);
    let url = format!("ssh://localhost{}", srv.display());
    plant(&a, &url, BIN);
    (a, srv, url)
}

#[test]
fn planted_remote_is_refused_without_a_terminal_and_never_contacted() {
    let e = Env::new("refuse");
    let (a, srv, _) = planted(&e);
    for args in [&["sync"][..], &["sync", "origin"], &["sync", "--json"]] {
        let o = e.run(&a, args);
        assert_eq!(o.status.code(), Some(1), "{args:?}");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(
            err.contains("untrusted-remote") || err.contains("never added or confirmed"),
            "{err}"
        );
        assert!(err.contains("remote trust origin"), "{err}");
    }
    let j = String::from_utf8(e.run(&a, &["sync", "--json"]).stderr).unwrap();
    assert!(j.contains("\"code\":\"untrusted-remote\""), "{j}");
    assert_eq!(e.contacts(), 0, "the planted host was contacted");
    assert!(!e.store().exists());
    // Nothing reached the server.
    assert!(!e.ok(&srv, &["ls"]).contains("f.txt"));
}

#[test]
fn yes_trusts_records_and_later_syncs_need_no_flag() {
    let e = Env::new("yes");
    let (a, _, url) = planted(&e);
    let s = e.ok(&a, &["sync", "--yes"]);
    assert!(s.contains("synced with"), "{s}");
    assert_eq!(e.contacts(), 1);
    let st = fs::read_to_string(e.store()).unwrap();
    assert!(st.starts_with("barocss-hub/trusted-remotes/v1\n"), "{st}");
    let local = fs::canonicalize(a.join(".hub")).unwrap();
    assert!(
        st.contains(&format!("T s{} s{url} s{BIN}", local.display())),
        "{st}"
    );
    e.ok(&a, &["sync"]);
    let j = e.ok(&a, &["sync", "-y", "origin", "--json"]);
    assert!(
        j.contains("\"rejected\":[],\"rejected_reported\":true"),
        "{j}"
    );
    assert_eq!(e.contacts(), 3);
    // The store lives outside the repository.
    assert!(!fs::read_dir(a.join(".hub")).unwrap().any(|x| x
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("trust")));
}

#[test]
fn changing_url_or_exec_by_hand_is_a_new_remote() {
    let e = Env::new("key");
    let (a, _, url) = planted(&e);
    e.ok(&a, &["remote", "trust", "origin"]);
    e.ok(&a, &["sync"]);
    // Another exec on the same URL: untrusted again.
    let alias = e.t.join("bin");
    fs::create_dir_all(&alias).unwrap();
    std::os::unix::fs::symlink(BIN, alias.join("hub")).unwrap();
    let other = alias.join("hub").to_string_lossy().into_owned();
    plant(&a, &url, &other);
    let o = e.run(&a, &["sync"]);
    assert_eq!(o.status.code(), Some(1));
    // Another host, user, port or path: untrusted again.
    for u in [
        url.replace("localhost", "127.0.0.1"),
        url.replace("ssh://", "ssh://me@"),
        url.replace("localhost", "localhost:22"),
        format!("{url}/x"),
    ] {
        plant(&a, &u, BIN);
        let o = e.run(&a, &["sync", "--json"]);
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("untrusted-remote"), "{u}: {err}");
    }
    assert_eq!(e.contacts(), 1);
}

#[test]
fn remote_add_and_set_url_are_trusted_by_the_user() {
    let e = Env::new("add");
    let srv = e.t.join("srv");
    e.ok(&e.t, &["init", "--bare", "srv"]);
    let srv2 = e.t.join("srv2");
    e.ok(&e.t, &["init", "--bare", "srv2"]);
    let a = e.repo("a");
    let url = format!("ssh://localhost{}", srv.display());
    e.ok(&a, &["remote", "add", "--exec", BIN, "origin", &url]);
    e.ok(&a, &["sync"]);
    let url2 = format!("localhost:{}", srv2.display());
    e.ok(&a, &["remote", "set-url", "origin", &url2]);
    e.ok(&a, &["sync"]);
    assert_eq!(e.contacts(), 2);
    // The user's own `remote add` of a local repository: no prompt.
    let b = e.repo("b");
    e.ok(&a, &["remote", "add", "b", b.to_str().unwrap()]);
    e.ok(&a, &["sync", "b"]);
}

/// Every byte under `d` (the planted-victim checks).
fn tree(d: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    let mut m = std::collections::BTreeMap::new();
    let mut st = vec![d.to_path_buf()];
    while let Some(x) = st.pop() {
        for en in fs::read_dir(&x).unwrap() {
            let p = en.unwrap().path();
            if p.is_dir() {
                st.push(p);
            } else {
                m.insert(p.clone(), fs::read(&p).unwrap());
            }
        }
    }
    m
}

/// #45 repair R1: a planted `file://` remote naming another of the user's
/// repositories is refused: nothing pulled, nothing pushed.
#[test]
fn planted_file_remote_is_refused_and_both_repos_are_untouched() {
    let e = Env::new("pfile");
    let victim = e.repo("victim");
    fs::write(victim.join("secret.txt"), "victim secret\n").unwrap();
    e.ok(&victim, &["record"]);
    let planted = e.repo("planted");
    fs::write(planted.join("evil.txt"), "planted\n").unwrap();
    e.ok(&planted, &["record"]);
    // Through a symlink too: the key is the canonical target.
    let link = e.t.join("link-to-victim");
    std::os::unix::fs::symlink(&victim, &link).unwrap();
    for target in [victim.clone(), link.clone()] {
        fs::write(
            planted.join(".hub/remotes"),
            format!(
                "barocss-hub/remotes/v0\nR sv sfile://{} default=1\n",
                target.display()
            ),
        )
        .unwrap();
        let (tv, tp) = (tree(&victim), tree(&planted));
        for args in [&["sync"][..], &["sync", "v"], &["sync", "--json"]] {
            let o = e.run(&planted, args);
            assert_eq!(o.status.code(), Some(1), "{args:?}");
            assert!(
                String::from_utf8_lossy(&o.stderr).contains("never added or confirmed"),
                "{}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
        let j = String::from_utf8(e.run(&planted, &["sync", "--json"]).stderr).unwrap();
        assert!(j.contains("\"code\":\"untrusted-remote\""), "{j}");
        assert_eq!(tree(&victim), tv, "victim changed");
        assert_eq!(tree(&planted), tp, "planted folder changed");
    }
    // Trusting through the symlink trusts the canonical target: the direct
    // path then works too.
    e.ok(&planted, &["remote", "trust", "v"]);
    fs::write(
        planted.join(".hub/remotes"),
        format!(
            "barocss-hub/remotes/v0\nR sv sfile://{} default=1\n",
            victim.display()
        ),
    )
    .unwrap();
    e.ok(&planted, &["sync"]);
    // A missing target: refused as untrusted, never created.
    let gone = e.t.join("gone");
    fs::write(
        planted.join(".hub/remotes"),
        format!(
            "barocss-hub/remotes/v0\nR sg sfile://{} default=1\n",
            gone.display()
        ),
    )
    .unwrap();
    let o = e.run(&planted, &["sync", "--json"]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("untrusted-remote"));
    assert!(!gone.exists());
}

/// #45 repair R2: the local repository is part of the key. A planted
/// folder carrying the exact URL of a trusted team server is refused.
#[test]
fn planted_copy_of_a_trusted_url_in_another_folder_is_refused() {
    let e = Env::new("copyurl");
    e.ok(&e.t, &["init", "--bare", "srv"]);
    let url = format!("ssh://localhost{}", e.t.join("srv").display());
    let mine = e.repo("mine");
    e.ok(&mine, &["remote", "add", "--exec", BIN, "origin", &url]);
    e.ok(&mine, &["sync"]);
    let other = e.repo("other");
    plant(&other, &url, BIN);
    let o = e.run(&other, &["sync", "--json"]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("untrusted-remote"));
    assert_eq!(e.contacts(), 1);
    // A v0 store (first #45 round, no local path) matches nothing.
    fs::write(
        e.store(),
        format!("barocss-hub/trusted-remotes/v0\nT s{url} s{BIN}\n"),
    )
    .unwrap();
    let o = e.run(&mine, &["sync", "--json"]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("untrusted-remote"));
    e.ok(&mine, &["remote", "trust", "origin"]);
    assert!(fs::read_to_string(e.store())
        .unwrap()
        .starts_with("barocss-hub/trusted-remotes/v1\n"));
    e.ok(&mine, &["sync"]);
}

/// The peers -> remotes migration grants no trust.
#[test]
fn migrated_peers_start_untrusted() {
    let e = Env::new("mig");
    let a = e.repo("a");
    let b = e.repo("b");
    let root = fs::canonicalize(&b).unwrap();
    fs::write(
        a.join(".hub/peers"),
        format!("barocss-hub/peers/v1\n{}\n", root.display()),
    )
    .unwrap();
    // Migrate (an exclusive-lock command), then sync by name: refused.
    let _ = e.run(&a, &["remote", "rm", "none"]);
    assert!(!a.join(".hub/peers").exists(), "not migrated");
    let o = e.run(&a, &["sync", "--json", "b"]);
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("untrusted-remote"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    e.ok(&a, &["sync", "--yes", "b"]);
    e.ok(&a, &["sync", "b"]);
}

/// `watch` skips an untrusted `file://` remote with one event.
#[test]
fn watch_skips_untrusted_file_remote() {
    let e = Env::new("wfile");
    let victim = e.repo("victim");
    let a = e.repo("a");
    fs::write(
        a.join(".hub/remotes"),
        format!(
            "barocss-hub/remotes/v0\nR sv sfile://{} default=1\n",
            victim.display()
        ),
    )
    .unwrap();
    let tv = tree(&victim);
    let w = e
        .cmd(
            &a,
            &[
                "watch",
                "--json",
                "--poll",
                "50",
                "--debounce",
                "50",
                "--sync",
                "150",
            ],
        )
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(w.id().to_string())
        .status();
    let out = w.wait_with_output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        s.matches("\"event\":\"untrusted-remote\"").count(),
        1,
        "{s}"
    );
    assert!(!s.contains("\"event\":\"error\""), "{s}");
    assert_eq!(tree(&victim), tv);
}

/// D-046: where the store lives without the test override.
#[test]
fn store_location_per_platform_and_macos_migration() {
    let e = Env::new("home");
    let (a, _, _) = planted(&e);
    let home = e.t.join("home");
    let xdg = e.t.join("xdg2");
    fs::create_dir_all(&home).unwrap();
    let cmd = |args: &[&str], xdg_v: Option<&Path>| {
        let mut c = e.cmd(&a, args);
        c.env_remove("BAROCSS_HUB_CONFIG_DIR")
            .env_remove("XDG_CONFIG_HOME")
            .env("HOME", &home);
        if let Some(x) = xdg_v {
            c.env("XDG_CONFIG_HOME", x);
        }
        c.output().unwrap()
    };
    let mac = home.join("Library/Application Support/barocss-hub/trusted-remotes");
    let dotcfg = home.join(".config/barocss-hub/trusted-remotes");
    let xdgf = xdg.join("barocss-hub/trusted-remotes");
    assert!(cmd(&["sync", "--yes"], Some(&xdg)).status.success());
    if cfg!(target_os = "macos") {
        // XDG_CONFIG_HOME is not consulted on macOS.
        assert!(mac.is_file());
        assert!(!xdgf.exists() && !dotcfg.exists());
        // Migration: a pre-D-046 ~/.config store is read, then moved.
        let entry = fs::read_to_string(&mac).unwrap();
        fs::remove_file(&mac).unwrap();
        fs::create_dir_all(dotcfg.parent().unwrap()).unwrap();
        fs::write(&dotcfg, &entry).unwrap();
        assert!(
            cmd(&["sync"], None).status.success(),
            "legacy entry not read"
        );
        assert!(dotcfg.is_file(), "a read must not move the file");
        assert!(cmd(&["remote", "trust", "origin"], None).status.success());
        assert!(!dotcfg.exists(), "not moved");
        assert_eq!(fs::read_to_string(&mac).unwrap(), entry);
        assert!(cmd(&["sync"], None).status.success());
    } else {
        assert!(xdgf.is_file());
        assert!(!mac.exists());
        for x in [None, Some(Path::new("relative/dir"))] {
            let _ = fs::remove_file(&dotcfg);
            assert!(cmd(&["sync", "--yes"], x).status.success());
            assert!(dotcfg.is_file());
        }
    }
    assert!(!e.store().exists());
    // No usable variable: refused (nothing to remember it in) unless
    // --yes, which still syncs.
    let mut c = e.cmd(&a, &["sync", "--json"]);
    c.env_remove("BAROCSS_HUB_CONFIG_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HOME");
    let o = c.output().unwrap();
    assert!(String::from_utf8_lossy(&o.stderr).contains("untrusted-remote"));
    let mut c = e.cmd(&a, &["sync", "--yes"]);
    c.env_remove("BAROCSS_HUB_CONFIG_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HOME");
    assert!(c.output().unwrap().status.success());
}

/// On a terminal the CLI asks (`script` gives the child a pty).
#[test]
fn terminal_prompt_asks_on_first_contact() {
    if !Path::new("/usr/bin/script").exists() {
        eprintln!("no script(1): skipping");
        return;
    }
    let e = Env::new("tty");
    let (a, _, _) = planted(&e);
    let run = |answer: &str| -> Output {
        let mut c = Command::new("/usr/bin/script");
        if cfg!(target_os = "macos") {
            c.args(["-q", "/dev/null", BIN, "sync"]);
        } else {
            c.args(["-qec", &format!("{BIN} sync"), "/dev/null"]);
        }
        let mut ch = c
            .current_dir(&a)
            .env("HUB_SSH_COMMAND", &e.ssh)
            .env("FAKE_SSH_LOG", &e.log)
            .env("XDG_CONFIG_HOME", &e.xdg)
            .env(
                "BAROCSS_HUB_CONFIG_DIR",
                std::path::Path::new(&e.xdg).join("barocss-hub"),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        let mut i = ch.stdin.take().unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let _ = i.write_all(answer.as_bytes());
        std::thread::sleep(Duration::from_millis(1500));
        drop(i);
        ch.wait_with_output().unwrap()
    };
    let o = run("n\n");
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(out.contains("Trust it and connect now? [y/N]"), "{out}");
    assert_eq!(e.contacts(), 0, "{out}");
    let o = run("y\n");
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(out.contains("synced with"), "{out}");
    assert_eq!(e.contacts(), 1);
    assert!(e.store().is_file());
}

/// `watch` skips an untrusted remote with one event (no error loop), keeps
/// running, and picks it up once trusted.
#[test]
fn watch_skips_untrusted_remote_with_one_event() {
    let e = Env::new("watch");
    let (a, _, _) = planted(&e);
    let mut w = e
        .cmd(
            &a,
            &[
                "watch",
                "--json",
                "--poll",
                "50",
                "--debounce",
                "50",
                "--sync",
                "150",
            ],
        )
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let out = w.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    let mut lines = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(2000) {
        if let Ok(l) = rx.recv_timeout(Duration::from_millis(100)) {
            lines.push(l);
        }
    }
    let untrusted = lines
        .iter()
        .filter(|l| l.contains("\"event\":\"untrusted-remote\""))
        .count();
    assert_eq!(untrusted, 1, "{lines:#?}");
    assert!(
        !lines.iter().any(|l| l.contains("\"event\":\"error\"")),
        "{lines:#?}"
    );
    assert!(lines[0].contains("\"sync\":true"), "{}", lines[0]);
    assert_eq!(e.contacts(), 0);
    // Trusted from another terminal: the running watcher syncs.
    e.ok(&a, &["remote", "trust", "origin"]);
    let t0 = Instant::now();
    let mut synced = false;
    while t0.elapsed() < Duration::from_secs(15) && !synced {
        if let Ok(l) = rx.recv_timeout(Duration::from_millis(100)) {
            synced = l.contains("\"event\":\"synced\"");
            lines.push(l);
        }
    }
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(w.id().to_string())
        .status();
    let _ = w.wait();
    assert!(synced, "{lines:#?}");
    assert!(e.contacts() >= 1);
}

#[test]
fn watch_no_sync_contacts_nothing() {
    let e = Env::new("nosync");
    let (a, _, _) = planted(&e);
    e.ok(&a, &["remote", "trust", "origin"]);
    let mut w = e
        .cmd(
            &a,
            &[
                "watch",
                "--json",
                "--no-sync",
                "--poll",
                "50",
                "--debounce",
                "50",
                "--sync",
                "100",
            ],
        )
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let out = w.stdout.take().unwrap();
    let mut r = BufReader::new(out);
    let mut first = String::new();
    r.read_line(&mut first).unwrap();
    assert!(first.contains("\"sync\":false"), "{first}");
    fs::write(a.join("g.txt"), "x\n").unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(w.id().to_string())
        .status();
    let _ = w.wait();
    assert_eq!(e.contacts(), 0);
}

/// Issue #46: a crash between the trust-store rename and the legacy macOS
/// unlink (stage `trust-moved`, after the directory fsync) loses no entry:
/// the new store already holds the legacy entries plus the new one, the
/// legacy file is still there, and a rerun finishes the move.
#[test]
fn trust_move_crash_after_rename_loses_no_entry() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let e = Env::new("tmove");
    let (a, _, _) = planted(&e);
    let home = e.t.join("home");
    fs::create_dir_all(&home).unwrap();
    let mac = home.join("Library/Application Support/barocss-hub/trusted-remotes");
    let dotcfg = home.join(".config/barocss-hub/trusted-remotes");
    fs::create_dir_all(dotcfg.parent().unwrap()).unwrap();
    let legacy =
        "barocss-hub/trusted-remotes/v1\nT s/old/one sssh://h/x s\nT s/old/two sfile:///y s\n";
    fs::write(&dotcfg, legacy).unwrap();
    let run = |crash: bool| {
        let mut c = e.cmd(&a, &["remote", "trust", "origin"]);
        c.env_remove("BAROCSS_HUB_CONFIG_DIR")
            .env_remove("XDG_CONFIG_HOME")
            .env("HOME", &home);
        if crash {
            c.env("HUB_TEST_CRASH_STAGE", "trust-moved");
        }
        c.output().unwrap()
    };
    let o = run(true);
    assert!(!o.status.success(), "the crash hook did not fire");
    let new = fs::read_to_string(&mac).unwrap();
    for l in legacy.lines().skip(1) {
        assert!(new.contains(l), "lost {l}: {new}");
    }
    assert_eq!(new.lines().count(), 4, "{new}");
    assert!(dotcfg.is_file(), "legacy removed before the crash point");
    let o = run(false);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!dotcfg.exists());
    assert_eq!(fs::read_to_string(&mac).unwrap(), new);
}
