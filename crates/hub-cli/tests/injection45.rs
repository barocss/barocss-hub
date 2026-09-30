//! Issue #45: adversarial command-injection tests for everything that
//! reaches ssh or the remote shell (exec, user, host, port, path), at
//! `remote add`, at `remote set-url`, when loading a planted
//! `.hub/remotes`, and for a URL typed on the `sync` command line.
//!
//! The fake ssh (`HUB_SSH_COMMAND`) logs its argv and then *evaluates the
//! remote command with `sh -c`*, exactly as sshd hands it to the login
//! shell, so any injection would run and create the marker file. A
//! positive control proves the harness does execute an unquoted command.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_barocss-hub");

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "hub45i-{tag}-{}-{}",
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
    marker: PathBuf,
    xdg: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Env {
        let t = tmp(tag);
        let ssh = t.join("fake-ssh");
        fs::write(
            &ssh,
            r#"#!/bin/sh
# Log the whole argv, one arg per line, then run the command like sshd.
for a in "$@"; do printf 'ARG %s\n' "$a" >> "$FAKE_SSH_LOG"; done
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift
host="$1"; cmd="$2"
printf 'CMD %s\n' "$cmd" >> "$FAKE_SSH_LOG"
exec sh -c "$cmd"
"#,
        )
        .unwrap();
        Command::new("chmod").arg("+x").arg(&ssh).status().unwrap();
        let xdg = t.join("xdg");
        fs::create_dir_all(&xdg).unwrap();
        Env {
            ssh: ssh.to_string_lossy().into_owned(),
            log: t.join("ssh.log"),
            marker: t.join("PWNED"),
            xdg,
            t,
        }
    }
    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .current_dir(dir)
            .env("HUB_SSH_COMMAND", &self.ssh)
            .env("FAKE_SSH_LOG", &self.log)
            .env("XDG_CONFIG_HOME", &self.xdg)
            .env_remove("BAROCSS_HUB_SERVE_ROOT")
            .env("HUB_TEST_WIRE_TIMEOUT_MS", "3000")
            .stdin(Stdio::null())
            .output()
            .unwrap()
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
    fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
    fn clean(&self) {
        assert!(
            !self.marker.exists(),
            "INJECTION: the marker was created; ssh log:\n{}",
            self.log_text()
        );
    }
}

/// Every shell metacharacter (and friends) the issue lists, plus a few.
const META: &[&str] = &[
    ";", "&", "|", "`", "$", "(", ")", "<", ">", "\\", "\"", "'", " ", "\t", "\n", "*", "?", "[",
    "]", "{", "}", "!", "#", "~", "%", "^", "=", ",", "\r",
];

fn plant(repo: &Path, line: &str) {
    fs::write(
        repo.join(".hub/remotes"),
        format!("barocss-hub/remotes/v0\n{line}\n"),
    )
    .unwrap();
}

fn esc(s: &str) -> String {
    let mut o = String::from("s");
    for c in s.chars() {
        if c == '%' || (c as u32) <= 0x20 || c as u32 == 0x7f {
            o.push_str(&format!("%{:02x}", c as u32));
        } else {
            o.push(c);
        }
    }
    o
}

#[test]
fn harness_positive_control_executes_unquoted_commands() {
    let e = Env::new("control");
    let o = Command::new(&e.ssh)
        .args([
            "-o",
            "x",
            "--",
            "h",
            &format!("true;touch {}", e.marker.display()),
        ])
        .env("FAKE_SSH_LOG", &e.log)
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(
        e.marker.exists(),
        "the fake ssh must evaluate the command with sh -c"
    );
}

/// Payloads that would run `touch <marker>` if exec reached the shell.
fn exec_payloads(m: &Path) -> Vec<String> {
    let m = m.display();
    let mut v = vec![
        format!("{BIN};touch {m}"),
        format!("{BIN};touch${{IFS}}{m}"),
        format!("{BIN}&&touch {m}"),
        format!("{BIN}&touch${{IFS}}{m}"),
        format!("{BIN}|touch {m}"),
        format!("{BIN}||touch${{IFS}}{m}"),
        format!("`touch {m}`"),
        format!("`touch${{IFS}}{m}`{BIN}"),
        format!("$(touch {m})"),
        format!("{BIN}$(touch${{IFS}}{m})"),
        format!("{BIN}\ntouch {m}"),
        format!("{BIN}\rtouch {m}"),
        format!("{BIN}>{m}"),
        format!("{BIN}<{m}"),
        format!("{BIN}#"),
        format!("'{BIN}'"),
        format!("\"{BIN}\""),
        format!("{BIN}\\"),
        format!("{BIN}\ttouch {m}"),
        "-oProxyCommand=touch".to_string(),
        "-".to_string(),
        String::new(),
        format!("{BIN}{}", "x".repeat(1100)),
    ];
    for c in META.iter().filter(|c| **c != "~") {
        v.push(format!("{c}{BIN}"));
        v.push(format!("{BIN}{c}"));
        v.push(format!("/usr{c}bin/barocss-hub"));
        v.push(format!("{BIN}{c}touch{c}{m}"));
    }
    v
}

#[test]
fn exec_injection_refused_at_add() {
    let e = Env::new("exec-add");
    let a = e.repo("a");
    let url = format!("ssh://localhost{}/srv", e.t.display());
    let before = fs::read(a.join(".hub/remotes")).ok();
    for p in exec_payloads(&e.marker) {
        let o = e.run(&a, &["remote", "add", "--exec", &p, "o", &url, "--json"]);
        assert_eq!(o.status.code(), Some(1), "{p:?} accepted");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("\"code\":\"usage\""), "{p:?}: {err}");
        assert!(err.contains("bad --exec"), "{p:?}: {err}");
        assert_eq!(fs::read(a.join(".hub/remotes")).ok(), before, "{p:?}");
        e.clean();
    }
    assert!(e.log_text().is_empty(), "ssh ran: {}", e.log_text());
    // `~` is allowed (home-relative exec) and inert in any position.
    for ok in ["~/bin/barocss-hub", "a~b", "/opt/x+y/barocss-hub-1.2_3"] {
        e.ok(&a, &["remote", "add", "--exec", ok, "t", &url]);
        e.ok(&a, &["remote", "rm", "t"]);
    }
}

#[test]
fn exec_injection_refused_at_load() {
    let e = Env::new("exec-load");
    let a = e.repo("a");
    let url = format!("ssh://localhost{}/srv", e.t.display());
    let allowed = |t: &str| {
        !t.is_empty()
            && !t.starts_with('-')
            && t.len() <= 1024
            && t.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._/~+-".contains(c))
    };
    for p in exec_payloads(&e.marker) {
        // What the file parser sees as the exec token: up to the first
        // space (the rest become ignored tokens) or line end.
        let token = p.split([' ', '\n']).next().unwrap();
        if allowed(token) && !p.contains('\n') {
            // e.g. "BIN touch /m": exec is exactly BIN, the rest is inert.
            continue;
        }
        // Raw, as a hand edit or a planted file would carry it (a space
        // splits the token, a newline the line: both must still refuse).
        plant(&a, &format!("R sorigin {} exec={p} default=1", esc(&url)));
        for args in [
            &["sync", "--yes"][..],
            &["sync", "--yes", "origin"],
            &["sync", "--yes", "--json"],
            &["remote", "list"],
            &["watch", "--poll", "50"],
        ] {
            let o = e.run(&a, args);
            assert!(!o.status.success(), "{p:?} {args:?} accepted");
            let err = String::from_utf8_lossy(&o.stderr);
            assert!(
                err.contains("unsafe") || err.contains("corrupt"),
                "{p:?} {args:?}: {err}"
            );
            e.clean();
        }
        let o = e.run(&a, &["sync", "--yes", "--json"]);
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("\"code\":\"corrupt\""), "{p:?}: {err}");
        // The unsafe line can be removed (a newline payload instead made a
        // second, unparsable line: the whole file is corrupt, never run).
        let o = e.run(&a, &["remote", "rm", "origin"]);
        assert_eq!(o.status.success(), !p.contains('\n'), "{p:?}");
        e.clean();
    }
    assert!(e.log_text().is_empty(), "ssh ran: {}", e.log_text());
}

/// URLs whose user/host/port carry a metacharacter: refused before ssh.
fn bad_authority_urls(c: &str) -> Vec<String> {
    vec![
        format!("ssh://h{c}x/srv/p"),
        format!("ssh://{c}h/srv/p"),
        format!("ssh://h{c}/srv/p"),
        format!("ssh://u{c}x@h/srv/p"),
        format!("ssh://{c}u@h/srv/p"),
        format!("ssh://h:22{c}/srv/p"),
        format!("h{c}x:/srv/p"),
        format!("u{c}x@h:/srv/p"),
    ]
}

/// URLs whose path carries a metacharacter and an injection attempt: either
/// refused, or sent single-quoted so the shell never runs it.
fn path_urls(c: &str, m: &Path) -> Vec<String> {
    let m = m.display();
    vec![
        format!("ssh://h/srv/p{c}touch {m}"),
        format!("ssh://h/srv/p{c}touch${{IFS}}{m}{c}"),
        format!("h:/srv/p{c}touch {m}"),
        format!("h:/srv/p'{c}touch {m}{c}'"),
        format!("h:/srv/p'\\''{c}touch {m};'"),
        format!("h:{c}touch {m}"),
        format!("ssh://h/~/{c}touch {m}"),
    ]
}

fn check_ssh_log(e: &Env) {
    // Every command that reached the remote shell is exactly
    // `<exec> serve --stdio '<quoted path>'`.
    for l in e.log_text().lines().filter(|l| l.starts_with("CMD ")) {
        let c = &l[4..];
        assert!(
            (c.starts_with(&format!("{BIN} serve --stdio '"))
                || c.starts_with("barocss-hub serve --stdio '"))
                && c.ends_with('\''),
            "unexpected remote command {c:?}"
        );
    }
    // No ssh option was ever injected (`-o` only with BatchMode, host
    // after `--`).
    let log = e.log_text();
    let args: Vec<&str> = log.lines().filter_map(|l| l.strip_prefix("ARG ")).collect();
    for w in args.windows(2) {
        if w[0] == "-o" {
            assert!(w[1].starts_with("BatchMode="), "{:?}", w[1]);
        }
    }
}

#[test]
fn url_injection_at_add_set_url_load_and_command_line() {
    let e = Env::new("url");
    let a = e.repo("a");
    let good = format!("ssh://localhost{}/srv", e.t.display());
    for c in META {
        for url in bad_authority_urls(c) {
            let before = e.log_text();
            // add
            let o = e.run(&a, &["remote", "add", "--exec", BIN, "o", &url]);
            if o.status.success() {
                // Only a form where `c` ended up inside the path parses.
                e.run(&a, &["sync", "--yes", "o"]);
                e.ok(&a, &["remote", "rm", "o"]);
            }
            // set-url
            e.ok(&a, &["remote", "add", "--exec", BIN, "base", &good]);
            if e.run(&a, &["remote", "set-url", "base", &url])
                .status
                .success()
            {
                e.run(&a, &["sync", "--yes", "base"]);
            }
            e.ok(&a, &["remote", "rm", "base"]);
            // load
            plant(&a, &format!("R sorigin {} exec={BIN}", esc(&url)));
            e.run(&a, &["sync", "--yes", "origin"]);
            fs::remove_file(a.join(".hub/remotes")).unwrap();
            // command line
            e.run(&a, &["sync", &url]);
            e.clean();
            // A metacharacter in user, host or port: ssh never even runs.
            let after = e.log_text();
            assert_eq!(after, before, "{url:?} reached ssh");
        }
        for url in path_urls(c, &e.marker) {
            let o = e.run(&a, &["remote", "add", "--exec", BIN, "o", &url]);
            if o.status.success() {
                e.run(&a, &["sync", "--yes", "o"]);
                e.ok(&a, &["remote", "rm", "o"]);
            }
            e.ok(&a, &["remote", "add", "--exec", BIN, "base", &good]);
            if e.run(&a, &["remote", "set-url", "base", &url])
                .status
                .success()
            {
                e.run(&a, &["sync", "--yes", "base"]);
            }
            e.ok(&a, &["remote", "rm", "base"]);
            plant(&a, &format!("R sorigin {} exec={BIN}", esc(&url)));
            e.run(&a, &["sync", "--yes", "origin"]);
            fs::remove_file(a.join(".hub/remotes")).unwrap();
            e.run(&a, &["sync", &url]);
            e.clean();
        }
    }
    check_ssh_log(&e);
    // Some path forms are legitimately sent (quoted): the harness did run.
    assert!(e.log_text().contains("CMD "), "no ssh run at all?");
}

#[test]
fn option_injection_is_refused() {
    let e = Env::new("opt");
    let a = e.repo("a");
    for url in [
        "ssh://-oProxyCommand=touch/p",
        "-oProxyCommand=touch:/p",
        "ssh://-l@h/p",
        "ssh://u@-h/p",
        "h:~root/p",
        "ssh://.h/p",
    ] {
        assert!(!e.run(&a, &["sync", url]).status.success(), "{url}");
        let o = e.run(&a, &["remote", "add", "o", url]);
        assert!(!o.status.success(), "{url}");
    }
    e.clean();
    // A path can never be read as an option of `serve`: relative scp paths
    // become `~/…`, ssh:// paths are absolute.
    for url in [
        "h:-p",
        "h:--root=/",
        "h:--from-ssh-original-command",
        "ssh://h/-p",
    ] {
        assert!(!e.run(&a, &["sync", url]).status.success(), "{url}");
    }
    e.clean();
    let log = e.log_text();
    for l in log.lines().filter_map(|l| l.strip_prefix("CMD ")) {
        assert!(
            l.contains(" serve --stdio '/") || l.contains(" serve --stdio '~/"),
            "{l}"
        );
    }
}
