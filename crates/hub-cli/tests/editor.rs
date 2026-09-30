//! Issue #37: editor save patterns and built-in default ignores, real
//! binary, temp dirs. A save must be recorded as a content edit of the same
//! node (same NodeId), never delete+create or a rename, and no editor temp
//! file is ever recorded, under both `record` and `watch`.

mod support;

use std::fs;
use std::path::Path;
use std::time::Duration;
use support::*;

/// vim with `backupcopy=no` (+ `writebackup`): probe `4913` for write
/// permission, swap file `.x.swp`, rename x -> x~, write the new x, delete
/// x~. `gap` is the time between the rename and the new write.
fn vim_save(dir: &Path, name: &str, new: &[u8], gap: Duration) {
    let (d, base) = match name.rsplit_once('/') {
        Some((d, b)) => (dir.join(d), b.to_string()),
        None => (dir.to_path_buf(), name.to_string()),
    };
    fs::write(d.join("4913"), b"").unwrap();
    fs::remove_file(d.join("4913")).unwrap();
    fs::write(d.join(format!(".{base}.swp")), b"b0VIM 9.0 swap").unwrap();
    fs::rename(d.join(&base), d.join(format!("{base}~"))).unwrap();
    std::thread::sleep(gap);
    fs::write(d.join(&base), new).unwrap();
    fs::remove_file(d.join(format!("{base}~"))).unwrap();
}

/// VS Code-style atomic write: a temp file next to the target, renamed
/// over it (the temp name is not in the default list: the rename is
/// atomic, so it is only ever seen mid-save).
fn vscode_save(dir: &Path, name: &str, new: &[u8]) {
    let tmp = dir.join(format!("{name}.tmp-5f3a"));
    fs::write(&tmp, new).unwrap();
    fs::rename(&tmp, dir.join(name)).unwrap();
}

/// JetBrains safe write: x___jb_tmp___ gets the new bytes, x -> x___jb_old___,
/// x___jb_tmp___ -> x, delete x___jb_old___.
fn jetbrains_save(dir: &Path, name: &str, new: &[u8], gap: Duration) {
    let (tmp, old) = (
        dir.join(format!("{name}___jb_tmp___")),
        dir.join(format!("{name}___jb_old___")),
    );
    fs::write(&tmp, new).unwrap();
    fs::rename(dir.join(name), &old).unwrap();
    std::thread::sleep(gap);
    fs::rename(&tmp, dir.join(name)).unwrap();
    fs::remove_file(&old).unwrap();
}

fn repo(tag: &str) -> std::path::PathBuf {
    let d = tmp(tag);
    ok(&d, &["init"]);
    fs::create_dir_all(d.join("src")).unwrap();
    fs::write(d.join("x.txt"), b"one\n").unwrap();
    fs::write(d.join("src/y.rs"), b"fn a() {}\n").unwrap();
    ok(&d, &["record"]);
    d
}

/// The only recorded change is a `modified` of `path` on node `node`.
fn assert_one_edit(changes: &[V], path: &str, node: &str) {
    assert_eq!(changes.len(), 1, "{changes:?}");
    let c = &changes[0];
    assert_eq!(c.get("kind").str(), "modified", "{c:?}");
    assert_eq!(c.get("path").str(), path);
    assert_eq!(c.get("node").str(), node);
}

fn assert_no_temps(dir: &Path) {
    for p in nodes(dir).keys() {
        for t in [".swp", "~", "4913", "___jb_", ".tmp-"] {
            assert!(!p.contains(t), "temp recorded: {p}");
        }
    }
}

#[test]
fn save_patterns_under_record_edit_the_same_node() {
    type Save = fn(&Path, &str, &[u8]);
    let saves: [(&str, Save); 3] = [
        ("vim", |d, n, b| vim_save(d, n, b, Duration::ZERO)),
        ("vscode", vscode_save),
        ("jetbrains", |d, n, b| {
            jetbrains_save(d, n, b, Duration::ZERO)
        }),
    ];
    for (editor, save) in saves {
        let d = repo(&format!("rec-{editor}"));
        let before = nodes(&d);
        for (i, path) in ["x.txt", "src/y.rs"].iter().enumerate() {
            let new = format!("{editor} save {i}\n");
            save(&d, path, new.as_bytes());
            let st = js(&d, &["status"]);
            assert_one_edit(st.get("changes").arr(), path, &before[*path]);
            let r = js(&d, &["record"]);
            assert_one_edit(r.get("changes").arr(), path, &before[*path]);
            assert_eq!(fs::read(d.join(path)).unwrap(), new.as_bytes());
        }
        assert_eq!(nodes(&d), before, "{editor}: node ids changed");
        assert_no_temps(&d);
        // The vim swap file is still there, ignored by default.
        if editor == "vim" {
            assert!(d.join(".x.txt.swp").exists());
            assert_eq!(js(&d, &["record"]).get("recorded"), &V::Bool(false));
        }
        // The node's history holds only its create and content edits.
        let h = js(&d, &["log", "--path", "x.txt"]);
        let kinds: Vec<&str> = h
            .get("ops")
            .arr()
            .iter()
            .map(|o| o.get("kind").str())
            .collect();
        assert_eq!(kinds, ["set-content", "set-content", "create"], "{editor}");
    }
}

/// A watcher records each save as one content edit of the same node.
#[test]
fn save_patterns_under_watch_edit_the_same_node() {
    let d = repo("watch");
    let before = nodes(&d);
    let w = Watcher::start(&d, 300, &[]);
    let mut n = 0;
    let mut expect = |w: &Watcher, path: &str| {
        n += 1;
        wait_for(&format!("record #{n} of {path}"), || {
            w.of("recorded").len() >= n
        });
        let ev = &w.of("recorded")[n - 1];
        assert_one_edit(ev.get("changes").arr(), path, &before[path]);
    };
    vim_save(&d, "x.txt", b"vim\n", Duration::ZERO);
    expect(&w, "x.txt");
    vscode_save(&d, "x.txt", b"vscode\n");
    expect(&w, "x.txt");
    jetbrains_save(&d, "x.txt", b"jetbrains\n", Duration::ZERO);
    expect(&w, "x.txt");
    // Slow saves: the target is absent for longer than the debounce. The
    // watcher defers while the editor's sidecar of that file exists.
    vim_save(&d, "src/y.rs", b"vim slow\n", Duration::from_millis(1500));
    expect(&w, "src/y.rs");
    jetbrains_save(&d, "x.txt", b"jb slow\n", Duration::from_millis(1500));
    expect(&w, "x.txt");
    let evs = w.stop();
    assert!(
        evs.iter().any(|e| e.get("event").str() == "deferred"),
        "slow saves were deferred"
    );
    assert_eq!(evs.last().unwrap().get("event").str(), "stopped");
    assert_eq!(nodes(&d), before);
    assert_no_temps(&d);
    assert_eq!(fs::read(d.join("x.txt")).unwrap(), b"jb slow\n");
}

/// Mutation check of the deferral: with it disabled
/// (`HUB_WATCH_SAVE_WAIT_MS=0`) a slow vim save is recorded as a delete and
/// a create of a *new* node, which is exactly what the guard prevents.
#[test]
fn slow_save_without_deferral_is_delete_plus_create() {
    let d = repo("nodefer");
    let before = nodes(&d);
    let w = Watcher::start(&d, 200, &[("HUB_WATCH_SAVE_WAIT_MS", "0")]);
    vim_save(&d, "x.txt", b"new\n", Duration::from_millis(1500));
    wait_for("delete and re-create", || {
        w.of("recorded").iter().any(|e| {
            e.get("changes")
                .arr()
                .iter()
                .any(|c| c.get("kind").str() == "added")
        })
    });
    let evs = w.stop();
    let kinds: Vec<&str> = evs
        .iter()
        .filter(|e| e.get("event").str() == "recorded")
        .flat_map(|e| e.get("changes").arr().iter().map(|c| c.get("kind").str()))
        .collect();
    assert_eq!(kinds, ["deleted", "added"]);
    assert_ne!(nodes(&d)["x.txt"], before["x.txt"]);
}

/// A real delete while a backup lingers is still recorded, after the
/// deferral limit.
#[test]
fn delete_with_lingering_backup_is_recorded_after_the_limit() {
    let d = repo("linger");
    fs::write(d.join("x.txt~"), b"old backup").unwrap();
    let w = Watcher::start(&d, 200, &[("HUB_WATCH_SAVE_WAIT_MS", "800")]);
    fs::remove_file(d.join("x.txt")).unwrap();
    wait_for("delete recorded", || !w.of("recorded").is_empty());
    let ev = &w.of("recorded")[0];
    let c = &ev.get("changes").arr()[0];
    assert_eq!(c.get("kind").str(), "deleted");
    assert_eq!(c.get("path").str(), "x.txt");
    let evs = w.stop();
    assert!(evs.iter().any(|e| e.get("event").str() == "deferred"));
    assert!(!nodes(&d).contains_key("x.txt"));
    assert!(!nodes(&d).contains_key("x.txt~"));
}

const DEFAULT_SAMPLES: &[&str] = &[
    ".x.txt.swp",
    ".x.txt.swo",
    ".x.txt.swn",
    ".x.txt.swx",
    "4913",
    "x.txt~",
    ".#x.txt",
    "#x.txt#",
    "x.txt___jb_tmp___",
    "x.txt___jb_old___",
    ".~lock.doc.odt#",
    ".goutputstream-XY12ZW",
    ".DS_Store",
    "Thumbs.db",
    "desktop.ini",
];

/// Every default ignore, at the root and nested, with no `.hubignore`:
/// never recorded, listed by `status --json` as ignored.
#[test]
fn default_ignores_are_never_recorded() {
    assert_eq!(DEFAULT_SAMPLES.len(), hub_cli::ignore::DEFAULTS.len());
    let d = tmp("defaults");
    ok(&d, &["init"]);
    fs::create_dir_all(d.join("sub/deep")).unwrap();
    fs::write(d.join("real.txt"), b"r").unwrap();
    for s in DEFAULT_SAMPLES {
        fs::write(d.join(s), b"t").unwrap();
        fs::write(d.join("sub/deep").join(s), b"t").unwrap();
    }
    let st = js(&d, &["status"]);
    let ignored: Vec<&str> = st
        .get("ignored")
        .arr()
        .iter()
        .map(|e| e.get("path").str())
        .collect();
    for s in DEFAULT_SAMPLES {
        assert!(ignored.contains(s), "{s} not ignored: {ignored:?}");
        let nested = format!("sub/deep/{s}");
        assert!(ignored.contains(&nested.as_str()), "{nested}");
    }
    let r = js(&d, &["record"]);
    let paths: Vec<&str> = r
        .get("changes")
        .arr()
        .iter()
        .map(|c| c.get("path").str())
        .collect();
    assert_eq!(paths, ["real.txt", "sub", "sub/deep"]);
    assert_eq!(
        nodes(&d).keys().collect::<Vec<_>>(),
        ["real.txt", "sub", "sub/deep"]
    );
}

/// `!` in `.hubignore` re-includes a default; a file tracked while
/// re-included stays tracked after the rule goes away (#24).
#[test]
fn defaults_re_include_and_tracked_stays_tracked() {
    let d = tmp("reinclude");
    ok(&d, &["init"]);
    fs::write(d.join(".hubignore"), "!*~\n!4913\n").unwrap();
    fs::write(d.join("notes~"), b"1").unwrap();
    fs::write(d.join("4913"), b"probe").unwrap();
    fs::write(d.join("a.swp"), b"s").unwrap();
    ok(&d, &["record"]);
    let n = nodes(&d);
    assert!(n.contains_key("notes~") && n.contains_key("4913"));
    assert!(!n.contains_key("a.swp"));
    fs::remove_file(d.join(".hubignore")).unwrap();
    fs::write(d.join("notes~"), b"2").unwrap();
    let st = js(&d, &["status"]);
    let tbi: Vec<&str> = st
        .get("tracked_but_ignored")
        .arr()
        .iter()
        .map(|v| v.str())
        .collect();
    assert_eq!(tbi, ["4913", "notes~"]);
    let r = js(&d, &["record"]);
    let ch = r.get("changes").arr();
    let kinds: Vec<(&str, &str)> = ch
        .iter()
        .map(|c| (c.get("kind").str(), c.get("path").str()))
        .collect();
    assert_eq!(kinds, [("modified", "notes~"), ("deleted", ".hubignore")]);
    assert_eq!(nodes(&d)["notes~"], n["notes~"]);
}
