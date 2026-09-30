//! Issue #43: remote sync over the wire protocol, through a fake `ssh`
//! (`HUB_SSH_COMMAND`) so no sshd is needed. The fake runs the remote
//! command the way sshd does (`sh -c "$cmd"`, mode `shell`) or as a forced
//! command (`SSH_ORIGINAL_COMMAND`, mode `forced`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_barocss-hub");

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "hub43-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    fs::canonicalize(p).unwrap()
}

/// The fake ssh: skips options up to `--`, then host and command.
fn fake_ssh(dir: &Path) -> PathBuf {
    let p = dir.join("fake-ssh");
    fs::write(
        &p,
        format!(
            r#"#!/bin/sh
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift
host="$1"; cmd="$2"
[ -n "$FAKE_SSH_LOG" ] && printf '%s\n' "$host" "$cmd" >> "$FAKE_SSH_LOG"
if [ "$FAKE_SSH_MODE" = forced ]; then
  SSH_ORIGINAL_COMMAND="$cmd" exec {BIN} serve --stdio $FAKE_SSH_ARGS --from-ssh-original-command
fi
exec sh -c "$cmd"
"#
        ),
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&p).status().unwrap();
    p
}

fn hub_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(BIN);
    c.args(args).current_dir(dir).env_remove("BAROCSS_HUB_SERVE_ROOT");
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn ok_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let o = hub_env(dir, args, env);
    assert!(
        o.status.success(),
        "hub {args:?} in {}: {}{}",
        dir.display(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> String {
    ok_env(dir, args, &[])
}

fn fail_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let o = hub_env(dir, args, env);
    assert!(!o.status.success(), "hub {args:?} unexpectedly succeeded");
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn head(d: &Path) -> String {
    ok(d, &["status"])
        .lines()
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .to_string()
}

fn worktree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(d: &Path, rel: &str, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            let n = e.file_name().into_string().unwrap();
            if rel.is_empty() && n == ".hub" {
                continue;
            }
            let p = if rel.is_empty() { n } else { format!("{rel}/{n}") };
            if e.file_type().unwrap().is_dir() {
                walk(&e.path(), &p, out);
            } else {
                out.insert(p, fs::read(e.path()).unwrap());
            }
        }
    }
    let mut m = BTreeMap::new();
    walk(root, "", &mut m);
    m
}

/// Every file under `d` with bytes and mtime (read-only checks).
fn files_mtimes(d: &Path) -> BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
    let mut m = BTreeMap::new();
    let mut stack = vec![d.to_path_buf()];
    while let Some(x) = stack.pop() {
        for e in fs::read_dir(&x).unwrap() {
            let e = e.unwrap();
            let md = fs::symlink_metadata(e.path()).unwrap();
            if md.is_dir() {
                stack.push(e.path());
                m.insert(e.path(), (Vec::new(), md.modified().unwrap()));
            } else {
                m.insert(e.path(), (fs::read(e.path()).unwrap(), md.modified().unwrap()));
            }
        }
    }
    m
}

struct Env {
    t: PathBuf,
    ssh: String,
}

impl Env {
    fn new(tag: &str) -> Env {
        let t = tmp(tag);
        let ssh = fake_ssh(&t).to_string_lossy().into_owned();
        Env { t, ssh }
    }
    fn env(&self) -> Vec<(&str, &str)> {
        vec![("HUB_SSH_COMMAND", self.ssh.as_str())]
    }
    fn repo(&self, name: &str) -> PathBuf {
        let d = self.t.join(name);
        fs::create_dir_all(&d).unwrap();
        ok(&d, &["init"]);
        d
    }
    fn bare(&self, name: &str) -> PathBuf {
        ok(&self.t, &["init", "--bare", name]);
        self.t.join(name)
    }
    fn sync(&self, d: &Path, arg: &str) -> String {
        ok_env(d, &["sync", arg], &self.env())
    }
}

#[test]
fn ssh_sync_through_bare_server_converges() {
    let e = Env::new("ssh-basic");
    let srv = e.bare("srv.hub");
    assert!(srv.join(".hub/bare").exists());
    let (a, b) = (e.repo("a"), e.repo("b"));
    let url = format!("ssh://localhost{}", srv.display());
    for d in [&a, &b] {
        ok(d, &["remote", "add", "--exec", BIN, "origin", &url]);
    }
    fs::write(a.join("f.txt"), "from a\n").unwrap();
    fs::create_dir_all(a.join("d")).unwrap();
    fs::write(a.join("d/big.bin"), vec![7u8; 700_000]).unwrap();
    ok(&a, &["record", "-m", "a1"]);
    ok(&a, &["publish", "a1"]);
    fs::write(b.join("g.txt"), "from b\n").unwrap();
    ok(&b, &["record"]);
    let s = ok_env(&a, &["sync"], &e.env());
    assert!(s.contains("sent 5 op(s), 2 blob(s), 1 checkpoint(s)"), "{s}");
    e.sync(&b, "origin");
    e.sync(&a, "origin");
    ok(&a, &["checkout"]);
    ok(&b, &["checkout"]);
    assert_eq!(head(&a), head(&b));
    assert_eq!(head(&a), head(&srv));
    assert_eq!(worktree(&a), worktree(&b));
    assert!(ok(&b, &["log"]).contains("[published] a1"));
    // The server: bare status, no working directory section.
    let st = ok(&srv, &["status"]);
    assert!(st.contains("bare repository"), "{st}");
    // scp-like form, home-relative: HOME points at the temp root.
    let home = e.t.to_str().unwrap();
    let mut env = e.env();
    env.push(("HOME", home));
    // No `exec=` here: the remote binary is found on the login PATH.
    let path = format!(
        "{}:{}",
        Path::new(BIN).parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    env.push(("PATH", &path));
    let s = ok_env(&b, &["sync", "--json", "localhost:srv.hub"], &env);
    assert!(s.contains("\"command\":\"sync\""), "{s}");
    assert!(s.contains("\"read_only\":false"), "{s}");
}

#[test]
fn bare_repository_refuses_working_directory_commands() {
    let e = Env::new("bare");
    let srv = e.bare("srv");
    for c in [
        &["record"][..],
        &["checkout"],
        &["mkdir", "x"],
        &["touch", "x"],
        &["watch"],
    ] {
        let o = hub_env(&srv, &[c, &["--json"]].concat(), &[]);
        assert!(!o.status.success());
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("\"code\":\"bare\""), "{c:?}: {err}");
    }
    ok(&srv, &["log"]);
    ok(&srv, &["warnings"]);
    let j = ok(&srv, &["status", "--json"]);
    assert!(j.contains("\"bare\":true") && !j.contains("workdir"), "{j}");
}

#[test]
fn read_only_server_pulls_only_and_is_untouched() {
    let e = Env::new("ro");
    let srv = e.repo("srv");
    fs::write(srv.join("s.txt"), "server\n").unwrap();
    ok(&srv, &["record", "-m", "s"]);
    ok(&srv, &["publish", "s"]);
    let a = e.repo("a");
    fs::write(a.join("a.txt"), "client\n").unwrap();
    ok(&a, &["record", "-m", "c"]);
    ok(&a, &["publish", "c"]);
    let before = files_mtimes(&srv.join(".hub"));
    let root = e.t.to_str().unwrap().to_string();
    let args = format!("--read-only --root {root}");
    let mut env = e.env();
    env.push(("FAKE_SSH_MODE", "forced"));
    env.push(("FAKE_SSH_ARGS", &args));
    let url = format!("ssh://localhost{}", srv.display());
    let s = ok_env(&a, &["sync", &url], &env);
    assert!(s.contains("remote is read-only; pulled only"), "{s}");
    assert!(s.contains("sent 0 op(s)"), "{s}");
    let j = ok_env(&a, &["sync", "--json", &url], &env);
    assert!(j.contains("\"read_only\":true"), "{j}");
    assert_eq!(files_mtimes(&srv.join(".hub")), before, "read-only server changed");
    ok(&a, &["checkout"]);
    assert!(a.join("s.txt").exists());
    assert!(ok(&a, &["log"]).contains("[published] s"));
    // Read-only added by the client in SSH_ORIGINAL_COMMAND also holds.
    let args2 = format!("--root {root}");
    let mut env2 = e.env();
    env2.push(("FAKE_SSH_MODE", "forced"));
    env2.push(("FAKE_SSH_ARGS", &args2));
    let s = ok_env(&a, &["sync", &url], &env2);
    assert!(!s.contains("read-only"), "{s}");
}

#[test]
fn forced_command_confinement() {
    let e = Env::new("forced");
    let root = e.t.join("root");
    fs::create_dir_all(&root).unwrap();
    ok(&root, &["init", "--bare", "srv"]);
    let outside = e.repo("outside");
    let a = e.repo("a");
    let rootarg = format!("--root {}", root.display());
    let forced = |url: &str, extra: &[(&str, &str)]| {
        let mut env = e.env();
        env.push(("FAKE_SSH_MODE", "forced"));
        env.push(("FAKE_SSH_ARGS", &rootarg));
        env.extend_from_slice(extra);
        hub_env(&a, &["sync", "--json", url], &env)
    };
    let code = |o: &Output| String::from_utf8_lossy(&o.stderr).into_owned();
    // Inside the root: fine.
    let o = forced(&format!("ssh://h{}/srv", root.display()), &[]);
    assert!(o.status.success(), "{}", code(&o));
    // Outside, `..`, ~user, a symlink escaping the root.
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    for url in [
        format!("ssh://h{}", outside.display()),
        format!("ssh://h{}/srv/../../outside", root.display()),
        "h:~root/x".to_string(),
        format!("ssh://h{}/link", root.display()),
    ] {
        let o = forced(&url, &[]);
        assert!(!o.status.success(), "{url}");
        assert!(code(&o).contains("\"code\":\"forbidden\""), "{url}: {}", code(&o));
    }
    // The environment cannot widen the root under a forced command.
    let o = forced(
        &format!("ssh://h{}", outside.display()),
        &[("BAROCSS_HUB_SERVE_ROOT", "/")],
    );
    assert!(code(&o).contains("forbidden"), "{}", code(&o));
    // A .hub/ store file symlinked outside the repository.
    let srv2 = root.join("srv2");
    fs::create_dir_all(&srv2).unwrap();
    ok(&srv2, &["init"]);
    fs::remove_file(srv2.join(".hub/checkpoints.log")).unwrap();
    std::os::unix::fs::symlink(outside.join(".hub/checkpoints.log"), srv2.join(".hub/checkpoints.log")).unwrap();
    let o = forced(&format!("ssh://h{}", srv2.display()), &[]);
    assert!(code(&o).contains("forbidden"), "{}", code(&o));
}

#[test]
fn no_command_injection_through_url_path_or_exec() {
    let e = Env::new("inject");
    let a = e.repo("a");
    let pwned = e.t.join("pwned");
    let log = e.t.join("ssh.log");
    let logs = log.to_str().unwrap().to_string();
    let mut env = e.env();
    env.push(("FAKE_SSH_LOG", &logs));
    for url in [
        format!("ssh://h{}/x';touch {};'", e.t.display(), pwned.display()),
        format!("h:$(touch {})", pwned.display()),
        format!("h:`touch {}`", pwned.display()),
    ] {
        let o = hub_env(&a, &["sync", &url], &env);
        assert!(!o.status.success(), "{url}");
        assert!(!pwned.exists(), "{url} executed a command");
    }
    // The command sent is exactly the §5.6 form, quoting the path.
    let l = fs::read_to_string(&log).unwrap();
    assert!(l.contains("serve --stdio '"), "{l}");
    // exec values with spaces or quotes are refused at `remote add`.
    for bad in ["barocss-hub --x", "a'b", "a\"b"] {
        let err = fail_env(&a, &["remote", "add", "--exec", bad, "o", "h:/p"], &[]);
        assert!(err.contains("exec must be one word"), "{err}");
    }
    // Option injection through the host is refused before ssh runs.
    let err = fail_env(&a, &["sync", "-oProxyCommand=touch:x"], &env);
    assert!(err.contains("bad remote url") || err.contains("usage"), "{err}");
    assert!(!pwned.exists());
}

#[test]
fn central_replica_three_clients_concurrent_ssh_and_file() {
    for via_ssh in [false, true] {
        let e = Env::new(if via_ssh { "central-ssh" } else { "central-file" });
        let srv = e.bare("srv");
        let url = if via_ssh {
            format!("ssh://localhost{}", srv.display())
        } else {
            format!("file://{}", srv.display())
        };
        let cs: Vec<PathBuf> = (0..3).map(|i| e.repo(&format!("c{i}"))).collect();
        for (i, c) in cs.iter().enumerate() {
            ok(c, &["remote", "add", "--exec", BIN, "origin", &url]);
            fs::write(c.join("shared.txt"), format!("client {i}\n")).unwrap();
            fs::write(c.join(format!("own{i}.txt")), vec![i as u8; 50_000 + i]).unwrap();
            ok(c, &["record"]);
        }
        // Concurrent syncs (file:// serializes on the server's lock; ssh
        // sessions merge under the per-step lock).
        let ssh = e.ssh.clone();
        let hs: Vec<_> = cs
            .iter()
            .cloned()
            .map(|c| {
                let ssh = ssh.clone();
                std::thread::spawn(move || {
                    Command::new(BIN)
                        .args(["sync"])
                        .current_dir(&c)
                        .env("HUB_SSH_COMMAND", ssh)
                        .output()
                        .unwrap()
                })
            })
            .collect();
        for h in hs {
            let o = h.join().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        }
        // One more round in a fixed order reaches the join everywhere.
        for c in &cs {
            e.sync(c, "origin");
        }
        for c in &cs {
            ok(c, &["checkout"]);
        }
        let h0 = head(&cs[0]);
        let w0 = worktree(&cs[0]);
        for c in &cs[1..] {
            assert_eq!(head(c), h0);
            assert_eq!(worktree(c), w0);
        }
        assert_eq!(head(&srv), h0);
        assert!(ok(&cs[0], &["status"]).contains("conflicts:"));
    }
}

#[test]
fn watch_syncs_with_remotes() {
    let e = Env::new("watch");
    let srv = e.bare("srv");
    let (a, b) = (e.repo("a"), e.repo("b"));
    let url = format!("ssh://localhost{}", srv.display());
    for d in [&a, &b] {
        ok(d, &["remote", "add", "--exec", BIN, "origin", &url]);
    }
    let w = Command::new(BIN)
        .args(["watch", "--json", "--poll", "50", "--debounce", "100", "--sync", "300"])
        .current_dir(&b)
        .env("HUB_SSH_COMMAND", &e.ssh)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    fs::write(a.join("x.txt"), "x\n").unwrap();
    ok(&a, &["record"]);
    e.sync(&a, "origin");
    let t0 = std::time::Instant::now();
    while !b.join("x.txt").exists() && t0.elapsed() < std::time::Duration::from_secs(20) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = Command::new("kill").arg("-TERM").arg(w.id().to_string()).status();
    let out = w.wait_with_output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(b.join("x.txt").exists(), "{s}");
    assert!(s.contains("\"remotes\":1"), "{s}");
    assert!(s.contains("\"event\":\"synced\"") && s.contains("\"peer\":\"origin\""), "{s}");
}
