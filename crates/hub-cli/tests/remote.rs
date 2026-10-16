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

/// Issue #45: every run gets a private trusted-remotes store (never the
/// user's real `~/.config`). A test may override `XDG_CONFIG_HOME`/`HOME`.
fn cfg_home() -> &'static str {
    static D: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    D.get_or_init(|| tmp("xdg").to_string_lossy().into_owned())
}

fn cmd() -> Command {
    let mut c = Command::new(BIN);
    c.env("XDG_CONFIG_HOME", cfg_home())
        .env(
            "BAROCSS_HUB_CONFIG_DIR",
            std::path::Path::new(&cfg_home()).join("barocss-hub"),
        )
        .env_remove("BAROCSS_HUB_SERVE_ROOT");
    c
}

fn hub_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = cmd();
    c.args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null());
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
            let p = if rel.is_empty() {
                n
            } else {
                format!("{rel}/{n}")
            };
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
                m.insert(
                    e.path(),
                    (fs::read(e.path()).unwrap(), md.modified().unwrap()),
                );
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
    assert!(
        s.contains("sent 5 op(s), 2 blob(s), 1 checkpoint(s)"),
        "{s}"
    );
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
    assert_eq!(
        files_mtimes(&srv.join(".hub")),
        before,
        "read-only server changed"
    );
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
        format!("ssh://h{}/link", root.display()),
    ] {
        let o = forced(&url, &[]);
        assert!(!o.status.success(), "{url}");
        assert!(
            code(&o).contains("\"code\":\"forbidden\""),
            "{url}: {}",
            code(&o)
        );
    }
    // `~user` is refused by the client since #45 (never sent); the server
    // still refuses it (`remote::confine`, unit-tested with the grammar).
    let o = forced("h:~root/x", &[]);
    assert!(code(&o).contains("\"code\":\"usage\""), "{}", code(&o));
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
    std::os::unix::fs::symlink(
        outside.join(".hub/checkpoints.log"),
        srv2.join(".hub/checkpoints.log"),
    )
    .unwrap();
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
    // exec values with spaces or quotes are refused at `remote add`
    // (the full metacharacter matrix is in `tests/injection45.rs`).
    for bad in ["barocss-hub --x", "a'b", "a\"b"] {
        let err = fail_env(&a, &["remote", "add", "--exec", bad, "o", "h:/p"], &[]);
        assert!(err.contains("bad --exec"), "{err}");
    }
    // Option injection through the host is refused before ssh runs.
    let err = fail_env(&a, &["sync", "-oProxyCommand=touch:x"], &env);
    assert!(
        err.contains("bad remote url") || err.contains("usage"),
        "{err}"
    );
    assert!(!pwned.exists());
}

#[test]
fn central_replica_three_clients_concurrent_ssh_and_file() {
    for via_ssh in [false, true] {
        let e = Env::new(if via_ssh {
            "central-ssh"
        } else {
            "central-file"
        });
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
                    cmd()
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
    let w = cmd()
        .args([
            "watch",
            "--json",
            "--poll",
            "50",
            "--debounce",
            "100",
            "--sync",
            "300",
        ])
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
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(w.id().to_string())
        .status();
    let out = w.wait_with_output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(b.join("x.txt").exists(), "{s}");
    assert!(s.contains("\"remotes\":1"), "{s}");
    assert!(
        s.contains("\"event\":\"synced\"") && s.contains("\"peer\":\"origin\""),
        "{s}"
    );
}

/// Golden error documents for the codes #43 adds that a CLI command can
/// produce locally (`version-mismatch` and `limit` come from a peer and
/// are covered by `tests/wire.rs`). Same mechanism as `tests/json.rs`
/// (`UPDATE_GOLDEN=1` rewrites).
#[test]
fn new_error_code_goldens() {
    let e = Env::new("codes");
    let srv = e.bare("srv");
    let a = e.repo("a");
    let golden =
        |name: &str, dir: &Path, args: &[&str], env: &[(&str, &str)], subs: &[(&str, &str)]| {
            let o = hub_env(dir, &[args, &["--json"]].concat(), env);
            assert!(!o.status.success());
            let mut g = String::from_utf8(o.stderr).unwrap();
            for (f, t) in subs {
                g = g.replace(f, t);
            }
            assert!(g.contains(&format!("\"code\":\"{name}\"")), "{name}: {g}");
            let p = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("tests/golden/json/code-{name}.json"));
            if std::env::var("UPDATE_GOLDEN").is_ok() {
                fs::write(&p, &g).unwrap();
            } else {
                assert_eq!(g, fs::read_to_string(&p).unwrap(), "golden code-{name}");
            }
        };
    let t = e.t.to_string_lossy().into_owned();
    golden("bare", &srv, &["record"], &[], &[]);
    golden(
        "unsupported",
        &a,
        &["sync", "https://example.invalid/r"],
        &[],
        &[],
    );
    // A remote refusal carries `"remote": true`.
    let root = e.t.join("root");
    fs::create_dir_all(&root).unwrap();
    let args = format!("--root {}", root.display());
    let mut env = e.env();
    env.push(("FAKE_SSH_MODE", "forced"));
    env.push(("FAKE_SSH_ARGS", &args));
    golden(
        "forbidden",
        &a,
        &["sync", &format!("ssh://h{}", srv.display())],
        &env,
        &[(&t, "<t>")],
    );
    // Something that is not barocss-hub answers: no preamble.
    let junk = e.t.join("junk-ssh");
    fs::write(&junk, "#!/bin/sh\necho 'Welcome to host'\ncat >/dev/null\n").unwrap();
    Command::new("chmod").arg("+x").arg(&junk).status().unwrap();
    let js = junk.to_string_lossy().into_owned();
    golden(
        "protocol",
        &a,
        &["sync", "h:/r"],
        &[("HUB_SSH_COMMAND", &js)],
        &[(&t, "<t>")],
    );
    // Issue #45: a planted ssh remote, no terminal, no --yes.
    let p = e.repo("planted");
    fs::write(
        p.join(".hub/remotes"),
        format!(
            "barocss-hub/remotes/v0\nR sorigin sssh://attacker.example{}/r exec=barocss-hub default=1\n",
            e.t.display()
        ),
    )
    .unwrap();
    golden("untrusted-remote", &p, &["sync"], &e.env(), &[(&t, "<t>")]);
    // A remote that never answers.
    let mute = e.t.join("mute-ssh");
    fs::write(&mute, "#!/bin/sh\nexec sleep 30\n").unwrap();
    Command::new("chmod").arg("+x").arg(&mute).status().unwrap();
    let ms = mute.to_string_lossy().into_owned();
    golden(
        "timeout",
        &a,
        &["sync", "h:/r"],
        &[
            ("HUB_SSH_COMMAND", &ms),
            ("HUB_TEST_WIRE_TIMEOUT_MS", "300"),
        ],
        &[(&t, "<t>")],
    );
}

/// Issue #46 Review repro: two copies of one `.hub/` (same replica id)
/// record different content of `f` and meet only through a server. Before
/// #46 both ended at the same head with different content and no warning.
/// Now the copy's sync is refused with `duplicate-replica`, an equivocation
/// and a DuplicateReplica warning naming the server are persisted, and the
/// copy's log is unchanged. `via` = "ssh" or "file".
fn diverge_repro(via: &str) {
    let e = Env::new(&format!("div46-{via}"));
    let srv = e.bare("srv.hub");
    let a = e.repo("a");
    let url = match via {
        "ssh" => format!("ssh://localhost{}", srv.display()),
        _ => format!("file://{}", srv.display()),
    };
    ok(&a, &["remote", "add", "--exec", BIN, "origin", &url]);
    fs::write(a.join("f"), "base\n").unwrap();
    ok(&a, &["record"]);
    e.sync(&a, "origin");
    // The copy: same .hub/, same replica id, same remote.
    let c = e.t.join("c");
    let st = Command::new("cp")
        .args(["-R", a.to_str().unwrap(), c.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(st.success());
    // A copied folder is a new place for the trust store (#45).
    ok(&c, &["remote", "trust", "origin"]);
    fs::write(a.join("f"), "A").unwrap();
    ok(&a, &["record"]);
    fs::write(c.join("f"), "BB").unwrap();
    ok(&c, &["record"]);
    e.sync(&a, "origin");
    let log0 = fs::read(c.join(".hub/ops.log")).unwrap();
    let o = hub_env(&c, &["sync", "--json"], &e.env());
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(!o.status.success(), "copy sync must fail: {out} {err}");
    assert!(err.contains("duplicate-replica"), "{err}");
    assert!(err.contains("reid"), "{err}");
    assert_eq!(fs::read(c.join(".hub/ops.log")).unwrap(), log0);
    let w = ok(&c, &["warnings"]);
    assert!(w.contains("equivocation"), "{w}");
    assert!(w.contains("duplicate-replica"), "{w}");
    // Later syncs stay refused until reid (D-022), so nothing merges.
    let again = fail_env(&c, &["sync"], &e.env());
    assert!(again.contains("duplicate-replica"), "{again}");
    // The server and the original are untouched by the copy.
    assert_eq!(head(&a), head(&srv));
    // After reid (D-024: the diverged op stays), the divergence is a known,
    // listed warning: sync works and the warning is still shown.
    ok(&c, &["reid"]);
    // The push half then lets the server find the same divergence on its
    // own (op-level evidence from the copy's answer): it records it and
    // stops that session once; the next session goes through.
    let srv_w = fail_env(&c, &["sync"], &e.env());
    assert!(srv_w.contains("remote: sync aborted"), "{srv_w}");
    assert!(ok(&srv, &["warnings"]).contains("equivocation"));
    // From then on the session completes; the server keeps rejecting the
    // copy's diverged op, so `sync` exits 3 (D-045) and says so: visible,
    // never silent.
    let o = hub_env(&c, &["sync"], &e.env());
    assert_eq!(o.status.code(), Some(3));
    // Localization asks for ids only: no blob crosses for evidence (#46 R(b)).
    let so = String::from_utf8_lossy(&o.stdout);
    assert!(so.contains("sent 0 op(s), 0 blob(s)"), "{so}");
    assert!(String::from_utf8_lossy(&o.stderr).contains("(equivocation)"));
    assert!(ok(&c, &["warnings"]).contains("equivocation"));
    // Issue #50 (D-050): that rejection was reported; later syncs exit 0
    // without repeating it, while the D-008 equivocation warning stays
    // listed and the rejection is listed as already reported.
    for _ in 0..2 {
        let o = hub_env(&c, &["sync"], &e.env());
        let err = String::from_utf8_lossy(&o.stderr).into_owned();
        assert_eq!(o.status.code(), Some(0), "{err}");
        assert!(!err.contains("rejected your op"), "{err}");
    }
    let w = ok(&c, &["warnings"]);
    assert!(w.contains("equivocation"), "{w}");
    assert!(w.contains("reported rejection (suppressed)"), "{w}");
    // Clearing reports it again.
    ok(&c, &["warnings", "--clear-reported", "origin"]);
    let o = hub_env(&c, &["sync"], &e.env());
    assert_eq!(o.status.code(), Some(3));
}

#[test]
fn diverged_copy_detected_over_ssh() {
    diverge_repro("ssh");
}

#[test]
fn diverged_copy_detected_over_file() {
    diverge_repro("file");
}

// ---------------------------------------------------------------- #49

/// Every file under `d` (including `.hub/`) with its bytes.
fn all_bytes(d: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    files_mtimes(d)
        .into_iter()
        .map(|(p, (b, _))| (p, b))
        .collect()
}

/// The #46 repro up to the copy's refused sync. `edit` runs in the copy
/// before its diverging record. Returns (env, server, original, copy).
fn diverged(tag: &str, via: &str, edit: impl Fn(&Path)) -> (Env, PathBuf, PathBuf, PathBuf) {
    diverged_with(tag, via, true, edit)
}

/// [`diverged`]; `bytes`: `f` is forced binary by `.hubattributes`, so
/// its concurrent saves stay whole-blob LWW (the #49 expectations); else
/// they are `TextEdit`s that merge (Issue #56).
fn diverged_with(
    tag: &str,
    via: &str,
    bytes: bool,
    edit: impl Fn(&Path),
) -> (Env, PathBuf, PathBuf, PathBuf) {
    let e = Env::new(tag);
    let srv = e.bare("srv.hub");
    let a = e.repo("a");
    let url = match via {
        "ssh" => format!("ssh://localhost{}", srv.display()),
        _ => format!("file://{}", srv.display()),
    };
    ok(
        &a,
        &["remote", "add", "--exec", BIN, "--default", "origin", &url],
    );
    fs::write(a.join("f"), "base\n").unwrap();
    fs::write(a.join("g"), "gee\n").unwrap();
    fs::write(a.join("h"), "aitch\n").unwrap();
    if bytes {
        fs::write(a.join(".hubattributes"), "f binary\n").unwrap();
    }
    ok(&a, &["record"]);
    e.sync(&a, "origin");
    let c = e.t.join("c");
    let st = Command::new("cp")
        .args(["-R", a.to_str().unwrap(), c.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(st.success());
    ok(&c, &["remote", "trust", "origin"]);
    fs::write(a.join("f"), if bytes { "A" } else { "A\n" }).unwrap();
    ok(&a, &["record"]);
    fs::write(c.join("f"), if bytes { "BB" } else { "BB\n" }).unwrap();
    edit(&c);
    ok(&c, &["record"]);
    e.sync(&a, "origin");
    let err = fail_env(&c, &["sync"], &e.env());
    assert!(err.contains("reid --rescue"), "{err}");
    (e, srv, a, c)
}

/// A fresh clone of the server's head: its working tree.
fn server_tree(e: &Env, srv: &Path, tag: &str) -> BTreeMap<String, Vec<u8>> {
    let d = e.repo(tag);
    ok(
        &d,
        &[
            "remote",
            "add",
            "--exec",
            BIN,
            "origin",
            &format!("file://{}", srv.display()),
        ],
    );
    e.sync(&d, "origin");
    ok(&d, &["checkout"]);
    worktree(&d)
}

/// Blob ids of every set-content op on `path` in `d` (history).
fn content_history(d: &Path, path: &str) -> Vec<Vec<u8>> {
    let j = ok(d, &["log", "--path", path, "--json"]);
    let mut v = Vec::new();
    for part in j.split("\"blob\":\"").skip(1) {
        let b = &part[..64];
        let o = Command::new(BIN)
            .args(["cat-blob", b])
            .current_dir(d)
            .output()
            .unwrap();
        if o.status.success() && !v.contains(&o.stdout) {
            v.push(o.stdout);
        }
    }
    v
}

fn backups(c: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(rd) = fs::read_dir(c.join(".hub/rescue")) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                v.push(e.path());
            }
        }
    }
    v
}

/// Issue #49: the #46 repro, then `reid --rescue` (online: it confirms with
/// one sync); the next syncs exit 0, the server has the copy's content at
/// head and the original's in history, and the backup holds the copy's
/// bytes.
fn rescue_repro(via: &str) {
    let (e, srv, a, c) = diverged(&format!("res49-{via}"), via, |_| {});
    let o = hub_env(&c, &["reid", "--rescue"], &e.env());
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert_eq!(
        o.status.code(),
        Some(0),
        "{out}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(out.contains("re-authored by effect"), "{out}");
    assert!(out.contains("modified: f"), "{out}");
    assert!(out.contains("synced with"), "{out}");
    let b = backups(&c);
    assert_eq!(b.len(), 1);
    assert_eq!(fs::read(b[0].join("files/f")).unwrap(), b"BB");
    assert!(b[0].join("done").exists() && !c.join(".hub/rescue/current").exists());
    for _ in 0..2 {
        let o = hub_env(&c, &["sync"], &e.env());
        assert_eq!(
            o.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(!String::from_utf8_lossy(&o.stderr).contains("rejected"));
    }
    let t = server_tree(&e, &srv, "check");
    assert_eq!(t["f"], b"BB");
    assert_eq!(t["g"], b"gee\n");
    let hist = content_history(&e.t.join("check"), "f");
    assert!(hist.contains(&b"A".to_vec()), "{hist:?}");
    assert!(hist.contains(&b"BB".to_vec()), "{hist:?}");
    // The original converges on the rescued content too.
    e.sync(&a, "origin");
    ok(&a, &["checkout"]);
    assert_eq!(fs::read(a.join("f")).unwrap(), b"BB");
    assert_eq!(worktree(&a), worktree(&c));
    // Idempotent rerun: a no-op, .hub/ and the tree byte-identical.
    let before = all_bytes(&c);
    let o = ok_env(&c, &["reid", "--rescue"], &e.env());
    assert!(o.contains("nothing to rescue"), "{o}");
    assert_eq!(all_bytes(&c), before);
}

#[test]
fn rescue_after_divergence_over_ssh() {
    rescue_repro("ssh");
}

#[test]
fn rescue_after_divergence_over_file() {
    rescue_repro("file");
}

/// After a plain `reid` and the exit-3 report (D-045/D-050), rescue finds
/// the op through the reported-rejections memory and forgets it; offline,
/// then a sync.
#[test]
fn rescue_offline_after_reported_rejection() {
    let (e, srv, _a, c) = diverged("res49-rep", "file", |_| {});
    ok(&c, &["reid"]);
    let _ = hub_env(&c, &["sync"], &e.env()); // the server stops once
    let o = hub_env(&c, &["sync"], &e.env());
    assert_eq!(o.status.code(), Some(3));
    assert!(ok(&c, &["warnings"]).contains("reported rejection"));
    let out = ok_env(&c, &["reid", "--rescue", "--offline"], &e.env());
    assert!(!out.contains("synced with"), "{out}");
    assert!(out.contains("re-authored by effect"), "{out}");
    assert!(!ok(&c, &["warnings"]).contains("reported rejection"));
    let o = hub_env(&c, &["sync"], &e.env());
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(server_tree(&e, &srv, "check")["f"], b"BB");
}

/// An untouched copy (nothing recorded under the shared id): rescue is a
/// reid that clears the block; the copy then follows the server.
#[test]
fn rescue_untouched_copy_is_a_reid() {
    let e = Env::new("res49-untouched");
    let srv = e.bare("srv.hub");
    let a = e.repo("a");
    ok(
        &a,
        &[
            "remote",
            "add",
            "--exec",
            BIN,
            "origin",
            &format!("file://{}", srv.display()),
        ],
    );
    fs::write(a.join("f"), "base\n").unwrap();
    ok(&a, &["record"]);
    e.sync(&a, "origin");
    let c = e.t.join("c");
    assert!(Command::new("cp")
        .args(["-R", a.to_str().unwrap(), c.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    ok(&c, &["remote", "trust", "origin"]);
    fs::write(a.join("f"), "A").unwrap();
    ok(&a, &["record"]);
    e.sync(&a, "origin");
    let err = fail_env(&c, &["sync"], &e.env());
    assert!(err.contains("reid --rescue"), "{err}");
    let out = ok_env(&c, &["reid", "--rescue", "--offline"], &e.env());
    assert!(out.contains("no ops to re-author"), "{out}");
    assert!(!out.contains("recorded"), "{out}");
    e.sync(&c, "origin");
    ok(&c, &["checkout"]);
    assert_eq!(fs::read(c.join("f")).unwrap(), b"A");
}

/// Deletes and moves in the copy are reproduced by effect.
#[test]
fn rescue_reproduces_deletes_and_moves() {
    let (e, srv, _a, c) = diverged("res49-mv", "file", |c| {
        fs::remove_file(c.join("g")).unwrap();
        fs::rename(c.join("h"), c.join("h2")).unwrap();
        fs::write(c.join("new"), "n\n").unwrap();
    });
    ok_env(&c, &["reid", "--rescue"], &e.env());
    let o = hub_env(&c, &["sync"], &e.env());
    assert_eq!(o.status.code(), Some(0));
    let t = server_tree(&e, &srv, "check");
    assert_eq!(t["f"], b"BB");
    assert!(
        !t.contains_key("g") && !t.contains_key("h"),
        "{:?}",
        t.keys()
    );
    assert_eq!(t["h2"], b"aitch\n");
    assert_eq!(t["new"], b"n\n");
}

/// Issue #56 (Text-CRDT 7.4.4): a text file is re-authored as a
/// `TextEdit` against the store minus R, so the rescued save merges with
/// the concurrent server edit instead of overwriting it; offline too.
#[test]
fn rescue_reauthors_text_as_text_edit() {
    let (e, srv, _a, c) = diverged_with("res56-text", "file", false, |_| {});
    ok_env(&c, &["reid", "--rescue", "--offline"], &e.env());
    let h = ok(&c, &["log", "--path", "f", "--json"]);
    assert!(h.contains("\"kind\":\"text-edit\""), "{h}");
    let o = hub_env(&c, &["sync"], &e.env());
    assert_eq!(o.status.code(), Some(0));
    let t = server_tree(&e, &srv, "check");
    assert!(
        t["f"] == b"A\nBB\n" || t["f"] == b"BB\nA\n",
        "{:?}",
        String::from_utf8_lossy(&t["f"])
    );
    ok(&c, &["checkout"]);
    assert_eq!(fs::read(c.join("f")).unwrap(), t["f"]);
}

/// `--dry-run` (human and JSON) changes nothing.
#[test]
fn rescue_dry_run_changes_nothing() {
    let (e, _srv, _a, c) = diverged("res49-dry", "file", |c| {
        fs::write(c.join("unrecorded"), "u").unwrap();
    });
    let before = all_bytes(&c);
    let out = ok_env(&c, &["reid", "--rescue", "--dry-run"], &e.env());
    assert!(
        out.contains("dry run") && out.contains("modified: f"),
        "{out}"
    );
    let j = ok_env(&c, &["reid", "--rescue", "--dry-run", "--json"], &e.env());
    assert!(
        j.starts_with("{\"schema\":1,\"command\":\"reid\",\"dry_run\":true"),
        "{j}"
    );
    assert!(j.contains("\"new_replica\":null"), "{j}");
    assert_eq!(all_bytes(&c), before);
    // The intended tree includes unrecorded edits.
    let j = ok_env(&c, &["reid", "--rescue", "--json", "--offline"], &e.env());
    assert!(
        j.contains("\"dry_run\":false") && j.contains("\"noop\":false"),
        "{j}"
    );
    assert!(j.contains("\"path\":\"unrecorded\""), "{j}");
    assert!(j.contains("\"sync\":null"), "{j}");
    let o = hub_env(&c, &["sync"], &e.env());
    assert_eq!(o.status.code(), Some(0));
}

/// A crash (abort) at every stage, then a rerun: the same end state, and
/// mutating commands refuse while the rescue is unfinished.
#[test]
fn rescue_crash_at_every_stage_then_rerun() {
    for stage in hub_cli::rescue::STAGES {
        let (e, srv, _a, c) = diverged(&format!("res49-crash-{stage}"), "file", |c| {
            fs::rename(c.join("h"), c.join("h2")).unwrap();
        });
        ok(&c, &["commit", "-m", "kept msg"]);
        let mut env = e.env();
        env.push(("HUB_TEST_CRASH_STAGE", stage));
        let o = hub_env(&c, &["reid", "--rescue", "--offline"], &env);
        assert!(!o.status.success(), "{stage}: no crash");
        if c.join(".hub/rescue/current").exists() {
            // R3: the replica-header marker is set whenever a rescue is
            // in progress.
            // (After `rescue-unmark` every change is done; only `current`
            // is left.)
            if *stage != "rescue-unmark" {
                assert!(
                    hub_cli::rescue::marked(&c.join(".hub")),
                    "{stage}: no marker"
                );
            }
            let err = fail_env(&c, &["record"], &e.env());
            assert!(err.contains("reid --rescue"), "{stage}: {err}");
            let err = fail_env(&c, &["sync"], &e.env());
            assert!(err.contains("interrupted"), "{stage}: {err}");
        }
        assert_eq!(fs::read(c.join("f")).unwrap(), b"BB", "{stage}");
        let out = ok_env(&c, &["reid", "--rescue", "--offline"], &e.env());
        assert!(!out.contains("nothing to rescue"), "{stage}: {out}");
        assert!(!c.join(".hub/rescue/current").exists());
        assert!(
            !hub_cli::rescue::marked(&c.join(".hub")),
            "{stage}: marker left"
        );
        // R1: the checkpoint naming a rescued op is re-created.
        assert!(ok(&c, &["log"]).contains("kept msg"), "{stage}");
        let o = hub_env(&c, &["sync"], &e.env());
        assert_eq!(
            o.status.code(),
            Some(0),
            "{stage}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        let t = server_tree(&e, &srv, "check");
        assert_eq!(t["f"], b"BB", "{stage}");
        assert_eq!(t["h2"], b"aitch\n", "{stage}");
        assert!(!t.contains_key("h"), "{stage}");
        let _ = fs::remove_dir_all(&e.t);
    }
}

/// R1 repair: checkpoints (private and published) that name rescued ops are
/// listed in the plan, kept in the manifest, and re-created as private
/// checkpoints on the rescued head.
#[test]
fn rescue_keeps_checkpoint_messages() {
    let (e, _srv, _a, c) = diverged("res49-ck", "file", |_| {});
    ok(&c, &["commit", "-m", "private checkpoint msg"]);
    fs::write(c.join("f"), "BBB").unwrap();
    ok(&c, &["record", "-m", "my important message"]);
    ok(&c, &["publish", "my important message"]);
    let dry = ok_env(&c, &["reid", "--rescue", "--dry-run"], &e.env());
    assert!(dry.contains("checkpoint 'private checkpoint msg'"), "{dry}");
    assert!(
        dry.contains("checkpoint 'my important message' (published)"),
        "{dry}"
    );
    let j = ok_env(&c, &["reid", "--rescue", "--dry-run", "--json"], &e.env());
    assert!(
        j.contains("\"checkpoints\":[{\"message\":\"private checkpoint msg\""),
        "{j}"
    );
    let out = ok_env(&c, &["reid", "--rescue", "--offline"], &e.env());
    assert!(out.contains("re-created checkpoint"), "{out}");
    assert!(out.contains("re-publish"), "{out}");
    let log = ok(&c, &["log"]);
    assert!(log.contains("private checkpoint msg"), "{log}");
    assert!(log.contains("my important message"), "{log}");
    let b = backups(&c);
    let man = String::from_utf8_lossy(&fs::read(b[0].join("manifest")).unwrap()).into_owned();
    assert!(
        man.contains("ckpt private") && man.contains("ckpt published"),
        "{man}"
    );
    assert_eq!(fs::read(c.join("f")).unwrap(), b"BBB");
    let o = hub_env(&c, &["sync"], &e.env());
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}

/// Open #2 (decided): an online rescue checks out head when the working
/// tree is unchanged since the rescue recorded it, and leaves it otherwise.
#[test]
fn rescue_online_checks_out_only_an_unchanged_tree() {
    let (e, _srv, _a, c) = diverged("res49-co", "file", |_| {});
    let out = ok_env(&c, &["reid", "--rescue"], &e.env());
    assert!(out.contains("checked out head"), "{out}");
    let st = ok_env(&c, &["status", "--json"], &e.env());
    assert!(st.contains("\"state\":\"at-head\""), "{st}");
    let (e, _srv, _a, c) = diverged("res49-co2", "file", |_| {});
    let mut env = e.env();
    env.push(("HUB_TEST_RESCUE_EDIT_AFTER_SYNC", "late"));
    let j = ok_env(&c, &["reid", "--rescue", "--json"], &env);
    if !cfg!(debug_assertions) {
        // The hook is debug-only (#49/#55): a release binary ignores it,
        // edits nothing, and checks out head as usual.
        assert!(j.contains("\"checkout\":{\"done\":true"), "{j}");
        assert!(!c.join("late").exists());
        return;
    }
    assert!(j.contains("\"checkout\":{\"done\":false"), "{j}");
    assert_eq!(
        fs::read(c.join("late")).unwrap(),
        b"edited after the rescue\n"
    );
    let out = String::from_utf8_lossy(&hub_env(&c, &["status"], &e.env()).stdout).into_owned();
    assert!(out.contains("late"), "{out}");
}

/// R3 repair: with a pre-#49 binary (`HUB_TEST_OLD_BIN`, e.g. a develop
/// 714faf0 release build), every command it has refuses a repository mid-
/// rescue and leaves it byte-identical; after the rescue it works again.
/// Skipped when the variable is unset.
#[test]
fn rescue_marker_stops_old_binaries() {
    let Some(old) = std::env::var_os("HUB_TEST_OLD_BIN") else {
        eprintln!("HUB_TEST_OLD_BIN unset: skipping");
        return;
    };
    let old = PathBuf::from(old);
    let (e, _srv, _a, c) = diverged("res49-old", "file", |_| {});
    let mut env = e.env();
    env.push(("HUB_TEST_CRASH_STAGE", "rescue-ops"));
    assert!(!hub_env(&c, &["reid", "--rescue", "--offline"], &env)
        .status
        .success());
    let before = all_bytes(&c);
    let run_old = |a: &[&str]| {
        Command::new(&old)
            .args(a)
            .current_dir(&c)
            .env("XDG_CONFIG_HOME", cfg_home())
            .env(
                "BAROCSS_HUB_CONFIG_DIR",
                Path::new(cfg_home()).join("barocss-hub"),
            )
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    // Every command that opens the repository is refused, `.hub/` and the
    // working tree byte-identical.
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
        &["mkdir", "d"],
        &["touch", "t"],
        &["mv", "f", "f2"],
        &["rm", "f"],
        &["cat", "head", "f"],
        &["sync"],
        &["reid"],
        &["init"],
        &["serve", "--stdio", "."],
    ];
    for a in cmds {
        let o = run_old(a);
        assert!(
            !o.status.success(),
            "old {a:?} succeeded mid-rescue: {}",
            String::from_utf8_lossy(&o.stdout)
        );
        assert_eq!(all_bytes(&c), before, "old {a:?} changed the repository");
    }
    // Config-only commands of 714faf0 never open the repository, so no
    // marker can stop them (reported to Review): they touch only
    // `.hub/remotes`, the reported-rejections memory and the user's trust
    // store, none of which a rescue in progress reads. Everything else
    // stays byte-identical.
    let cfg_only: &[&[&str]] = &[
        &["remote", "list"],
        &["warnings", "--clear-reported"],
        &["remote", "add", "o2", "file:///nowhere"],
        &["remote", "set-url", "o2", "file:///x"],
        &["remote", "rm", "o2"],
        &["remote", "trust", "origin"],
    ];
    let core = |m: BTreeMap<PathBuf, Vec<u8>>| -> BTreeMap<PathBuf, Vec<u8>> {
        m.into_iter()
            .filter(|(p, _)| {
                let n = p.file_name().unwrap().to_string_lossy();
                !(n == "remotes" || n.starts_with(".remotes") || n.contains("reported-rejections"))
            })
            .collect()
    };
    let before_core = core(before.clone());
    // 714faf0's `watch` keeps running but every step is refused ("auto-
    // record: unsupported repository format"); stopped after 3 s, it has
    // changed nothing but its own `watch.pid`.
    let o = Command::new("timeout")
        .args(["-s", "INT", "3"])
        .arg(&old)
        .args(["watch", "--no-sync"])
        .current_dir(&c)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&o.stderr).contains("unsupported repository format"));
    let nopid = |m: BTreeMap<PathBuf, Vec<u8>>| -> BTreeMap<PathBuf, Vec<u8>> {
        m.into_iter()
            .filter(|(p, _)| !p.ends_with("watch.pid"))
            .collect()
    };
    assert_eq!(
        nopid(all_bytes(&c)),
        nopid(before.clone()),
        "old watch changed the repository"
    );
    for a in cfg_only {
        let _ = run_old(a);
        assert_eq!(
            core(all_bytes(&c)),
            before_core,
            "old {a:?} changed rescue state"
        );
    }
    ok_env(&c, &["reid", "--rescue", "--offline"], &e.env());
    let o = Command::new(&old)
        .args(["status"])
        .current_dir(&c)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = Command::new(&old)
        .args(["record"])
        .current_dir(&c)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}
