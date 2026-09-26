//! Issue #19 property: for random file trees, record -> wipe -> checkout
//! reproduces identical paths and bytes; so does checkout of an older
//! checkpoint after random edits / renames / deletes. Temp dirs only.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;

type Snap = BTreeMap<String, Option<Vec<u8>>>;

fn tmp() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "hub-wd-prop-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn hub(d: &Path, args: &[&str]) -> Result<String, String> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (mut out, mut warn) = (Vec::new(), Vec::new());
    hub_cli::run(&args, d, &mut out, &mut warn).map_err(|e| e.0)?;
    Ok(String::from_utf8(out).unwrap())
}

fn snapshot(root: &Path) -> Snap {
    fn walk(dir: &Path, rel: &str, out: &mut Snap) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            if rel.is_empty() && name == ".hub" {
                continue;
            }
            let p = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if e.file_type().unwrap().is_dir() {
                out.insert(p.clone(), None);
                walk(&e.path(), &p, out);
            } else {
                out.insert(p, Some(fs::read(e.path()).unwrap()));
            }
        }
    }
    let mut m = Snap::new();
    walk(root, "", &mut m);
    m
}

fn wipe(root: &Path) {
    for e in fs::read_dir(root).unwrap() {
        let e = e.unwrap();
        if e.file_name() == ".hub" {
            continue;
        }
        if e.file_type().unwrap().is_dir() {
            fs::remove_dir_all(e.path()).unwrap();
        } else {
            fs::remove_file(e.path()).unwrap();
        }
    }
}

/// Lowercase-only names: no case clashes on case-insensitive filesystems.
fn name() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "c", "d e", "x.txt", "%y", ".hidden", "ü"])
        .prop_map(str::to_string)
}

#[derive(Clone, Debug)]
enum Item {
    Dir(Vec<String>),
    File(Vec<String>, Vec<u8>),
}

fn item() -> impl Strategy<Value = Item> {
    let path = || prop::collection::vec(name(), 1..4);
    prop_oneof![
        path().prop_map(Item::Dir),
        (path(), prop::collection::vec(any::<u8>(), 0..64)).prop_map(|(p, b)| Item::File(p, b)),
    ]
}

#[derive(Clone, Debug)]
enum Mutation {
    Edit(usize, Vec<u8>),
    Rename(usize, String),
    Delete(usize),
    Add(Vec<String>, Vec<u8>),
}

fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        (any::<usize>(), prop::collection::vec(any::<u8>(), 0..32))
            .prop_map(|(i, b)| Mutation::Edit(i, b)),
        (any::<usize>(), name()).prop_map(|(i, n)| Mutation::Rename(i, n)),
        any::<usize>().prop_map(Mutation::Delete),
        (
            prop::collection::vec(name(), 1..3),
            prop::collection::vec(any::<u8>(), 0..32)
        )
            .prop_map(|(p, b)| Mutation::Add(p, b)),
    ]
}

/// Create `items` on disk, skipping any that clash with an existing entry.
fn build(root: &Path, items: &[Item]) {
    for it in items {
        let (parts, bytes) = match it {
            Item::Dir(p) => (p, None),
            Item::File(p, b) => (p, Some(b)),
        };
        let fp = parts.iter().fold(root.to_path_buf(), |a, n| a.join(n));
        // Every ancestor must be a dir (or absent), the target absent.
        let mut ok = !fp.exists();
        let mut cur = root.to_path_buf();
        for n in &parts[..parts.len() - 1] {
            cur = cur.join(n);
            if cur.is_file() {
                ok = false;
            }
        }
        if !ok {
            continue;
        }
        match bytes {
            None => fs::create_dir_all(&fp).unwrap(),
            Some(b) => {
                fs::create_dir_all(fp.parent().unwrap()).unwrap();
                fs::write(&fp, b).unwrap();
            }
        }
    }
}

fn mutate(root: &Path, ms: &[Mutation]) {
    for m in ms {
        let snap = snapshot(root);
        let files: Vec<&String> = snap
            .iter()
            .filter(|(_, v)| v.is_some())
            .map(|(k, _)| k)
            .collect();
        let all: Vec<&String> = snap.keys().collect();
        match m {
            Mutation::Edit(i, b) if !files.is_empty() => {
                fs::write(root.join(files[i % files.len()]), b).unwrap();
            }
            Mutation::Rename(i, n) if !all.is_empty() => {
                let from = root.join(all[i % all.len()]);
                let to = from.parent().unwrap().join(n);
                if !to.exists() {
                    fs::rename(from, to).unwrap();
                }
            }
            Mutation::Delete(i) if !all.is_empty() => {
                let p = root.join(all[i % all.len()]);
                if p.is_dir() {
                    fs::remove_dir_all(p).unwrap();
                } else if p.exists() {
                    fs::remove_file(p).unwrap();
                }
            }
            Mutation::Add(p, b) => build(root, &[Item::File(p.clone(), b.clone())]),
            _ => {}
        }
    }
}

fn short(record_out: &str) -> Option<String> {
    let l = record_out.lines().last()?;
    Some(l.strip_prefix('[')?.split(']').next()?.to_string())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    #[test]
    fn record_wipe_checkout_round_trips(
        items in prop::collection::vec(item(), 0..12),
        muts in prop::collection::vec(mutation(), 0..8),
    ) {
        let d = tmp();
        hub(&d, &["init"]).unwrap();
        build(&d, &items);
        let snap1 = snapshot(&d);
        let r1 = hub(&d, &["record", "-m", "v1"]).unwrap();
        prop_assert!(hub(&d, &["status"]).unwrap().contains("working directory: clean"));

        wipe(&d);
        hub(&d, &["checkout"]).unwrap();
        prop_assert_eq!(snapshot(&d), snap1.clone());

        mutate(&d, &muts);
        let snap2 = snapshot(&d);
        hub(&d, &["record", "-m", "v2"]).unwrap();
        prop_assert!(hub(&d, &["status"]).unwrap().contains("working directory: clean"));
        wipe(&d);
        hub(&d, &["checkout"]).unwrap();
        prop_assert_eq!(snapshot(&d), snap2.clone());

        if let Some(c1) = short(&r1) {
            hub(&d, &["checkout", &c1]).unwrap();
            prop_assert_eq!(snapshot(&d), snap1);
            hub(&d, &["checkout"]).unwrap();
            prop_assert_eq!(snapshot(&d), snap2);
        }
        let _ = fs::remove_dir_all(&d);
    }
}
