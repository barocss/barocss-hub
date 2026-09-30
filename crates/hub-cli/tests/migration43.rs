//! Issue #43 (D-041, P23): the `.hub/peers` → `.hub/remotes` migration
//! under the #33 bar: kill -9 (abort) at every step, idempotent reruns,
//! and the pre-#43 binary (`HUB_PRE43_BIN`, develop a28c722) on a migrated
//! repository.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use hub_cli::remote::{RMIG_PEERS_REMOVED, RMIG_RENAMED, RMIG_TMP};

const BIN: &str = env!("CARGO_BIN_EXE_barocss-hub");

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "hub43m-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    fs::canonicalize(p).unwrap()
}

fn run(bin: &str, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(bin);
    c.env("XDG_CONFIG_HOME", xdg());
    c.args(args).current_dir(dir);
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let o = run(BIN, dir, args, &[]);
    assert!(
        o.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

fn hub_files(d: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut m = BTreeMap::new();
    for e in fs::read_dir(d.join(".hub")).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_file() {
            m.insert(
                e.file_name().into_string().unwrap(),
                fs::read(e.path()).unwrap(),
            );
        }
    }
    m
}

/// A repo with a v1 `peers` file naming two peer repos (one whose
/// basename collides with an existing remote), plus an existing remote.
fn setup(tag: &str) -> (PathBuf, Vec<PathBuf>) {
    let t = tmp(tag);
    let d = t.join("repo");
    fs::create_dir_all(&d).unwrap();
    ok(&d, &["init"]);
    fs::write(d.join("f"), "f\n").unwrap();
    ok(&d, &["record"]);
    let peers: Vec<PathBuf> = ["p1", "origin"]
        .iter()
        .map(|n| {
            let p = t.join(n);
            fs::create_dir_all(&p).unwrap();
            ok(&p, &["init"]);
            p
        })
        .collect();
    let other = t.join("other");
    fs::create_dir_all(&other).unwrap();
    ok(&other, &["init"]);
    ok(&d, &["remote", "add", "origin", other.to_str().unwrap()]);
    let mut s = String::from("barocss-hub/peers/v1\n");
    for p in &peers {
        s.push_str(&format!("{}\n", p.display()));
    }
    fs::write(d.join(".hub/peers"), s).unwrap();
    (d, peers)
}

fn check_migrated(d: &Path, peers: &[PathBuf]) {
    assert!(!d.join(".hub/peers").exists(), "peers left behind");
    let l = ok(d, &["remote", "list"]);
    for p in peers {
        let url = format!("file://{}", p.display());
        assert_eq!(l.matches(&format!("\t{url}")).count(), 1, "{l}");
    }
    assert_eq!(l.lines().count(), 3, "{l}");
    assert!(l.contains("p1\t"), "{l}");
    assert!(l.contains("local-2\t"), "{l}");
    assert!(l.contains("origin\t") && l.contains("(default)"), "{l}");
}

#[test]
fn p23_crash_at_every_migration_step_then_rerun_is_exact_and_idempotent() {
    for stage in [RMIG_TMP, RMIG_RENAMED, RMIG_PEERS_REMOVED] {
        let (d, peers) = setup(stage);
        let before = hub_files(&d);
        let o = run(BIN, &d, &["record"], &[("HUB_TEST_CRASH_STAGE", stage)]);
        assert!(!o.status.success(), "{stage}: crash hook did not fire");
        let after = hub_files(&d);
        match stage {
            // Before the rename: `peers` and `remotes` are as they were.
            s if s == RMIG_TMP => {
                assert_eq!(after.get("peers"), before.get("peers"));
                assert_eq!(after.get("remotes"), before.get("remotes"));
            }
            // After the rename: `peers` still there, merged idempotently.
            s if s == RMIG_RENAMED => assert!(after.contains_key("peers")),
            _ => assert!(!after.contains_key("peers")),
        }
        // A shared-lock reader sees the same remotes before the rerun.
        let l = ok(&d, &["remote", "list"]);
        assert_eq!(l.lines().count(), 3, "{stage}: {l}");
        ok(&d, &["record"]);
        check_migrated(&d, &peers);
        // Idempotent: another exclusive command changes nothing.
        let once = hub_files(&d);
        ok(&d, &["record"]);
        ok(&d, &["remote", "list"]);
        assert_eq!(hub_files(&d).get("remotes"), once.get("remotes"));
        // The migrated file:// remotes work once confirmed (#45 repair:
        // the migration grants no trust).
        assert!(!run(BIN, &d, &["sync", "p1"], &[]).status.success());
        ok(&d, &["sync", "--yes", "p1"]);
        ok(&d, &["sync", "p1"]);
    }
}

#[test]
fn p23_v0_peers_header_also_migrates() {
    let (d, peers) = setup("v0hdr");
    let s = fs::read_to_string(d.join(".hub/peers")).unwrap();
    fs::write(d.join(".hub/peers"), s.replace("peers/v1", "peers/v0")).unwrap();
    ok(&d, &["record"]);
    check_migrated(&d, &peers);
}

/// `HUB_PRE43_BIN` = a develop (a28c722) build. On a migrated repository
/// the old binary must refuse or cope without losing anything; afterwards
/// the new binary still opens it and a rerun is idempotent.
#[test]
fn pre43_binary_on_a_migrated_repository() {
    let Some(old) = std::env::var_os("HUB_PRE43_BIN") else {
        eprintln!("HUB_PRE43_BIN not set; skipping the pre-#43 binary check");
        return;
    };
    let old = old.to_string_lossy().into_owned();
    let (d, peers) = setup("pre43");
    ok(&d, &["record"]);
    check_migrated(&d, &peers);
    let remotes = fs::read(d.join(".hub/remotes")).unwrap();
    let ops = fs::read(d.join(".hub/ops.log")).unwrap();
    let mut report = Vec::new();
    for c in [
        &["status"][..],
        &["log"],
        &["ls"],
        &["warnings"],
        &["peer", "list"],
        &["record"],
        &["sync", peers[0].to_str().unwrap()],
    ] {
        let o = run(&old, &d, c, &[]);
        report.push(format!(
            "{c:?} -> {:?} {}",
            o.status.code(),
            String::from_utf8_lossy(&o.stderr).trim()
        ));
        assert_eq!(
            fs::read(d.join(".hub/remotes")).unwrap(),
            remotes,
            "{c:?} touched remotes"
        );
        let now = fs::read(d.join(".hub/ops.log")).unwrap();
        assert!(now.starts_with(&ops), "{c:?} lost ops");
    }
    eprintln!("pre-#43 binary on a migrated repo:\n{}", report.join("\n"));
    // The old binary's format-v1 roll-forward recreates an empty v1
    // `peers`; the new binary's next exclusive open removes it again.
    let recreated = d.join(".hub/peers").exists();
    eprintln!("pre-#43 binary recreated .hub/peers: {recreated}");
    ok(&d, &["record"]);
    check_migrated(&d, &peers);
    ok(&d, &["status"]);
}

/// After migration, a pre-#43 binary's `peer add X` writes a non-empty v1
/// `.hub/peers` (header + canonical roots, exactly as `write_peers` did).
/// The new binary merges it: nothing lost, a URL already present is not
/// duplicated, a basename that collides with an existing remote name
/// becomes `local-<n>` (n = its 1-based line in `peers`, bumped past used
/// names); a valid unused basename is kept as the name.
#[test]
fn peers_written_after_migration_are_merged_not_lost() {
    let (d, peers) = setup("readd");
    ok(&d, &["record"]);
    check_migrated(&d, &peers);
    let t = d.parent().unwrap();
    let fresh = t.join("fresh");
    let clash = t.join("x").join("p1"); // basename `p1` is already a remote
    for p in [&fresh, &clash] {
        fs::create_dir_all(p).unwrap();
        ok(p, &["init"]);
    }
    let clash = fs::canonicalize(clash).unwrap();
    fs::write(
        d.join(".hub/peers"),
        format!(
            "barocss-hub/peers/v1\n{}\n{}\n{}\n",
            peers[0].display(), // already a remote (p1): deduplicated by URL
            fresh.display(),
            clash.display()
        ),
    )
    .unwrap();
    ok(&d, &["record"]);
    assert!(!d.join(".hub/peers").exists());
    let l = ok(&d, &["remote", "list"]);
    assert_eq!(l.lines().count(), 5, "{l}");
    assert_eq!(
        l.matches(&format!("\tfile://{}", peers[0].display()))
            .count(),
        1,
        "{l}"
    );
    assert!(
        l.contains(&format!("fresh\tfile://{}", fresh.display())),
        "{l}"
    );
    assert!(
        l.contains(&format!("local-3\tfile://{}", clash.display())),
        "{l}"
    );
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
