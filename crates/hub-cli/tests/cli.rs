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
