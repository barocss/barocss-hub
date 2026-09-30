//! Issue #45: planted remotes and first contact. A `.hub/remotes` that came
//! with a folder must not make `sync` or `watch` contact its ssh remote
//! silently. Every test uses a private `XDG_CONFIG_HOME` (or `HOME`), never
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
    assert!(st.starts_with("barocss-hub/trusted-remotes/v0\n"), "{st}");
    assert!(st.contains(&format!("T s{url} s{BIN}")), "{st}");
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
    // file:// remotes are local paths: never gated.
    let b = e.repo("b");
    fs::write(
        a.join(".hub/remotes"),
        format!("barocss-hub/remotes/v0\nR sb sfile://{}\n", b.display()),
    )
    .unwrap();
    e.ok(&a, &["sync", "b"]);
    assert!(e
        .ok(&a, &["remote", "trust", "b"])
        .contains("no trust needed"));
}

#[test]
fn home_fallback_when_xdg_is_unset_or_relative() {
    let e = Env::new("home");
    let (a, _, _) = planted(&e);
    let home = e.t.join("home");
    fs::create_dir_all(&home).unwrap();
    for xdg in [None, Some("relative/dir")] {
        let mut c = e.cmd(&a, &["sync", "--yes"]);
        c.env_remove("XDG_CONFIG_HOME").env("HOME", &home);
        if let Some(x) = xdg {
            c.env("XDG_CONFIG_HOME", x);
        }
        assert!(c.output().unwrap().status.success());
    }
    assert!(home.join(".config/barocss-hub/trusted-remotes").is_file());
    assert!(!e.store().exists());
    // Neither variable usable: refused (nothing to remember it in) unless
    // --yes, which still syncs.
    let mut c = e.cmd(&a, &["sync", "--json"]);
    c.env_remove("XDG_CONFIG_HOME").env_remove("HOME");
    let o = c.output().unwrap();
    assert!(String::from_utf8_lossy(&o.stderr).contains("untrusted-remote"));
    let mut c = e.cmd(&a, &["sync", "--yes"]);
    c.env_remove("XDG_CONFIG_HOME").env_remove("HOME");
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
